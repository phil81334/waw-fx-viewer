#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod catalogue;
mod diagnostics;
mod fx;
mod playback;
mod viewport;

use base64::{engine::general_purpose::STANDARD, Engine};
use fx::renderer::background::Preset;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
};
use tauri::{Emitter, Manager};

struct Runtime {
    game: PathBuf,
    catalogue: RwLock<catalogue::Catalogue>,
    preparation: Mutex<()>,
    thumbnail_gpu: tauri::async_runtime::Mutex<()>,
    thumbnail_job: AtomicU64,
    selection: AtomicU64,
    background: Mutex<Preset>,
    thumbnail_background: Mutex<Preset>,
    thumbnail_background_revision: AtomicU64,
}

#[derive(Default)]
struct AppState {
    runtime: Mutex<Option<Arc<Runtime>>>,
}

fn game_root() -> Result<PathBuf, String> {
    let path = if cfg!(debug_assertions) {
        let saved = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.local/game-path.txt");
        std::env::var_os("WAW_ROOT")
            .map(PathBuf::from)
            .or_else(|| {
                std::fs::read_to_string(saved)
                    .ok()
                    .filter(|text| !text.trim().is_empty())
                    .map(|text| PathBuf::from(text.trim()))
            })
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.local/test-root")
            })
    } else {
        std::env::current_exe()
            .map_err(|error| error.to_string())?
            .parent()
            .ok_or("Cannot locate the app folder")?
            .to_path_buf()
    };
    if !path.join("CoDWaW.exe").is_file() {
        return Err("Put this executable in your World at War folder, beside CoDWaW.exe, then open it again.".into());
    }
    Ok(path)
}

fn runtime(app: &tauri::AppHandle, state: &AppState) -> Result<Arc<Runtime>, String> {
    let mut slot = state.runtime.lock().map_err(|error| error.to_string())?;
    if let Some(runtime) = slot.as_ref() {
        return Ok(runtime.clone());
    }
    let game = game_root()?;
    for path in [
        "raw/fx",
        "raw/materials",
        "raw/techsets",
        "raw/techniques",
        "raw/shader_bin",
        "main",
    ] {
        if !game.join(path).is_dir() {
            return Err(
                "Install the WaW Modtools in this game folder first, then open the viewer again."
                    .into(),
            );
        }
    }
    let id = format!("{:x}", Sha256::digest(game.to_string_lossy().as_bytes()));
    let cache = app
        .path()
        .app_cache_dir()
        .map_err(|error| error.to_string())?
        .join("fx-v1")
        .join(id);
    let value = Arc::new(Runtime {
        catalogue: RwLock::new(catalogue::Catalogue::new(game.clone(), cache)),
        game,
        preparation: Mutex::new(()),
        thumbnail_gpu: tauri::async_runtime::Mutex::new(()),
        thumbnail_job: AtomicU64::new(0),
        selection: AtomicU64::new(0),
        background: Mutex::new(Preset::Dark),
        thumbnail_background: Mutex::new(Preset::Dark),
        thumbnail_background_revision: AtomicU64::new(0),
    });
    *slot = Some(value.clone());
    Ok(value)
}

#[tauri::command]
fn get_context(app: tauri::AppHandle, state: tauri::State<AppState>) -> Value {
    match runtime(&app, &state) {
        Ok(runtime) => json!({"valid":true,"modtoolsPresent":true,"gameRoot":runtime.game,
            "development":cfg!(debug_assertions),"version":env!("CARGO_PKG_VERSION"),
            "diagnosticPath":diagnostics::path(&app).ok(),
            "background":runtime.background.lock().map(|value| value.name()).unwrap_or("dark"),
            "thumbnailBackground":runtime.thumbnail_background.lock().map(|value| value.name()).unwrap_or("dark")}),
        Err(error) => json!({"valid":false,"modtoolsPresent":false,"error":error,
            "development":cfg!(debug_assertions),"version":env!("CARGO_PKG_VERSION")}),
    }
}

/// Only plain packets cross threads. The closure owns every native API call.
async fn on_ui<T: Send + 'static>(
    app: &tauri::AppHandle,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let _ = sender.send(work());
    })
    .map_err(|error| error.to_string())?;
    tauri::async_runtime::spawn_blocking(move || {
        receiver.recv().map_err(|error| error.to_string())?
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn viewport_bounds(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    bounds: viewport::Bounds,
) -> Result<(), String> {
    let runtime = runtime(&app, &state)?;
    let parent = app
        .get_webview_window("main")
        .ok_or("Window is closed")?
        .hwnd()
        .map_err(|error| error.to_string())?
        .0 as usize;
    on_ui(&app, move || {
        viewport::set_bounds(parent, &runtime.game, bounds)
    })
    .await
}

#[tauri::command]
async fn scan_catalogue(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    force: bool,
) -> Result<catalogue::CatalogueIndex, String> {
    let runtime = runtime(&app, &state)?;
    runtime.selection.fetch_add(1, Ordering::AcqRel);
    on_ui(&app, || {
        viewport::clear();
        Ok(())
    })
    .await?;
    let progress = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _work = runtime
            .preparation
            .lock()
            .map_err(|error| error.to_string())?;
        runtime
            .catalogue
            .write()
            .map_err(|error| error.to_string())?
            .scan(force, |update| {
                let _ = progress.emit("fx-scan-progress", update);
            })
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn select_effect(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<Value, String> {
    let runtime = runtime(&app, &state)?;
    let token = runtime.selection.fetch_add(1, Ordering::AcqRel) + 1;
    let generation = runtime
        .catalogue
        .read()
        .map_err(|error| error.to_string())?
        .generation();
    let result = select_effect_request(&app, &runtime, id.clone(), token).await;
    if let Err(error) = &result {
        remember_failure(&app, &runtime, &id, error, generation, "selection").await;
    }
    result
}

async fn select_effect_request(
    app: &tauri::AppHandle,
    runtime: &Arc<Runtime>,
    id: String,
    token: u64,
) -> Result<Value, String> {
    let worker = runtime.clone();
    let name_id = id.clone();
    let (playback, first) = tauri::async_runtime::spawn_blocking(move || {
        let _work = worker
            .preparation
            .lock()
            .map_err(|error| error.to_string())?;
        if token != worker.selection.load(Ordering::Acquire) {
            return Err("transient: A newer selection replaced this request".into());
        }
        let source = worker
            .catalogue
            .read()
            .map_err(|error| error.to_string())?
            .source(&name_id)?;
        let effect_name = source["name"].as_str().unwrap_or(&name_id).to_owned();
        let prepared = fx::prepare::prepare_playback_with_cancel(&worker.game, source, &|| {
            token != worker.selection.load(Ordering::Acquire)
        })
        .map_err(|error| {
            if error.starts_with("transient:") {
                error
            } else {
                format!("unsupported: {effect_name}: {error}")
            }
        })?;
        let playback = playback::Playback::start(Arc::new(prepared));
        let first = playback.first()?;
        Ok::<_, String>((playback, first))
    })
    .await
    .map_err(|error| error.to_string())??;
    let runtime = runtime.clone();
    on_ui(app, move || {
        if token != runtime.selection.load(Ordering::Acquire) {
            return Err("transient: A newer selection replaced this request".into());
        }
        viewport::install_playback(id, token, playback, first)
    })
    .await
}

#[tauri::command]
async fn thumbnail(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    preset: Option<String>,
) -> Result<String, String> {
    let runtime = runtime(&app, &state)?;
    let preset = requested_thumbnail_background(&runtime, preset.as_deref())?;
    let generation = runtime
        .catalogue
        .read()
        .map_err(|error| error.to_string())?
        .generation();
    let background_revision = runtime
        .thumbnail_background_revision
        .load(Ordering::Acquire);
    let result = thumbnail_request(&app, &runtime, id.clone(), preset, background_revision).await;
    if let Err(error) = &result {
        remember_failure(&app, &runtime, &id, error, generation, "thumbnail").await;
    }
    result
}

async fn remember_failure(
    app: &tauri::AppHandle,
    runtime: &Arc<Runtime>,
    id: &str,
    error: &str,
    generation: u64,
    kind: &str,
) {
    diagnostics::record(app, kind, &format!("{id}: {error}"));
    if error.starts_with("unsupported:") {
        let worker = runtime.clone();
        let id = id.to_owned();
        let reason = error.to_owned();
        let _ = tauri::async_runtime::spawn_blocking(move || {
            worker
                .catalogue
                .read()
                .map_err(|error| error.to_string())?
                .record_preview_failure(&id, &reason, generation)
        })
        .await;
    }
}

async fn thumbnail_request(
    app: &tauri::AppHandle,
    runtime: &Arc<Runtime>,
    id: String,
    preset: Preset,
    background_revision: u64,
) -> Result<String, String> {
    let cache_worker = runtime.clone();
    let cache_id = id.clone();
    let cached = tauri::async_runtime::spawn_blocking(move || {
        let catalogue = cache_worker
            .catalogue
            .read()
            .map_err(|error| error.to_string())?;
        Ok::<_, String>(catalogue.cached_thumbnail_for_background(&cache_id, preset.name()))
    })
    .await
    .map_err(|error| error.to_string())??;
    if let Some(cached) = cached {
        return Ok(cached);
    }
    let selection = runtime.selection.load(Ordering::Acquire);
    let mut fallback = false;
    let mut first_clip = None;
    let (pixels, generation) = loop {
        let worker = runtime.clone();
        let request_id = id.clone();
        let (clip, generation) = tauri::async_runtime::spawn_blocking(move || {
            let _work = worker
                .preparation
                .lock()
                .map_err(|error| error.to_string())?;
            if selection != worker.selection.load(Ordering::Acquire) {
                return Err(
                    "transient: Selected preview takes priority; retry this thumbnail".into(),
                );
            }
            if background_revision != worker.thumbnail_background_revision.load(Ordering::Acquire) {
                return Err("transient: Preview background changed; retry this thumbnail".into());
            }
            let (source, generation) = {
                let catalogue = worker.catalogue.read().map_err(|error| error.to_string())?;
                (catalogue.source(&request_id)?, catalogue.generation())
            };
            let cancelled = || {
                selection != worker.selection.load(Ordering::Acquire)
                    || background_revision
                        != worker.thumbnail_background_revision.load(Ordering::Acquire)
            };
            let clip = if fallback {
                fx::prepare::prepare_thumbnail_fallback_with_cancel(&worker.game, source, &cancelled)
            } else {
                fx::prepare::prepare_thumbnail_with_cancel(&worker.game, source, &cancelled)
            }
            .map_err(|error| {
                if error.starts_with("transient:") {
                    error
                } else {
                    format!("unsupported: Could not prepare {request_id}: {error}")
                }
            })?;
            Ok::<_, String>((Arc::new(clip), generation))
        })
        .await
        .map_err(|error| error.to_string())??;
        // Yield between small GPU batches, while retaining exclusive ownership
        // of the hidden thumbnail renderer for this complete request.
        let gpu = runtime.thumbnail_gpu.lock().await;
        let job = runtime.thumbnail_job.fetch_add(1, Ordering::Relaxed) + 1;
        let result = render_thumbnail(
            app,
            runtime,
            selection,
            job,
            clip.clone(),
            preset,
            background_revision,
        )
        .await;
        drop(gpu);
        match result {
            Ok(pixels) => break (pixels, generation),
            Err(error) if error.starts_with("empty:") && !fallback => {
                first_clip = Some(clip);
                fallback = true;
            }
            Err(error) if error.starts_with("empty:") => {
                // Absence on one background is not an effect-wide failure.
                // Reuse both completed clips to check the other presets;
                // never repeat simulation or change authored effect settings.
                for alternative in [
                    Preset::Dark,
                    Preset::Grey,
                    Preset::Light,
                    Preset::Checkerboard,
                ] {
                    if alternative == preset {
                        continue;
                    }
                    for candidate in first_clip.iter().chain(std::iter::once(&clip)) {
                        let _gpu = runtime.thumbnail_gpu.lock().await;
                        let next_job = runtime.thumbnail_job.fetch_add(1, Ordering::Relaxed) + 1;
                        match render_thumbnail(
                            app,
                            runtime,
                            selection,
                            next_job,
                            candidate.clone(),
                            alternative,
                            background_revision,
                        )
                        .await
                        {
                            Ok(_) => {
                                return Err("empty: No visible preview on this thumbnail background; another background works".into());
                            }
                            Err(reason) if reason.starts_with("empty:") => {}
                            Err(reason) => return Err(reason),
                        }
                    }
                }
                return Err("unsupported: No visible thumbnail output on any preview background in the sampled effect timeline".into());
            }
            Err(error) => return Err(error),
        }
    };
    let runtime = runtime.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut buffer = std::io::Cursor::new(Vec::new());
        image::write_buffer_with_format(
            &mut buffer,
            &pixels,
            192,
            192,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .map_err(|error| error.to_string())?;
        let bytes = buffer.into_inner();
        runtime
            .catalogue
            .read()
            .map_err(|error| error.to_string())?
            .save_thumbnail_for_background(&id, &bytes, generation, preset.name())?;
        Ok(format!("data:image/png;base64,{}", STANDARD.encode(bytes)))
    })
    .await
    .map_err(|error| error.to_string())?
}

async fn render_thumbnail(
    app: &tauri::AppHandle,
    runtime: &Arc<Runtime>,
    selection: u64,
    job: u64,
    clip: Arc<fx::prepare::PreparedClip>,
    preset: Preset,
    background_revision: u64,
) -> Result<Vec<u8>, String> {
    let mut start = 0;
    loop {
        let guard = runtime.clone();
        let frames = clip.clone();
        let pixels = on_ui(app, move || {
            if selection != guard.selection.load(Ordering::Acquire) {
                return Err(
                    "transient: Selected preview takes priority; retry this thumbnail".into(),
                );
            }
            if background_revision != guard.thumbnail_background_revision.load(Ordering::Acquire) {
                return Err("transient: Preview background changed; retry this thumbnail".into());
            }
            viewport::thumbnail_batch_with_background(job, &frames, start, 4, preset)
        })
        .await?;
        if let Some(pixels) = pixels {
            return Ok(pixels);
        }
        start += 4;
    }
}

#[tauri::command]
async fn playback_control(
    app: tauri::AppHandle,
    selection: u64,
    action: String,
    value: Value,
) -> Result<(), String> {
    on_ui(&app, move || {
        viewport::control_for_selection(selection, &action, &value)
    })
    .await
}

/// Cached cards must not wait behind a different effect's CPU/GPU preparation.
#[tauri::command]
async fn cached_thumbnails(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    ids: Vec<String>,
    preset: Option<String>,
) -> Result<std::collections::HashMap<String, String>, String> {
    if ids.len() > 64 {
        return Err("Too many cached previews requested at once".into());
    }
    let runtime = runtime(&app, &state)?;
    let preset = requested_thumbnail_background(&runtime, preset.as_deref())?;
    tauri::async_runtime::spawn_blocking(move || {
        let catalogue = runtime
            .catalogue
            .read()
            .map_err(|error| error.to_string())?;
        let mut previews = std::collections::HashMap::new();
        for id in ids {
            if let Some(image) = catalogue.cached_thumbnail_for_background(&id, preset.name()) {
                previews.insert(id, image);
            }
        }
        Ok(previews)
    })
    .await
    .map_err(|error| error.to_string())?
}

fn requested_thumbnail_background(runtime: &Runtime, name: Option<&str>) -> Result<Preset, String> {
    if let Some(name) = name {
        Preset::parse(name)
    } else {
        runtime
            .thumbnail_background
            .lock()
            .map(|value| *value)
            .map_err(|error| error.to_string())
    }
}

#[tauri::command]
async fn set_background(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    preset: String,
) -> Result<(), String> {
    let runtime = runtime(&app, &state)?;
    let preset = Preset::parse(&preset)?;
    on_ui(&app, move || {
        let mut current = runtime
            .background
            .lock()
            .map_err(|error| error.to_string())?;
        *current = preset;
        viewport::set_background(preset);
        Ok(())
    })
    .await
}

/// Thumbnail changes have their own generation. They never redraw/restart the
/// selected viewport, and viewport changes never cancel thumbnail work.
#[tauri::command]
fn set_thumbnail_background(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    preset: String,
) -> Result<(), String> {
    let runtime = runtime(&app, &state)?;
    let preset = Preset::parse(&preset)?;
    let mut current = runtime
        .thumbnail_background
        .lock()
        .map_err(|error| error.to_string())?;
    if *current != preset {
        *current = preset;
        runtime
            .thumbnail_background_revision
            .fetch_add(1, Ordering::AcqRel);
    }
    Ok(())
}

#[tauri::command]
fn report_ui(app: tauri::AppHandle, kind: String, message: String) {
    diagnostics::record(&app, &kind, &message);
}

#[tauri::command]
async fn cache_info(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<catalogue::CacheInfo, String> {
    let runtime = runtime(&app, &state)?;
    tauri::async_runtime::spawn_blocking(move || {
        runtime
            .catalogue
            .read()
            .map_err(|error| error.to_string())?
            .cache_info()
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn cache_clear(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let runtime = runtime(&app, &state)?;
    runtime.selection.fetch_add(1, Ordering::AcqRel);
    on_ui(&app, || {
        viewport::clear();
        Ok(())
    })
    .await?;
    tauri::async_runtime::spawn_blocking(move || {
        runtime
            .catalogue
            .read()
            .map_err(|error| error.to_string())?
            .clear()
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn quit(app: tauri::AppHandle) {
    app.exit(0);
}

fn remember_playback_failure(app: &tauri::AppHandle, update: &Value) {
    diagnostics::record(app, "playback", &update.to_string());
    let Some(reason) = update["message"]
        .as_str()
        .filter(|message| message.starts_with("unsupported:"))
    else {
        return;
    };
    let Some(id) = update["id"].as_str() else {
        return;
    };
    let Some(selection) = update["selection"].as_u64() else {
        return;
    };
    let state = app.state::<AppState>();
    let runtime = state.runtime.lock().ok().and_then(|slot| slot.clone());
    if let Some(runtime) = runtime {
        let id = id.to_owned();
        let reason = reason.to_owned();
        tauri::async_runtime::spawn_blocking(move || {
            if runtime.selection.load(Ordering::Acquire) != selection {
                return;
            }
            if let Ok(catalogue) = runtime.catalogue.read() {
                let _ = catalogue.record_preview_failure(&id, &reason, catalogue.generation());
            }
        });
    }
}

fn main() {
    let profile = tempfile::Builder::new()
        .prefix("waw-fx-ui-")
        .tempdir()
        .expect("Cannot create the temporary browser profile");
    let profile_path = profile.path().to_path_buf();
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            get_context,
            viewport_bounds,
            scan_catalogue,
            select_effect,
            thumbnail,
            cached_thumbnails,
            set_background,
            set_thumbnail_background,
            report_ui,
            playback_control,
            cache_info,
            cache_clear,
            quit
        ])
        .setup(move |app| {
            tauri::WebviewWindowBuilder::from_config(app, &app.config().app.windows[0])?
                .data_directory(profile_path)
                .build()?;
            let pending = Arc::new(AtomicBool::new(false));
            let handle = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_millis(16));
                if pending.swap(true, Ordering::AcqRel) {
                    continue;
                }
                let complete = pending.clone();
                let status = handle.clone();
                if handle
                    .run_on_main_thread(move || {
                        if let Some(update) = viewport::tick() {
                            if update["state"] == "error" {
                                remember_playback_failure(&status, &update);
                            }
                            let _ = status.emit("fx-status", update);
                        }
                        complete.store(false, Ordering::Release);
                    })
                    .is_err()
                {
                    break;
                }
            });
            Ok(())
        })
        .on_window_event(|_, event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                viewport::shutdown();
            }
        })
        .run(tauri::generate_context!())
        .expect("Could not start WaW FX Viewer");
}
