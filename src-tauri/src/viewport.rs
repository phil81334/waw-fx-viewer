//! Windows child surface and playback ownership. All HWND and D3D9 work runs on
//! Tauri's UI thread. CPU preparation and PNG encoding run on background workers.

use crate::{
    fx::{
        prepare::PreparedClip,
        renderer::{background::Preset, Renderer},
        simulation::StreamFrame,
    },
    playback::Playback,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{ValidateRect, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;

type Result<T> = std::result::Result<T, String>;
const RESOLUTION: u32 = 512;
const THUMBNAIL_SIZE: u32 = 192;

#[derive(Clone, Deserialize)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub visible: bool,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
    static INPUT: RefCell<Input> = RefCell::new(Input::default());
    static BACKGROUND: Cell<Preset> = const { Cell::new(Preset::Dark) };
}

struct Input {
    dragging: bool,
    last: [i32; 2],
    pan: [f32; 2],
    zoom: f32,
    side: f32,
    revision: u64,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            dragging: false,
            last: [0; 2],
            pan: [0.0; 2],
            zoom: 0.0,
            side: 512.0,
            revision: 0,
        }
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let point = [
        lparam.0 as u16 as i16 as i32,
        (lparam.0 >> 16) as u16 as i16 as i32,
    ];
    match message {
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            // Re-present paused effects after exposure without borrowing HOST
            // from a window callback that may be re-entered by Direct3D.
            INPUT.with(|input| input.borrow_mut().revision += 1);
            let _ = ValidateRect(Some(window), None);
            return LRESULT(0);
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            INPUT.with(|input| {
                let mut input = input.borrow_mut();
                input.dragging = true;
                input.last = point;
            });
            SetCapture(window);
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_CAPTURECHANGED => {
            INPUT.with(|input| input.borrow_mut().dragging = false);
            if message != WM_CAPTURECHANGED {
                let _ = ReleaseCapture();
            }
        }
        WM_MOUSEMOVE => {
            INPUT.with(|input| {
                let mut input = input.borrow_mut();
                if input.dragging {
                    let side = input.side.max(1.0);
                    input.pan[0] += (point[0] - input.last[0]) as f32 * 2.0 / side;
                    input.pan[1] -= (point[1] - input.last[1]) as f32 * 2.0 / side;
                    input.revision += 1;
                }
                input.last = point;
            });
        }
        WM_MOUSEWHEEL => {
            let delta = (wparam.0 >> 16) as u16 as i16 as f32 / 120.0;
            INPUT.with(|input| {
                let mut input = input.borrow_mut();
                input.zoom = (input.zoom + delta * 0.12).clamp(-2.0, 2.0);
                input.revision += 1;
            });
        }
        _ => {}
    }
    DefWindowProcW(window, message, wparam, lparam)
}

/// These styles are essential: without sibling clipping, WebView2 can paint
/// over a correctly created native child. We only change our own window tree.
unsafe fn create_child(parent: HWND) -> Result<HWND> {
    let module = GetModuleHandleW(None).map_err(|error| error.to_string())?;
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: HINSTANCE(module.0),
        hCursor: LoadCursorW(None, IDC_ARROW).map_err(|error| error.to_string())?,
        hbrBackground: HBRUSH::default(),
        lpszClassName: w!("WaWFxViewport"),
        ..Default::default()
    };
    RegisterClassW(&class);
    let style = GetWindowLongPtrW(parent, GWL_STYLE);
    SetWindowLongPtrW(parent, GWL_STYLE, style | WS_CLIPCHILDREN.0 as isize);
    let mut sibling = GetWindow(parent, GW_CHILD).ok();
    while let Some(window) = sibling {
        let style = GetWindowLongPtrW(window, GWL_STYLE);
        SetWindowLongPtrW(window, GWL_STYLE, style | WS_CLIPSIBLINGS.0 as isize);
        sibling = GetWindow(window, GW_HWNDNEXT).ok();
    }
    CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("WaWFxViewport"),
        w!("FX preview"),
        WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
        0,
        0,
        512,
        512,
        Some(parent),
        None,
        Some(HINSTANCE(module.0)),
        None,
    )
    .map_err(|error| error.to_string())
}

struct Player {
    id: String,
    selection: u64,
    playback: Playback,
    frame: Value,
    initial_frame: Value,
    next: Option<StreamFrame>,
    completion: Option<u32>,
    matrix: Value,
    projection: Value,
    plane: Option<Value>,
    position: f64,
    speed: f64,
    playing: bool,
    repeat: bool,
    show_plane: bool,
    last_frame: Option<u32>,
    last_input: u64,
    dirty: bool,
}

impl Player {
    fn restart(&mut self) {
        let prepared = self.playback.prepared.clone();
        self.playback = Playback::start(prepared);
        self.frame = self.initial_frame.clone();
        self.next = None;
        self.completion = None;
        self.position = 0.0;
        self.playing = true;
        self.dirty = true;
    }

    fn advance(&mut self, elapsed: f64) -> Result<()> {
        if !self.playing {
            return Ok(());
        }
        let target = self.position + elapsed * self.speed;
        loop {
            let packet = if let Some(packet) = self.next.take() {
                Some(packet)
            } else {
                self.playback.next()?
            };
            let Some(packet) = packet else {
                // Stop at the latest available sample without moving the
                // playback clock backwards. A briefly empty queue is neither
                // an effect restart nor evidence that its lifetime has ended.
                self.position = self
                    .position
                    .max(self.frame["time"].as_f64().unwrap_or(0.0));
                break;
            };
            let time = packet.frame["time"]
                .as_u64()
                .ok_or("Missing live frame time")? as u32;
            if f64::from(time) > target {
                self.position = target;
                self.next = Some(packet);
                break;
            }
            let current_time = self.frame["time"].as_u64().unwrap_or(0) as u32;
            let unpresented_visual = self.last_frame != Some(current_time)
                && ["draws", "lights", "models"].iter().any(|field| {
                    self.frame[*field]
                        .as_array()
                        .is_some_and(|items| !items.is_empty())
                });
            if packet.lifecycle.completion_time.is_some() && unpresented_visual {
                // A real 1-2 ms burst can begin and end between display ticks.
                // Present its actual sampled frame once before the terminal
                // packet replaces it. Its simulation lifetime is not extended.
                self.position = f64::from(time);
                self.next = Some(packet);
                break;
            }
            self.completion = packet.lifecycle.completion_time;
            self.frame = packet.frame;
            if let Some(end) = self.completion {
                // This terminal packet is emitted only after future births,
                // children and trail tails have all finished. Blank frames
                // between bursts never cause a restart.
                if self.repeat {
                    self.restart();
                } else {
                    self.position = f64::from(end);
                    self.playing = false;
                }
                break;
            }
        }
        Ok(())
    }
}

struct Host {
    window: HWND,
    thumbnail_window: HWND,
    parent: HWND,
    game: PathBuf,
    owner: std::thread::ThreadId,
    bounds: Bounds,
    renderer: Option<Renderer>,
    thumbnails: Option<Renderer>,
    thumbnail_job: Option<u64>,
    thumbnail_best: Option<(Vec<u8>, usize)>,
    player: Option<Player>,
    last_tick: Instant,
    last_status: Instant,
    last_error: Option<String>,
}

impl Host {
    fn assert_owner(&self) {
        assert_eq!(self.owner, std::thread::current().id());
    }

    unsafe fn renderer(&mut self) -> Result<&mut Renderer> {
        self.assert_owner();
        if self.renderer.is_none() {
            let mut renderer = Renderer::new(
                self.window,
                self.bounds.width.max(2),
                self.bounds.height.max(2),
                RESOLUTION,
                &self.game,
            )
            .map_err(render_error)?;
            if let Some(player) = &self.player {
                renderer
                    .load_frames(&player.playback.prepared.material_frames)
                    .map_err(render_error)?;
            }
            self.renderer = Some(renderer);
        }
        self.renderer
            .as_mut()
            .ok_or_else(|| "transient: Native renderer is unavailable".into())
    }

    unsafe fn layout(&mut self, bounds: Bounds) -> Result<()> {
        let resized = bounds.width != self.bounds.width || bounds.height != self.bounds.height;
        if resized {
            // Dropping releases every default-pool resource before a new device
            // is created. No COM resource or HWND crosses its owner thread.
            self.renderer.take();
            if let Some(player) = self.player.as_mut() {
                player.dirty = true;
            }
        }
        if bounds.visible && !self.bounds.visible {
            if let Some(player) = self.player.as_mut() {
                player.dirty = true;
            }
        }
        let visible = bounds.visible && bounds.width >= 2 && bounds.height >= 2;
        if visible {
            SetWindowPos(
                self.window,
                Some(HWND_TOP),
                bounds.x,
                bounds.y,
                bounds.width as i32,
                bounds.height as i32,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
            .map_err(|error| error.to_string())?;
        } else {
            let _ = ShowWindow(self.window, SW_HIDE);
        }
        INPUT.with(|input| input.borrow_mut().side = bounds.width.min(bounds.height).max(1) as f32);
        self.bounds = bounds;
        self.last_tick = Instant::now();
        Ok(())
    }

    unsafe fn draw(&mut self) -> Result<Option<Value>> {
        self.assert_owner();
        let now = Instant::now();
        let delta = now.duration_since(self.last_tick).as_secs_f64() * 1000.0;
        self.last_tick = now;
        if self.last_error.is_some()
            || !self.bounds.visible
            || IsIconic(self.parent).as_bool()
            || self.player.is_none()
        {
            return Ok(None);
        }
        self.renderer()?;
        match self.renderer.as_ref().ok_or("No renderer")?.device_state() {
            0 => {}
            // A temporarily lost device must not mark an asset unsupported.
            0x8876_0868 => return Ok(None),
            0x8876_0869 => {
                self.renderer.take();
                self.renderer()?;
                if let Some(player) = self.player.as_mut() {
                    player.dirty = true;
                }
            }
            _ => {
                return Err(
                    "transient: The graphics device is unavailable. Retry the preview.".into(),
                )
            }
        }
        let player = self.player.as_mut().ok_or("No effect selected")?;
        player.advance(delta)?;
        let frame_time = player.frame["time"].as_u64().unwrap_or(0) as u32;
        let (pan, zoom, revision) = INPUT.with(|input| {
            let input = input.borrow();
            (input.pan, input.zoom.exp(), input.revision)
        });
        if player.last_frame != Some(frame_time) || player.last_input != revision || player.dirty {
            let frame = &mut player.frame;
            frame["backgroundPreset"] = json!(BACKGROUND.with(Cell::get).name());
            frame["matrixRows"] = viewing_transform(&player.matrix, pan, zoom);
            frame["projectionMatrixRows"] = viewing_transform(&player.projection, pan, zoom);
            if player.show_plane {
                if let Some(plane) = &player.plane {
                    frame["previewPlane"] = plane.clone();
                }
            } else if let Some(plane) = &player.plane {
                let mut depth_only = plane.clone();
                depth_only["depthOnly"] = json!(true);
                frame["previewPlane"] = depth_only;
            }
            let renderer = self.renderer.as_ref().ok_or("No renderer")?;
            renderer.frame(frame).map_err(render_error)?;
            renderer
                .present(self.bounds.width, self.bounds.height)
                .map_err(render_error)?;
            player.last_frame = Some(frame_time);
            player.last_input = revision;
            player.dirty = false;
        }
        if now.duration_since(self.last_status) >= Duration::from_millis(100) {
            self.last_status = now;
            let prepared = &player.playback.prepared;
            let duration = player
                .completion
                .or(prepared.duration)
                .map(|time| f64::from(time) / 1000.0);
            return Ok(Some(
                json!({"id":player.id,"selection":player.selection,"name":prepared.name,
                "state":if player.playing {"playing"} else if player.completion.is_some() {"ended"} else {"paused"},
                "time":player.position / 1000.0,"duration":duration,"continuous":prepared.continuous,
                "activeParticles":player.frame["draws"].as_array().map_or(0, Vec::len)}),
            ));
        }
        Ok(None)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.assert_owner();
        self.renderer.take();
        self.thumbnails.take();
        unsafe {
            let _ = DestroyWindow(self.thumbnail_window);
            let _ = DestroyWindow(self.window);
        }
    }
}

fn render_error(error: Box<dyn std::error::Error>) -> String {
    if error.downcast_ref::<windows::core::Error>().is_some()
        || error.downcast_ref::<std::io::Error>().is_some()
    {
        format!("transient: Could not render this preview: {error}")
    } else {
        format!("unsupported: {error}")
    }
}

/// Screen pan and magnification keep the simulated eye fixed. Moving that eye
/// would require regenerating distance fades, trails and view-dependent geometry.
fn viewing_transform(original: &Value, pan: [f32; 2], zoom: f32) -> Value {
    let mut output = original.clone();
    for row in 0..2 {
        for column in 0..4 {
            output[row][column] = json!(
                original[row][column].as_f64().unwrap_or(0.0) * f64::from(zoom)
                    + f64::from(pan[row]) * original[3][column].as_f64().unwrap_or(0.0)
            );
        }
    }
    output
}

pub fn set_bounds(parent: usize, game: &Path, bounds: Bounds) -> Result<()> {
    if bounds.width > 8192
        || bounds.height > 8192
        || !bounds.scale.is_finite()
        || bounds.scale <= 0.0
    {
        return Err("Invalid viewport dimensions".into());
    }
    HOST.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() && bounds.width >= 2 && bounds.height >= 2 {
            let parent = HWND(parent as *mut c_void);
            let window = unsafe { create_child(parent)? };
            let thumbnail_window = match unsafe { create_child(parent) } {
                Ok(window) => window,
                Err(error) => {
                    unsafe {
                        let _ = DestroyWindow(window);
                    }
                    return Err(error);
                }
            };
            *slot = Some(Host {
                window,
                thumbnail_window,
                parent,
                game: game.to_owned(),
                owner: std::thread::current().id(),
                bounds: bounds.clone(),
                renderer: None,
                thumbnails: None,
                thumbnail_job: None,
                thumbnail_best: None,
                player: None,
                last_tick: Instant::now(),
                last_status: Instant::now(),
                last_error: None,
            });
        }
        if let Some(host) = slot.as_mut() {
            unsafe {
                host.layout(bounds)?;
            }
        }
        Ok(())
    })
}

pub fn install_playback(
    id: String,
    selection: u64,
    playback: Playback,
    first: StreamFrame,
) -> Result<Value> {
    HOST.with(|slot| {
        let mut slot = slot.borrow_mut();
        let host = slot.as_mut().ok_or("transient: The preview area is not ready")?;
        unsafe { host.renderer()?.load_frames(&playback.prepared.material_frames).map_err(render_error)?; }
        let prepared = &playback.prepared;
        let metadata = json!({"id":id,"selection":selection,"name":prepared.name,"duration":prepared.duration.map(|time| f64::from(time)/1000.0),
            "looping":prepared.continuous,"continuous":prepared.continuous,"truncated":false,
            "lightReceiver":first.frame.get("previewPlane").is_some_and(|plane| plane["depthOnly"] != true),
            "focusedView":prepared.camera["focusedView"] == true,
            "delayedActivity":prepared.camera["delayedActivityMsec"].as_u64().is_some_and(|time| time > 0)});
        let matrix = first.frame["matrixRows"].clone();
        let projection = first.frame["projectionMatrixRows"].clone();
        let plane = first.frame.get("previewPlane").cloned();
        host.player = Some(Player { id, selection, playback, matrix, projection, plane, position:0.0,
            initial_frame:first.frame.clone(),frame:first.frame, next:None, completion:first.lifecycle.completion_time,
            speed:1.0, playing:true, repeat:true, show_plane:true, last_frame:None,
            last_input:0, dirty:true });
        host.last_error = None;
        host.last_tick = Instant::now();
        INPUT.with(|input| { let side = input.borrow().side; *input.borrow_mut() = Input { side, ..Input::default() }; });
        Ok(metadata)
    })
}

/// Offline checks can consume all batches directly. The desktop command uses
/// thumbnail_batch and yields back to the UI event loop between bounded calls.
pub fn thumbnail(clip: &PreparedClip) -> Result<Vec<u8>> {
    let mut start = 0;
    loop {
        if let Some(pixels) = thumbnail_batch(0, clip, start, 4)? {
            return Ok(pixels);
        }
        start += 4;
    }
}

/// Render a small number of candidates on the owner thread. Loading and draw
/// state belongs to one job, so a superseded continuation cannot accidentally
/// use a newer thumbnail's material table.
pub fn thumbnail_batch(
    job: u64,
    clip: &PreparedClip,
    start: usize,
    count: usize,
) -> Result<Option<Vec<u8>>> {
    thumbnail_batch_with_background(job, clip, start, count, Preset::Dark)
}

pub fn thumbnail_batch_with_background(
    job: u64,
    clip: &PreparedClip,
    start: usize,
    count: usize,
    preset: Preset,
) -> Result<Option<Vec<u8>>> {
    HOST.with(|slot| {
        let mut slot = slot.borrow_mut();
        let host = slot
            .as_mut()
            .ok_or("transient: The preview area is not ready")?;
        host.assert_owner();
        if start == 0 {
            host.thumbnail_job = None;
            host.thumbnail_best = None;
        } else if host.thumbnail_job != Some(job) {
            return Err("transient: Thumbnail request was replaced".into());
        }
        if let Some(renderer) = host.thumbnails.as_ref() {
            match unsafe { renderer.device_state() } {
                0 => {}
                0x8876_0868 => {
                    return Err(
                        "transient: Graphics device is temporarily unavailable; retry".into(),
                    )
                }
                _ => {
                    host.thumbnails.take();
                    host.thumbnail_job = None;
                    host.thumbnail_best = None;
                    if start > 0 {
                        return Err("transient: The thumbnail renderer was reset; retry".into());
                    }
                }
            }
        }
        if host.thumbnails.is_none() {
            host.thumbnails = Some(unsafe {
                Renderer::new(
                    host.thumbnail_window,
                    THUMBNAIL_SIZE,
                    THUMBNAIL_SIZE,
                    THUMBNAIL_SIZE,
                    &host.game,
                )
                .map_err(render_error)?
            });
        }
        let renderer = host
            .thumbnails
            .as_mut()
            .ok_or("transient: Thumbnail renderer unavailable")?;
        unsafe {
            if start == 0 {
                renderer.load_frames(&clip.frames).map_err(render_error)?;
                host.thumbnail_job = Some(job);
            }
            let end = start
                .saturating_add(count.clamp(1, 8))
                .min(clip.thumbnail_candidates.len());
            for &index in clip.thumbnail_candidates.get(start..end).unwrap_or(&[]) {
                let mut frame = clip.frames[index].clone();
                frame["backgroundPreset"] = json!(preset.name());
                renderer.frame(&frame).map_err(render_error)?;
                let mut pixels = renderer.pixels().map_err(render_error)?;
                let mut empty = frame.clone();
                empty["draws"] = json!([]);
                empty["lights"] = json!([]);
                renderer.frame(&empty).map_err(render_error)?;
                let baseline = renderer.pixels().map_err(render_error)?;
                let changed_pixels = pixels
                    .chunks_exact(4)
                    .zip(baseline.chunks_exact(4))
                    .filter(|(a, b)| a[..3] != b[..3])
                    .count();
                if changed_pixels == 0 {
                    continue;
                }
                if host
                    .thumbnail_best
                    .as_ref()
                    .is_none_or(|(_, best)| changed_pixels > *best)
                {
                    for pixel in pixels.chunks_exact_mut(4) {
                        pixel.swap(0, 2);
                        pixel[3] = 255;
                    }
                    host.thumbnail_best = Some((pixels, changed_pixels));
                }
                // Stop promptly for a useful still. For very small/faint FX,
                // keep the strongest actual sample instead of caching the
                // first isolated pixel. Only one bounded RGBA image is kept.
                if changed_pixels >= 32 {
                    host.thumbnail_job = None;
                    return Ok(host.thumbnail_best.take().map(|(pixels, _)| pixels));
                }
            }
            if end >= clip.thumbnail_candidates.len() {
                host.thumbnail_job = None;
                if let Some((pixels, _)) = host.thumbnail_best.take() {
                    return Ok(Some(pixels));
                }
                return Err("empty: No visible pixels were found in the sampled frames".into());
            }
            Ok(None)
        }
    })
}

/// This changes the scene behind the effect, never its simulation or clock.
/// It also works before a selection exists and redraws a paused effect once.
pub fn set_background(preset: Preset) {
    BACKGROUND.with(|background| background.set(preset));
    HOST.with(|slot| {
        if let Some(player) = slot
            .borrow_mut()
            .as_mut()
            .and_then(|host| host.player.as_mut())
        {
            player.dirty = true;
        }
    });
}

/// A delayed command from an older selection must never change the current
/// effect. Discard it quietly; the caller has already moved on.
pub fn control_for_selection(selection: u64, action: &str, value: &Value) -> Result<()> {
    HOST.with(|slot| {
        let slot = slot.borrow();
        let current = slot.as_ref().and_then(|host| host.player.as_ref());
        if current.is_none_or(|player| player.selection != selection) {
            return Ok(());
        }
        drop(slot);
        control(action, value)
    })
}

pub fn control(action: &str, value: &Value) -> Result<()> {
    HOST.with(|slot| {
        let mut slot = slot.borrow_mut();
        let host = slot.as_mut().ok_or("No preview area")?;
        let player = host.player.as_mut().ok_or("Select an effect first")?;
        match action {
            "play" => {
                if player.completion.is_some() {
                    player.restart();
                }
                player.playing = true;
            }
            "pause" => player.playing = false,
            "restart" => {
                player.restart();
            }
            "speed" => {
                player.speed = value
                    .as_f64()
                    .filter(|speed| [0.25, 0.5, 1.0, 2.0].contains(speed))
                    .ok_or("Invalid speed")?
            }
            "loop" => player.repeat = value.as_bool().ok_or("Invalid loop setting")?,
            "plane" => {
                player.show_plane = value.as_bool().ok_or("Invalid plane setting")?;
                player.dirty = true;
            }
            "reset_view" => {
                INPUT.with(|input| {
                    let side = input.borrow().side;
                    *input.borrow_mut() = Input {
                        side,
                        ..Input::default()
                    };
                });
                player.dirty = true;
            }
            _ => return Err("Unknown playback control".into()),
        }
        host.last_tick = Instant::now();
        Ok(())
    })
}

pub fn clear() {
    HOST.with(|slot| {
        if let Some(host) = slot.borrow_mut().as_mut() {
            host.player.take();
            host.renderer.take();
            unsafe {
                let _ = ShowWindow(host.window, SW_HIDE);
            }
        }
    });
}

pub fn tick() -> Option<Value> {
    HOST.with(|slot| {
        let mut slot = slot.borrow_mut();
        let host = slot.as_mut()?;
        match unsafe { host.draw() } {
            Ok(update) => update,
            Err(error) => {
                host.last_error = Some(error.clone());
                let player = host.player.as_mut()?;
                player.playing = false;
                Some(
                    json!({"id":player.id,"selection":player.selection,"name":player.playback.prepared.name,"state":"error","message":error}),
                )
            }
        }
    })
}

pub fn shutdown() {
    HOST.with(|slot| {
        slot.borrow_mut().take();
    });
}
