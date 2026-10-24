//! Per-root trail state for the existing shared child timeline.
//! Algorithms copied from the separately native-proven trail lifecycle checkpoint.
//! WaW uses sequence zero for point motion but the stored sequence for drawing.
//! All point state is owned here; no native executable is used at runtime.

use crate::fx::simulation::TrailLayer;
use crate::fx::trails::{self, DrawSegment, SegmentState, TrailDefinition};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct SourceFrame {
    pub time: u32,
    pub origin: [f32; 3],
}

/// Native FX_GetLoopingLife uses the longest enabled looping definition,
/// including non-trail and sound definitions, rather than each trail's count.
pub(crate) fn looping_life(effect: &Value) -> Result<i32, String> {
    if let Some(value) = effect.get("trailLoopingLife") {
        return value
            .as_i64()
            .and_then(|v| i32::try_from(v).ok())
            .filter(|v| *v >= 0)
            .ok_or("Invalid whole-effect looping lifespan".into());
    }
    let mut maximum = 0_i64;
    for element in effect["elements"]
        .as_array()
        .ok_or("Missing EFX elements")?
    {
        let flags = words(element, "editorFlags");
        if flags.iter().any(|value| value == "disabled")
            || !flags.iter().any(|value| value == "looping")
        {
            continue;
        }
        let values = words(element, "spawnLooping");
        if values.len() != 2 {
            return Err("Missing looping spawn interval/count".into());
        }
        let interval = values[0]
            .parse::<i64>()
            .map_err(|_| "Invalid loop interval")?
            .max(1);
        let count = values[1].parse::<i64>().map_err(|_| "Invalid loop count")?;
        if count == 0 || count == i64::from(i32::MAX) {
            return Ok(i32::MAX);
        }
        if count < 0 {
            return Err("Negative loop count".into());
        }
        maximum = maximum.max(
            interval
                .checked_mul(count - 1)
                .ok_or("Loop lifespan overflow")?,
        );
    }
    i32::try_from(maximum).map_err(|_| "Loop lifespan exceeds native i32 range".into())
}

#[derive(Clone, Debug)]
struct Point {
    origin: [f32; 3],
    distance: f32,
    begin: i32,
    sequence: u8,
    base_vel_z: i16,
}

struct Layer {
    sampler: TrailLayer,
    config: Value,
    profile: TrailDefinition,
    points: Vec<Point>,
    sequence: u8,
}

fn numbers(value: &Value) -> Result<Vec<f32>, String> {
    value
        .as_array()
        .ok_or("Missing numeric array")?
        .iter()
        .map(|v| {
            let number = v.as_f64().ok_or("Invalid number")? as f32;
            if number.is_finite() {
                Ok(number)
            } else {
                Err("Nonfinite trail value".into())
            }
        })
        .collect()
}

fn field(element: &Value, name: &str, default: f32) -> Result<f32, String> {
    match element[name].as_array().and_then(|v| v.first()) {
        Some(value) => value
            .as_str()
            .ok_or("Invalid trail token")?
            .parse()
            .map_err(|_| "Invalid trail scalar".into()),
        None => Ok(default),
    }
}

fn words(element: &Value, name: &str) -> Vec<String> {
    element[name]
        .as_array()
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim_matches('"').to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn profile(element: &Value) -> Result<TrailDefinition, String> {
    let tokens = words(element, "trailDef");
    let mut blocks = Vec::<Vec<f32>>::new();
    let mut depth = 0;
    for token in tokens {
        match token.as_str() {
            "{" => {
                depth += 1;
                blocks.push(Vec::new());
            }
            "}" => {
                depth -= 1;
            }
            _ => {
                if depth != 1 {
                    return Err("Unexpected authored trail profile nesting".into());
                }
                blocks
                    .last_mut()
                    .ok_or("Missing trail block")?
                    .push(token.parse().map_err(|_| "Invalid trail profile number")?);
            }
        }
    }
    if depth != 0 || blocks.len() != 2 || blocks[0].len() % 3 != 0 {
        return Err("Invalid authored trail profile blocks".into());
    }
    let vertices: Vec<[f32; 3]> = blocks[0]
        .chunks_exact(3)
        .map(|v| [v[0], v[1], v[2]])
        .collect();
    let mut edges = Vec::new();
    for value in &blocks[1] {
        if *value < 0.0 || value.fract() != 0.0 || *value > u16::MAX as f32 {
            return Err("Invalid trail edge index".into());
        }
        edges.push(*value as u16);
    }
    trails::compile_profile(
        &vertices,
        &edges,
        field(element, "trailSplitDist", 0.0)? as i32,
        field(element, "trailRepeatDist", 0.0)? as i32,
        field(element, "trailScrollTime", 0.0)?,
    )
}

fn random_integer(layer: &Layer, key: &str, seed: usize, offset: usize) -> i32 {
    let pair = layer.config[key].as_array().unwrap();
    pair[0].as_i64().unwrap() as i32
        + ((((pair[1].as_i64().unwrap() + 1) * layer.sampler.random_word(seed, offset)) >> 16)
            as i32)
}

fn point_seed(seed: u32, begin: i32, sequence: u8) -> usize {
    (i64::from(seed) + i64::from(begin) + 296 * i64::from(sequence)).rem_euclid(479) as usize
}

fn source_at(frames: &[SourceFrame], now: u32) -> [f32; 3] {
    for pair in frames.windows(2) {
        if now <= pair[1].time {
            let blend = (now - pair[0].time) as f32 / (pair[1].time - pair[0].time) as f32;
            return std::array::from_fn(|axis| {
                ((f64::from(pair[1].origin[axis]) - f64::from(pair[0].origin[axis]))
                    * f64::from(blend)
                    + f64::from(pair[0].origin[axis])) as f32
            });
        }
    }
    frames.last().unwrap().origin
}

fn spawn(
    layer: &mut Layer,
    seed: u32,
    scheduled: i32,
    origin: [f32; 3],
    distance: f32,
    now: i32,
    culled: bool,
) -> Result<(), String> {
    if culled {
        layer.sequence = layer.sequence.wrapping_add(1);
        return Ok(());
    }
    let delay_base = layer.config["delay"][0].as_i64().unwrap() as i32;
    let preliminary = scheduled + delay_base;
    let delay_seed = point_seed(seed, preliminary, layer.sequence);
    let begin = scheduled + random_integer(layer, "delay", delay_seed, 18);
    let random_seed = point_seed(seed, begin, layer.sequence);
    let life = random_integer(layer, "life", random_seed, 17);
    if begin + life <= now {
        return Ok(());
    }
    if layer.points.len() >= 2048 {
        return Err("Native trail point pool capacity exceeded".into());
    }
    let ranges = numbers(&layer.config["originRanges"])?;
    let position: [f32; 3] = std::array::from_fn(|axis| {
        (f64::from(origin[axis])
            + f64::from(ranges[axis * 2])
            + f64::from(ranges[axis * 2 + 1])
                * f64::from(layer.sampler.random(random_seed, 6 + axis))) as f32
    });
    let position = layer.sampler.apply_spawn_offset(
        random_seed,
        [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        position,
    );
    layer.points.push(Point {
        origin: position,
        distance,
        begin,
        sequence: layer.sequence,
        base_vel_z: 0,
    });
    layer.sequence = layer.sequence.wrapping_add(1);
    Ok(())
}

// The native velocity tables contain both sampled velocity and its prefix
// integral. Keeping those stored sample boundaries avoids inventing substeps.
fn graph_delta(
    layer: &Layer,
    seed: usize,
    life: i32,
    begin_age: i32,
    end_age: i32,
    origin: &mut [f32; 3],
) {
    let samples = layer.config["velocitySamples"].as_array().unwrap();
    let count = samples.len() - 1;
    let t0 = begin_age as f32 / life as f32;
    let t1 = end_age as f32 / life as f32;
    let start_point = (count as f64 * f64::from(t0)) as f32;
    let end_point = (count as f64 * f64::from(t1)) as f32;
    let start = (start_point.floor() as usize).min(count - 1);
    let end = (end_point.ceil() as usize).saturating_sub(1).min(count - 1);
    let a = (f64::from(start_point) - start as f64) as f32;
    let b = (f64::from(end_point) - end as f64) as f32;
    let bits = layer.config["flags"].as_u64().unwrap() as u32;
    for world in 0..2 {
        if bits & (0x01000000 << world) == 0 {
            continue;
        }
        let offset = world * 12;
        for axis in 0..3 {
            let amplitude = layer.sampler.random(seed, axis);
            let sample = |index: usize, integral: bool| -> f64 {
                let p = offset + axis + if integral { 6 } else { 0 };
                f64::from(samples[index][p].as_f64().unwrap() as f32)
                    + f64::from(amplitude)
                        * f64::from(samples[index][p + 3].as_f64().unwrap() as f32)
            };
            let scale = life as f64;
            if start == end {
                let w1 = (scale * 0.5 * (f64::from(b) * f64::from(b) - f64::from(a) * f64::from(a)))
                    as f32;
                let w0 = ((f64::from(b) - f64::from(a)) * scale - f64::from(w1)) as f32;
                origin[axis] =
                    (f64::from(origin[axis]) + sample(start, false) * f64::from(w0)) as f32;
                origin[axis] =
                    (f64::from(origin[axis]) + sample(start + 1, false) * f64::from(w1)) as f32;
            } else {
                for (index, t, sign) in [(end, b, 1.0_f64), (start, a, -1.0)] {
                    let integral_scale = scale * sign;
                    let w1 = (0.5 * f64::from(t) * f64::from(t) * integral_scale) as f32;
                    let w0 = (f64::from(t) * integral_scale - f64::from(w1)) as f32;
                    origin[axis] =
                        (f64::from(origin[axis]) + sample(index, true) * integral_scale) as f32;
                    origin[axis] =
                        (f64::from(origin[axis]) + sample(index, false) * f64::from(w0)) as f32;
                    origin[axis] =
                        (f64::from(origin[axis]) + sample(index + 1, false) * f64::from(w1)) as f32;
                }
            }
        }
    }
}

fn advance(layer: &Layer, point: &mut Point, seed: u32, begin: i32, end: i32) -> bool {
    if end < point.begin {
        return true;
    }
    // Native FX_UpdateTrailElement deliberately supplies sequence zero here.
    let random_seed = point_seed(seed, point.begin, 0);
    let life = random_integer(layer, "life", random_seed, 17);
    if end >= point.begin + life {
        return false;
    }
    let begin = begin.max(point.begin);
    if begin == end {
        return true;
    }
    graph_delta(
        layer,
        random_seed,
        life,
        begin - point.begin,
        end - point.begin,
        &mut point.origin,
    );
    let seconds = ((end - begin) as f64 * 0.0010000000474974513) as f32;
    let velocity = (f64::from(point.base_vel_z) * 0.0010000000474974513) as f32;
    point.origin[2] =
        (f64::from(point.origin[2]) + f64::from(velocity) * f64::from(seconds)) as f32;
    let gravity = (layer.config["gravity"][0].as_f64().unwrap()
        + layer.config["gravity"][1].as_f64().unwrap()
            * f64::from(layer.sampler.random(random_seed, 15))) as f32;
    let delta = (f64::from(gravity) * 800.0 * f64::from(seconds)) as f32;
    let velocity = (f64::from(velocity) - f64::from(delta)) as f32;
    point.origin[2] =
        (f64::from(point.origin[2]) - f64::from(seconds) * f64::from(delta) * 0.5) as f32;
    point.base_vel_z = (f64::from(velocity) / 0.0010000000474974513)
        .trunc()
        .clamp(-32768.0, 32767.0) as i16;
    true
}

/// Root trail storage advances on the caller's shared one millisecond clock.
/// It consumes no CRT randomness: table randomness is addressed by point seed.
/// The caller counts all point states against the common native pool bound.
pub(crate) struct State {
    name: Value,
    seed: u32,
    layers: Vec<Layer>,
    looping_life: i32,
    pending_loop: bool,
    distance: f32,
    previous_source: [f32; 3],
    now: u32,
    streaming_clock: bool,
}

impl State {
    pub(crate) fn new(
        effect: &Value,
        seed: u32,
        start: u32,
        origin: [f32; 3],
        axis: [[f32; 3]; 3],
    ) -> Result<Self, String> {
        if start != 0 || seed >= 479 || axis != [[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]] {
            return Err(
                "Shared trail proof currently bounds the identity-oriented root at time zero"
                    .into(),
            );
        }
        if origin.iter().any(|v| !v.is_finite()) {
            return Err("Invalid trail root origin".into());
        }
        if let Some(wind) = effect.get("previewWind") {
            let values = wind.as_array().ok_or("Invalid trail preview wind")?;
            if values.len() != 3 || values.iter().any(|v| v.as_f64() != Some(0.)) {
                return Err("Shared trail proof requires zero wind".into());
            }
        }
        let mut layers = Vec::new();
        for (index, element) in effect["elements"]
            .as_array()
            .ok_or("Missing EFX elements")?
            .iter()
            .enumerate()
        {
            if words(element, "editorFlags").contains(&"disabled".into())
                || element.get("trail").is_none()
            {
                continue;
            }
            if words(element, "editorFlags")
                .iter()
                .any(|v| matches!(v.as_str(), "playOnRun" | "playOnDeath" | "playOnTouch"))
            {
                return Err("Child dispatch directly from a trail definition requires separate native proof".into());
            }
            // Original shared-pool controls prove authored wind influence is
            // inert in the explicitly zero-wind preview environment. Keep
            // malformed inputs rejected; nonzero environment wind is guarded
            // above and is not implied by this result.
            if !field(element, "windinfluence", 0.)?.is_finite() {
                return Err("Trail wind influence must be finite".into());
            }
            let runtime = words(element, "flags");
            if runtime.iter().any(|value| value == "runRelToOffset") {
                return Err("Trail run-relative-to-offset frames remain unverified".into());
            }
            let sampler =
                TrailLayer::compile(element, index, effect, effect.get("materialInfo").is_none())?;
            let config = sampler.config();
            if numbers(&config["originRanges"])?.iter().any(|v| *v != 0.)
                || config["life"][0].as_i64().unwrap_or(0) <= 0
            {
                return Err(
                    "Shared trail root requires zero authored origin offset and positive life"
                        .into(),
                );
            }
            if config["looping"] != true
                || config["materialNames"].as_array().map(Vec::len) != Some(1)
            {
                return Err("Shared trail root requires looping single-material layers".into());
            }
            layers.push(Layer {
                sampler,
                config,
                profile: profile(element)?,
                points: Vec::new(),
                sequence: 0,
            });
        }
        if layers.is_empty() || layers.len() > 8 {
            return Err("Shared root requires one to eight trails".into());
        }
        for layer in layers.iter_mut().rev() {
            spawn(layer, seed, 0, origin, 0., 0, false)?;
            spawn(layer, seed, 0, origin, 0., 0, false)?;
        }
        let looping_life = looping_life(effect)?;
        Ok(Self {
            name: effect["name"].clone(),
            seed,
            layers,
            looping_life,
            pending_loop: looping_life != 0,
            distance: 0.,
            previous_source: origin,
            now: 0,
            streaming_clock: false,
        })
    }

    pub(crate) fn compiled(&self) -> Vec<Value> {
        self.layers.iter().map(|v| v.config.clone()).collect()
    }

    pub(crate) fn point_count(&self) -> usize {
        self.layers.iter().map(|v| v.points.len()).sum()
    }

    pub(crate) fn enable_streaming_clock(&mut self) {
        self.streaming_clock = true;
    }

    pub(crate) fn is_complete(&self) -> bool {
        !self.pending_loop && self.point_count() == 0
    }

    pub(crate) fn naturally_infinite(&self) -> bool {
        self.pending_loop && self.looping_life == i32::MAX
    }

    pub(crate) fn lifecycle_samples(&self) -> Vec<u32> {
        let mut samples = Vec::new();
        for layer in &self.layers {
            if let Some(point) = layer.points.first() {
                let seed = point_seed(self.seed, point.begin, point.sequence);
                let middle = point
                    .begin
                    .saturating_add(random_integer(layer, "life", seed, 17) / 2);
                if let Ok(time) = u32::try_from(middle) {
                    if !samples.contains(&time) && samples.len() < 128 {
                        samples.push(time);
                    }
                }
            }
        }
        samples
    }

    pub(crate) fn update(
        &mut self,
        now: u32,
        current_source: [f32; 3],
        stop: bool,
    ) -> Result<(), String> {
        if now != self.now + 1
            || (!self.streaming_clock && now > 60000)
            || now > i32::MAX as u32
            || current_source.iter().any(|v| !v.is_finite())
        {
            return Err(
                "Shared trail update must use the contiguous bounded one millisecond clock".into(),
            );
        }
        if stop {
            self.pending_loop = false;
        }
        let delta = current_source
            .iter()
            .zip(self.previous_source)
            .map(|(a, b)| (a - b) * (a - b))
            .sum::<f32>()
            .sqrt();
        let new_distance = self.distance + delta;
        for layer in self.layers.iter_mut().rev() {
            let requests = if self.pending_loop {
                trails::split_requests(
                    &layer.profile,
                    self.distance,
                    new_distance,
                    now as i32 - 1,
                    now as i32,
                    self.previous_source,
                    current_source,
                )?
            } else {
                Vec::new()
            };
            for request in requests {
                spawn(
                    layer,
                    self.seed,
                    request.begin_msec,
                    request.origin,
                    request.distance,
                    now as i32,
                    false,
                )?;
            }
            let anchor_head = self.pending_loop && now as i32 <= self.looping_life && !stop;
            let mut points = std::mem::take(&mut layer.points);
            let mut removable = true;
            let mut keep_from = 0;
            for (i, point) in points.iter_mut().enumerate() {
                if advance(layer, point, self.seed, now as i32 - 1, now as i32) {
                    removable = false;
                }
                if removable && i > 0 {
                    keep_from = i;
                }
            }
            if removable {
                points.clear();
            } else {
                points.drain(..keep_from);
                if let Some(last) = points.last_mut().filter(|_| anchor_head) {
                    // Native endpoint anchoring repeats origin sampling with
                    // this retained point's original seed and source frame.
                    let seed = point_seed(self.seed, last.begin, last.sequence);
                    last.origin = layer.sampler.apply_spawn_offset(
                        seed,
                        [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                        current_source,
                    );
                    last.distance = self.distance;
                }
            }
            layer.points = points;
        }
        if self.point_count() > 2048 {
            return Err("Shared native trail point pool capacity exceeded".into());
        }
        if now as i32 > self.looping_life || stop {
            self.pending_loop = false;
        }
        self.distance = new_distance;
        self.previous_source = current_source;
        self.now = now;
        Ok(())
    }

    pub(crate) fn draw(&self, now: u32, camera: [f32; 3]) -> Result<Value, String> {
        if now != self.now {
            return Err("Draw time must match shared trail update clock".into());
        }
        let mut draws = Vec::new();
        let mut diagnostics = Vec::new();
        let mut meshes = Vec::new();
        for layer in self.layers.iter().rev() {
            let mut segments = Vec::new();
            for point in &layer.points {
                diagnostics.push(json!({"sourceIndex":layer.config["sourceIndex"],"origin":point.origin,"distance":point.distance,"begin":point.begin,"baseVelZ":point.base_vel_z,"sequence":point.sequence}));
                if (now as i32) < point.begin {
                    continue;
                }
                let random_seed = point_seed(self.seed, point.begin, point.sequence);
                let life = random_integer(layer, "life", random_seed, 17);
                let age = now as i32 - point.begin;
                let v =
                    layer
                        .sampler
                        .visual(point.origin, life, random_seed, age.min(life), camera)?;
                let size = numbers(&v["size"])?;
                let color = std::array::from_fn(|i| v["bgra"][i].as_u64().unwrap() as u8);
                segments.push(DrawSegment {
                    state: SegmentState {
                        position: point.origin,
                        basis: [[0., 1., 0.], [0., 0., 1.]],
                        rotation: v["rotation"].as_f64().unwrap() as f32,
                        size: [size[0], size[1]],
                        texture_u: 0.,
                        color,
                    },
                    normalized_age: age as f32 / life as f32,
                    spawn_distance: point.distance,
                    sequence: point.sequence,
                });
            }
            let mesh = trails::assemble_mesh(&layer.profile, &segments, now as i32)?;
            meshes.push(json!({"sourceIndex":layer.config["sourceIndex"],"vertices":STANDARD.encode(&mesh.vertices),"indices":mesh.indices,"ringCount":mesh.ring_count}));
            if !mesh.indices.is_empty() {
                draws.push(json!({"texture":layer.config["materialNames"][0],"vertices":STANDARD.encode(mesh.vertices),"indices":mesh.indices}));
            }
        }
        Ok(
            json!({"name":self.name,"time":now,"draws":draws,"particles":diagnostics,"trailMeshes":meshes}),
        )
    }
}
