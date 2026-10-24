//! Fresh standalone preview simulator. No game/editor process or native runtime.
//!
//! Algorithms adapted from xoxor4d/iw3xo-radiant, pinned commit
//! 665347c2e90671068088ddc4cd58291194cf9423, src/fx_system:
//! fx_convert.cpp (adaptive sampling), fx_random.cpp (table indexing),
//! fx_system.cpp / fx_update.cpp (birth/life scheduling), fx_sort.cpp
//! (persistent insertion), fx_draw.cpp (visual state and billboard quads).
//! MIT attribution is retained in this app's ATTRIBUTION.md.
//!
//! This is a strict, bounded quad/child playback subset. Unsupported visual
//! behaviour returns an error for the entire effect, never a partial preview.
//! Sound definitions retain their slots but allocate no visual particles.

use crate::fx::frames::{self, Axis};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

#[path = "admission.rs"]
mod dense_vehicle_admission;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone)]
struct Graph {
    scale: f32,
    curves: [Vec<Vec<f32>>; 2],
}

impl Graph {
    fn read(element: &Value, key: &str, dimensions: usize) -> Result<Self, String> {
        let raw = crate::fx::loader::graph(element, key, dimensions)?;
        let mut curves: [Vec<Vec<f32>>; 2] = [Vec::new(), Vec::new()];
        let signed = key.starts_with("velGraph") || key == "rotGraph";
        let (minimum, maximum) = if signed { (-0.5, 0.5) } else { (0.0, 1.0) };
        for endpoint in 0..2 {
            for point in raw["curves"][endpoint].as_array().ok_or("missing curve")? {
                let mut values = Vec::new();
                for (channel, value) in point.as_array().ok_or("missing key")?.iter().enumerate() {
                    let mut number = value.as_f64().ok_or("invalid graph number")? as f32;
                    if channel > 0 {
                        number = number.clamp(minimum, maximum);
                    }
                    values.push(number);
                }
                curves[endpoint].push(values);
            }
        }
        Ok(Self {
            scale: raw["scale"].as_f64().ok_or("missing scale")? as f32,
            curves,
        })
    }

    fn at(&self, endpoint: usize, time: f32, channel: usize) -> f32 {
        sample_curve(&self.curves[endpoint], time, channel)
    }
}

fn sample_curve(keys: &[Vec<f32>], time: f32, channel: usize) -> f32 {
    if time <= keys[0][0] {
        return keys[0][channel];
    }
    for pair in keys.windows(2) {
        if time <= pair[1][0] {
            let left = &pair[0];
            let right = &pair[1];
            if left.len() == 2 {
                // FxCurve_Interpolate1d keeps this multiply/divide/add chain in
                // native x87 precision, then stores the scalar result as f32.
                // The 3D colour interpolator has a separate expression order.
                return (((time as f64 - left[0] as f64)
                    * (right[channel] as f64 - left[channel] as f64)
                    / (right[0] as f64 - left[0] as f64))
                    + left[channel] as f64) as f32;
            }
            return left[channel]
                + (right[channel] - left[channel]) * ((time - left[0]) / (right[0] - left[0]));
        }
    }
    keys.last().unwrap()[channel]
}

// FX_MaxErrorForIntervalCount evaluates errors at the original knots between
// evenly spaced samples. Search order and the native 0.02 cutoff matter.
fn interval_error(keys: &[Vec<f32>], intervals: usize) -> f32 {
    let mut maximum = 0.0_f32;
    for key in keys.iter().skip(1).take(keys.len().saturating_sub(2)) {
        let index = (key[0] * intervals as f32).floor() as usize;
        let start = index as f32 / intervals as f32;
        let end = (index + 1).min(intervals) as f32 / intervals as f32;
        if end <= start {
            continue;
        }
        for channel in 1..key.len() {
            let a = sample_curve(keys, start, channel);
            let b = sample_curve(keys, end, channel);
            let reconstructed = a + (b - a) * ((key[0] - start) / (end - start));
            maximum = maximum.max((key[channel] - reconstructed).abs());
        }
    }
    maximum
}

fn interval_count(curves: &[&Vec<Vec<f32>>], limit: usize) -> usize {
    let mut best_error = f32::MAX;
    let mut best = 1;
    for intervals in 1..=limit.max(1) {
        let error = curves
            .iter()
            .map(|keys| interval_error(keys, intervals))
            .fold(0.0_f32, f32::max);
        if error < best_error {
            best = intervals;
            best_error = error - 0.02;
            if best_error <= 0.0 {
                break;
            }
        }
    }
    best
}

fn tokens(element: &Value, field: &str) -> Vec<String> {
    element[field]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter(|word| !matches!(*word, "{" | "}" | ";"))
                .map(|word| word.trim_matches('"').to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn scalar(element: &Value, field: &str, default: f32) -> Result<f32, String> {
    let values = tokens(element, field);
    if values.is_empty() {
        return Ok(default);
    }
    values[0]
        .parse::<f32>()
        .map_err(|_| format!("invalid {field}"))
}

fn range(element: &Value, field: &str, canonical: bool) -> Result<[f32; 2], String> {
    let values = tokens(element, field);
    if values.is_empty() {
        return Ok([0.0, 0.0]);
    }
    if values.len() != 2 {
        return Err(format!("{field} must be a pair"));
    }
    let mut pair = [
        values[0].parse::<f32>().map_err(|_| field)?,
        values[1].parse::<f32>().map_err(|_| field)?,
    ];
    if canonical && pair[1] < 0.0 {
        pair[0] += pair[1];
        pair[1] = -pair[1];
    }
    Ok(pair)
}

fn flags(element: &Value, field: &str) -> BTreeSet<String> {
    tokens(element, field).into_iter().collect()
}

fn integer_range(element: &Value, field: &str, canonical: bool) -> Result<[i64; 2], String> {
    let values = tokens(element, field);
    if values.len() != 2 {
        return Err(format!("{field} must be an integer pair"));
    }
    let mut pair = [
        values[0]
            .parse::<i32>()
            .map_err(|_| format!("invalid integer {field}"))? as i64,
        values[1]
            .parse::<i32>()
            .map_err(|_| format!("invalid integer {field}"))? as i64,
    ];
    if canonical && pair[1] < 0 {
        pair[0] += pair[1];
        pair[1] = -pair[1];
    }
    Ok(pair)
}

fn random(seed: usize, offset: usize) -> f32 {
    f32::from_bits(RANDOM_BITS[seed + offset])
}

fn random_word(seed: usize, offset: usize) -> i64 {
    (RANDOM_BITS[seed + offset] & 0xffff) as i64
}

fn integer_random(pair: [i64; 2], seed: usize, offset: usize) -> i64 {
    pair[0] + (((pair[1] + 1) * random_word(seed, offset)) >> 16)
}

#[derive(Clone)]
struct VisualSample {
    colors: [[u8; 4]; 2],
    // rotation rate/accumulation, half-width, half-height, scale; [base, amplitude].
    values: [[f32; 5]; 2],
}

#[derive(Clone)]
struct Definition {
    source: usize,
    name: String,
    kind: String,
    children: Vec<String>,
    death: Option<String>,
    emission: Option<String>,
    // Original empty-editor trace never hits. Keep the active reference as
    // metadata without requiring or dispatching its unreachable child assets.
    impact: Option<String>,
    emit_distance: [f32; 2],
    emit_variance: [f32; 2],
    sound: bool,
    runtime: BTreeSet<String>,
    bits: u32,
    looping: bool,
    spawn: [i64; 2],
    delay: [i64; 2],
    life: [i64; 2],
    origin: [[f32; 2]; 3],
    offset_radius: [f32; 2],
    offset_height: [f32; 2],
    initial_rotation: [f32; 2],
    spawn_angles: [[f32; 2]; 3],
    angular_velocity: [[f32; 2]; 3],
    gravity: [f32; 2],
    // Authored influence remains meaningful metadata. This preview preview
    // supplies the native on-disk zero wind vector, so its force is exactly zero.
    wind_influence: f32,
    fade_in: [f32; 2],
    fade_out: [f32; 2],
    sort: u8,
    atlas: [u32; 7],
    materials: Vec<String>,
    velocity: Vec<[f32; 24]>,
    visual: Vec<VisualSample>,
    lighting_fraction: u8,
    preview_packed_lighting: Option<u16>,
}

fn packed_color(
    graphs: &BTreeMap<String, Graph>,
    editor: &BTreeSet<String>,
    time: f32,
    endpoint: usize,
) -> [u8; 4] {
    let color_graph = &graphs["colorGraph"];
    let alpha_graph = &graphs["alphaGraph"];
    let color_endpoint = usize::from(endpoint == 1 && editor.contains("useRandColor"));
    let alpha_endpoint = usize::from(endpoint == 1 && editor.contains("useRandAlpha"));
    let mut rgba = [
        color_graph.at(color_endpoint, time, 1),
        color_graph.at(color_endpoint, time, 2),
        color_graph.at(color_endpoint, time, 3),
        alpha_graph.at(alpha_endpoint, time, 1),
    ];
    if editor.contains("modColorByAlpha") {
        for channel in 0..3 {
            rgba[channel] *= rgba[3];
        }
        rgba[3] = 1.0;
    }
    let bytes = rgba.map(|number| (number * 255.0 + 0.5).floor().clamp(0.0, 255.0) as u8);
    [bytes[2], bytes[1], bytes[0], bytes[3]]
}

fn compile(
    element: &Value,
    source: usize,
    effect: &Value,
    metadata_optional: bool,
) -> Result<Option<Definition>, String> {
    let mut editor = flags(element, "editorFlags");
    if editor.contains("disabled") {
        return Ok(None);
    }
    if effect["version"].as_u64() == Some(1) {
        editor.insert("useBackCompatVel".to_string());
    }
    let runtime = flags(element, "flags");
    let authored_fraction = scalar(element, "lightingFrac", 0.0)?;
    if !authored_fraction.is_finite() || !(0.0..=1.0).contains(&authored_fraction) {
        return Err(format!("element {source}: invalid lightingFrac range"));
    }
    // Original conversion truncates after multiplying the authored f32 by 255
    // in x87 precision. A rounded byte would change 0.5 from 127 to 128.
    let lighting_fraction = (authored_fraction as f64 * 255.0) as u8;
    let preview_packed_lighting = effect
        .get("previewPackedLighting")
        .map(|value| {
            value
                .as_u64()
                .filter(|value| *value <= u16::MAX as u64)
                .map(|value| value as u16)
                .ok_or_else(|| {
                    "previewPackedLighting must be an explicit unsigned 16-bit integer".to_string()
                })
        })
        .transpose()?;
    let sound = element.get("sound").is_some();
    let kind = [
        "billboardSprite",
        "orientedSprite",
        "tail",
        "line",
        "cloud",
        "model",
        "light",
        "spotLight",
        "runner",
        "sound",
        "decal",
    ]
    .into_iter()
    .find(|kind| element.get(*kind).is_some())
    .ok_or_else(|| format!("element {source}: unsupported visual type"))?
    .to_string();
    let native_visual_noop = sound || kind == "decal";
    let name = tokens(element, "name")
        .first()
        .cloned()
        .unwrap_or_else(|| format!("element {source}"));
    let reject = |reason: String| format!("element {source} ({name}): {reason}");
    let allowed_editor = [
        "looping",
        "useRandColor",
        "useRandAlpha",
        "useRandSize0",
        "useRandSize1",
        "useRandScale",
        "useRandRotDelta",
        "modColorByAlpha",
        "useRandVel0",
        "useRandVel1",
        "useBackCompatVel",
        "absVel0",
        "absVel1",
        "playOnDeath",
        "playOnRun",
        "playOnTouch",
        // Bounds shape and item-clip selection do not affect this preview's
        // culling-disabled, collision-free trajectories. Collision itself is
        // still rejected below rather than pretending that a scene exists.
        "boundingSphere",
        "useItemClip",
    ];
    let allowed_runtime = [
        "spawnRelative",
        "spawnFrustumCull",
        "spawnOffsetNone",
        "spawnOffsetSphere",
        "spawnOffsetCylinder",
        "runRelToWorld",
        "runRelToSpawn",
        "runRelToEffect",
        "runRelToOffset",
        "nonUniformScale",
        "drawWithViewModel",
        "hasReflection",
        // Original standalone editor playback preserves these classification
        // bits without applying game content filtering or world flame logic.
        "isMatureContent",
        "flamechunk",
        // This preview has no AI/visibility consumer. The original editor's
        // opacity-volume writer changes that service, but not visual packets.
        "blocksSight",
        // The controlled preview has no fog. Preserve the native bit while
        // allowing this flag; do not claim scene fog behaviour is implemented.
        "drawPastFog",
        // Stock EffectsEd3's trace callback (0x406610) always clears its trace
        // and returns fraction 1. Preserve authored collision bits while using
        // that verified editor-preview policy, rather than inventing a world.
        "useCollision",
        "dieOnTouch",
    ];
    if !native_visual_noop {
        for flag in &editor {
            if !allowed_editor.contains(&flag.as_str()) {
                return Err(reject(format!("unsupported editor flag {flag}")));
            }
        }
        for flag in &runtime {
            if !allowed_runtime.contains(&flag.as_str()) {
                return Err(reject(format!("unsupported runtime flag {flag}")));
            }
        }
        if kind != "model" && lighting_fraction > 0 && preview_packed_lighting.is_none() {
            return Err(reject(
                "unsupported nonzero lightingFrac without explicit previewPackedLighting".into(),
            ));
        }
        // Identity emitter makes world/spawn/effect frames coincide. Native
        // FX_GetOrientation ignores spawnAngle*/angleVel*: those fields belong
        // to model placement, not these quad or unrotated runner frames.
    }
    let looping = editor.contains("looping");
    if runtime.contains("runRelToOffset")
        && !runtime.contains("spawnOffsetSphere")
        && !runtime.contains("spawnOffsetCylinder")
    {
        return Err(reject(
            "run-relative-to-offset requires a sphere/cylinder offset".into(),
        ));
    }
    let mut spawn = integer_range(
        element,
        if looping {
            "spawnLooping"
        } else {
            "spawnOneShot"
        },
        false,
    )?;
    if looping {
        spawn[0] = spawn[0].max(1);
        if spawn[1] == 0 {
            spawn[1] = 2147483647;
        }
    }
    let life = integer_range(element, "lifeSpanMsec", true)?;
    let delay = integer_range(element, "spawnDelayMsec", true)?;
    if (life[0] < 0 && kind != "runner" && !native_visual_noop) || spawn[0] < 0 || spawn[1] < 0 {
        return Err(reject("invalid life/delay/spawn range".into()));
    }
    if kind == "spotLight" {
        // EffectsEd has one selected system spotlight, rather than the omni
        // list. Until replacement/overlap has separate native evidence, admit
        // only a schedule that can keep at most one spotlight alive.
        let overlap = if looping {
            life[0] + life[1] + delay[1] > spawn[0]
        } else {
            spawn[0] + spawn[1] > 1
        };
        if overlap {
            return Err(reject(
                "overlapping spotlight replacement policy remains unverified".into(),
            ));
        }
    }
    let origin = [
        range(element, "spawnOrgX", true)?,
        range(element, "spawnOrgY", true)?,
        range(element, "spawnOrgZ", true)?,
    ];
    let mut initial_rotation = range(element, "initialRot", false)?;
    initial_rotation = initial_rotation.map(|value| value * 0.017453292);
    let gravity = range(element, "gravity", false)?.map(|value| value * 0.0099999998);
    let materials =
        if native_visual_noop || matches!(kind.as_str(), "runner" | "light" | "spotLight") {
            Vec::new()
        } else {
            tokens(element, &kind)
        };
    let mut atlas = [0_u32; 7];
    if !sound && kind != "model" && !materials.is_empty() {
        let info = &effect["materialInfo"][&materials[0]];
        // Empty-timeline preflight may precede asset resolution. Its placeholder
        // atlas is never used for a rendered frame or accepted as an asset pass.
        let columns = info["atlasColumns"]
            .as_u64()
            .or(if metadata_optional { Some(1) } else { None })
            .ok_or_else(|| reject("missing material atlas metadata".into()))?
            as u32;
        let rows = info["atlasRows"]
            .as_u64()
            .or(if metadata_optional { Some(1) } else { None })
            .ok_or_else(|| reject("missing material atlas metadata".into()))?
            as u32;
        if columns == 0
            || rows == 0
            || !columns.is_power_of_two()
            || !rows.is_power_of_two()
            || columns * rows > 256
        {
            return Err(reject("invalid material atlas dimensions".into()));
        }
        for material in &materials {
            let other = &effect["materialInfo"][material];
            if !metadata_optional
                && (other["atlasColumns"].as_u64() != Some(columns as u64)
                    || other["atlasRows"].as_u64() != Some(rows as u64))
            {
                return Err(reject("material atlas dimensions disagree".into()));
            }
        }
        let atlas_flags = tokens(element, "atlasBehavior");
        let mut behavior = 0;
        for flag in atlas_flags {
            match flag.as_str() {
                "startFixed" => behavior &= !3,
                "startRandom" => behavior = (behavior & !3) | 1,
                "startIndexed" => behavior = (behavior & !3) | 2,
                "playOverLife" => behavior |= 4,
                "loopOnlyNTimes" => behavior |= 8,
                _ => return Err(reject(format!("unsupported atlas flag {flag}"))),
            }
        }
        atlas = [
            behavior,
            scalar(element, "atlasIndex", 0.0)? as u32 & 255,
            scalar(element, "atlasFps", 0.0)? as u32 & 255,
            (scalar(element, "atlasLoopCount", 0.0)? as u32 + 1) & 255,
            columns.trailing_zeros(),
            rows.trailing_zeros(),
            columns * rows,
        ];
    }
    let mut bits = 0;
    for (flag, value) in [
        ("spawnRelative", 2),
        ("spawnFrustumCull", 4),
        ("runRelToSpawn", 64),
        ("runRelToEffect", 128),
        ("runRelToOffset", 192),
        ("spawnOffsetSphere", 16),
        ("spawnOffsetCylinder", 32),
        ("drawPastFog", 1024),
        ("drawWithViewModel", 2048),
        ("nonUniformScale", 0x10000000),
        ("hasReflection", 0x40000000),
        ("isMatureContent", 0x80000000),
        ("flamechunk", 0x20000000),
        ("blocksSight", 0x1000),
        ("useCollision", 0x100),
        ("dieOnTouch", 0x200),
    ] {
        if runtime.contains(flag) {
            bits |= value;
        }
    }
    if gravity != [0.0, 0.0] {
        bits |= 0x04000000;
    }
    let mut graphs = BTreeMap::new();
    for key in [
        "colorGraph",
        "alphaGraph",
        "rotGraph",
        "sizeGraph0",
        "sizeGraph1",
        "scaleGraph",
        "velGraph0X",
        "velGraph0Y",
        "velGraph0Z",
        "velGraph1X",
        "velGraph1Y",
        "velGraph1Z",
    ] {
        graphs.insert(
            key.to_string(),
            Graph::read(element, key, if key == "colorGraph" { 3 } else { 1 })?,
        );
    }
    let limit = (((life[0] as i64 + life[1] as i64 / 2) / 100).clamp(0, 80)) as usize;
    let mut velocity_curves = Vec::new();
    for group in 0..2 {
        for axis in ["X", "Y", "Z"] {
            let graph = &graphs[&format!("velGraph{group}{axis}")];
            velocity_curves.push(&graph.curves[0]);
            velocity_curves.push(&graph.curves[1]);
        }
    }
    let velocity_intervals = interval_count(&velocity_curves, limit);
    let mut visual_curves = vec![
        &graphs["colorGraph"].curves[0],
        &graphs["alphaGraph"].curves[0],
        &graphs["sizeGraph0"].curves[0],
    ];
    if !matches!(kind.as_str(), "model" | "cloud" | "light" | "spotLight") {
        visual_curves.push(&graphs["rotGraph"].curves[0]);
    }
    for (flag, key) in [
        ("useRandColor", "colorGraph"),
        ("useRandAlpha", "alphaGraph"),
        ("useRandSize0", "sizeGraph0"),
        ("useRandRotDelta", "rotGraph"),
    ] {
        if editor.contains(flag)
            && !(matches!(kind.as_str(), "model" | "cloud" | "light" | "spotLight")
                && key == "rotGraph")
        {
            visual_curves.push(&graphs[key].curves[1]);
        }
    }
    // Native WaW tails and lines use sizeGraph1 as their independent length,
    // even when the NONUNIFORM_SCALE runtime bit is absent. Ordinary sprite
    // height uses sizeGraph0 unless that bit explicitly enables sizeGraph1.
    let independent_height = !matches!(kind.as_str(), "model" | "light" | "spotLight")
        && (matches!(kind.as_str(), "tail" | "line" | "cloud")
            || runtime.contains("nonUniformScale"));
    if independent_height {
        visual_curves.push(&graphs["sizeGraph1"].curves[0]);
        if editor.contains("useRandSize1") {
            visual_curves.push(&graphs["sizeGraph1"].curves[1]);
        }
    }
    if matches!(kind.as_str(), "cloud" | "model") {
        visual_curves.push(&graphs["scaleGraph"].curves[0]);
        if editor.contains("useRandScale") {
            visual_curves.push(&graphs["scaleGraph"].curves[1]);
        }
    }
    if kind == "model" {
        // Native model definitions route only the scale channel.
        visual_curves = vec![&graphs["scaleGraph"].curves[0]];
        if editor.contains("useRandScale") {
            visual_curves.push(&graphs["scaleGraph"].curves[1]);
        }
    }
    let visual_intervals = interval_count(&visual_curves, limit);
    let mut velocity = Vec::<[f32; 24]>::new();
    for point in 0..=velocity_intervals {
        let mut values = [0.0; 24];
        for world in 0..2 {
            let offset = world * 12;
            let used = [
                editor.contains("absVel0") == (world == 1),
                editor.contains("absVel1") == (world == 1),
            ];
            let routed_random = [
                used[0] && editor.contains("useRandVel0"),
                used[1] && editor.contains("useRandVel1"),
            ];
            let mut random_additions = [[0.0_f32; 3]; 2];
            for group in 0..2 {
                if !used[group] {
                    continue;
                }
                for (axis, letter) in ["X", "Y", "Z"].iter().enumerate() {
                    let graph = &graphs[&format!("velGraph{group}{letter}")];
                    let scale = graph.scale * (1.0 / (velocity_intervals as f32 * 1000.0));
                    let a = graph.at(0, point as f32 / velocity_intervals as f32, 1) * scale;
                    let b = graph.at(1, point as f32 / velocity_intervals as f32, 1) * scale;
                    values[offset + axis] += a;
                    let subtraction_group = if group == 1 && editor.contains("useBackCompatVel") {
                        0
                    } else {
                        group
                    };
                    if routed_random[subtraction_group] {
                        values[offset + 3 + axis] -= a;
                    }
                    if routed_random[group] {
                        random_additions[group][axis] = b;
                    }
                }
            }
            // Native conversion subtracts all enabled base endpoints before
            // adding either random endpoint. Interleaving those operations
            // changes real two-graph amplitudes by several float bits.
            for group in 0..2 {
                if routed_random[group] {
                    for axis in 0..3 {
                        values[offset + 3 + axis] += random_additions[group][axis];
                    }
                }
            }
            if point > 0 {
                let previous = &velocity[point - 1];
                for axis in 0..3 {
                    values[offset + 6 + axis] = previous[offset + 6 + axis]
                        + (previous[offset + axis] + values[offset + axis]) * 0.5;
                    values[offset + 9 + axis] = previous[offset + 9 + axis]
                        + (previous[offset + 3 + axis] + values[offset + 3 + axis]) * 0.5;
                }
            }
        }
        velocity.push(values);
    }
    // FX_SampleVelocityInFrame uses integrated displacement against an epsilon
    // relative to the source scale. A merely nonzero curve is not sufficient.
    for world in 0..2 {
        let mut scale_squared = 0.0;
        for group in 0..2 {
            let absolute = editor.contains(if group == 0 { "absVel0" } else { "absVel1" });
            if absolute != (world == 1) {
                continue;
            }
            for letter in ["X", "Y", "Z"] {
                let scale = graphs[&format!("velGraph{group}{letter}")].scale
                    / (velocity_intervals as f32 * 1000.0);
                scale_squared += scale * scale;
            }
        }
        let epsilon = scale_squared * 0.000001;
        if velocity.iter().any(|sample| {
            let offset = world * 12;
            let base_squared = sample[offset + 6..offset + 9]
                .iter()
                .map(|value| value * value)
                .sum::<f32>();
            let amplitude_squared = sample[offset + 9..offset + 12]
                .iter()
                .map(|value| value * value)
                .sum::<f32>();
            base_squared > epsilon || amplitude_squared > epsilon
        }) {
            bits |= if world == 0 { 0x01000000 } else { 0x02000000 };
        }
    }
    let rotation_scale = ((graphs["rotGraph"].scale as f64 * 0.01745329238474369_f64)
        / (visual_intervals as f64 * 1000.0)) as f32;
    let mut visual = Vec::<VisualSample>::new();
    for point in 0..=visual_intervals {
        let time = point as f32 / visual_intervals as f32;
        let mut values = [[0.0; 5]; 2];
        values[0][0] = graphs["rotGraph"].at(0, time, 1) * rotation_scale;
        if editor.contains("useRandRotDelta") {
            values[1][0] = graphs["rotGraph"].at(1, time, 1) * rotation_scale - values[0][0];
        }
        if point > 0 {
            for endpoint in 0..2 {
                // Native 0x44626b/0x4462bc keeps the trapezoid sum in x87
                // precision until the accumulated sample is stored as f32.
                values[endpoint][1] = (visual[point - 1].values[endpoint][1] as f64
                    + (visual[point - 1].values[endpoint][0] as f64 + values[endpoint][0] as f64)
                        * 0.5) as f32;
            }
        }
        for axis in 0..2 {
            if kind == "model" || (matches!(kind.as_str(), "light" | "spotLight") && axis == 1) {
                // Native point/spot lights compile a radius only; height is zero.
                continue;
            }
            let second = axis == 1 && independent_height;
            let key = if second { "sizeGraph1" } else { "sizeGraph0" };
            let flag = if second {
                "useRandSize1"
            } else {
                "useRandSize0"
            };
            let graph = &graphs[key];
            values[0][2 + axis] = graph.at(0, time, 1) * (graph.scale * 0.5);
            if editor.contains(flag) {
                values[1][2 + axis] =
                    graph.at(1, time, 1) * (graph.scale * 0.5) - values[0][2 + axis];
            }
        }
        if matches!(kind.as_str(), "model" | "cloud" | "light" | "spotLight") {
            // Clouds and lights have no sprite rotation channel.
            values[0][0] = 0.0;
            values[0][1] = 0.0;
            values[1][0] = 0.0;
            values[1][1] = 0.0;
            if matches!(kind.as_str(), "cloud" | "model") {
                let graph = &graphs["scaleGraph"];
                values[0][4] = graph.at(0, time, 1) * graph.scale;
                if editor.contains("useRandScale") {
                    values[1][4] = graph.at(1, time, 1) * graph.scale - values[0][4];
                }
            }
        }
        visual.push(VisualSample {
            colors: if kind == "model" {
                [[255; 4]; 2]
            } else {
                [
                    packed_color(&graphs, &editor, time, 0),
                    packed_color(&graphs, &editor, time, 1),
                ]
            },
            values,
        });
    }
    let has_callback = active_child(element, &editor, "playOnDeath", "fxOnDeath").is_some()
        || active_child(element, &editor, "playOnRun", "emission").is_some()
        || active_child(element, &editor, "playOnTouch", "fxOnImpact").is_some();
    if life[0] == 0 && has_callback && (kind != "billboardSprite" || atlas[0] & 4 != 0) {
        return Err("Zero-life callback visuals are proved only for billboard sprites without play-over-life atlas animation".into());
    }
    Ok(Some(Definition {
        source,
        name,
        kind,
        children: tokens(element, "runner")
            .into_iter()
            .map(|name| canonical(&name))
            .collect(),
        death: active_child(element, &editor, "playOnDeath", "fxOnDeath"),
        emission: active_child(element, &editor, "playOnRun", "emission"),
        impact: active_child(element, &editor, "playOnTouch", "fxOnImpact"),
        emit_distance: range(element, "emitDist", true)?,
        emit_variance: range(element, "emitDistVariance", true)?,
        sound,
        runtime,
        bits,
        looping,
        spawn,
        delay,
        life,
        origin,
        offset_radius: range(element, "spawnOffsetRadius", true)?,
        offset_height: range(element, "spawnOffsetHeight", true)?,
        initial_rotation,
        spawn_angles: ["spawnAnglePitch", "spawnAngleYaw", "spawnAngleRoll"]
            .map(|key| range(element, key, false).map(|pair| pair.map(|v| v * 0.017453292)))
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .try_into()
            .unwrap(),
        angular_velocity: ["angleVelPitch", "angleVelYaw", "angleVelRoll"]
            .map(|key| range(element, key, false).map(|pair| pair.map(|v| v * 0.000017453292)))
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .try_into()
            .unwrap(),
        gravity,
        wind_influence: scalar(element, "windinfluence", 0.0)?,
        fade_in: range(element, "fadeInRange", true)?,
        fade_out: range(element, "fadeOutRange", true)?,
        sort: scalar(element, "sortOrder", 5.0)?.clamp(0.0, 255.0) as u8,
        atlas,
        materials,
        velocity,
        visual,
        lighting_fraction,
        preview_packed_lighting,
    }))
}

fn definition_json(definition: &Definition) -> Value {
    let mut visual_bytes = Vec::<u8>::new();
    for sample in &definition.visual {
        for endpoint in 0..2 {
            visual_bytes.extend_from_slice(&sample.colors[endpoint]);
            for value in sample.values[endpoint] {
                visual_bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
    let hexadecimal = visual_bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    json!({"sourceIndex": definition.source, "name": definition.name,
        "kind": definition.kind,
        "deathChild": definition.death, "emissionChild": definition.emission,
        "impactChild": definition.impact,
        "emitDistance": definition.emit_distance, "emitVariance": definition.emit_variance,
        "flags": definition.bits, "spawn": definition.spawn, "delay": definition.delay,
        "life": definition.life, "originRanges": definition.origin.concat(),
        "offsetRadius": definition.offset_radius, "offsetHeight": definition.offset_height,
        "initialRotation": definition.initial_rotation, "gravity": definition.gravity,
        "spawnAngles": definition.spawn_angles, "angularVelocity": definition.angular_velocity,
        "windInfluence": definition.wind_influence,
        "atlas": definition.atlas, "sortOrder": definition.sort,
        "velocityIntervals": definition.velocity.len() - 1,
        "visualIntervals": definition.visual.len() - 1,
        "velocitySamples": definition.velocity, "visualSampleBytes": hexadecimal,
        "materialNames": definition.materials})
}

#[derive(Clone)]
struct MotionState {
    age: i64,
    previous_age: i64,
    position: [f32; 3],
    previous_position: [f32; 3],
    gravity_velocity: f32,
}

#[derive(Clone)]
struct CloudMotion {
    age: i64,
    previous_age: i64,
    stored: [f32; 3],
    world: [f32; 3],
    // Native emission consumes update->posWorld before its inverse-frame
    // storage is transformed again by drawing. Those roundings are distinct.
    update_world: [f32; 3],
    previous_world: [f32; 3],
    base_velocity: [f32; 3],
}

#[derive(Clone)]
struct Particle {
    effect: usize,
    layer: usize,
    sequence: usize,
    scheduled: i64,
    birth: i64,
    life: i64,
    seed: usize,
    origin: [f32; 3],
    frame_origin: [f32; 3],
    frame_axis: Axis,
    emit_residual: u8,
    motion: Option<MotionState>,
    cloud_motion: Option<CloudMotion>,
    // Only the moving-cloud fixture adapter sets this. Birth placement and
    // the effect's current run-relative frame are distinct native inputs.
    cloud_current_frame: Option<crate::fx::cloud_motion::Frame>,
    // The preview environment is fixed for the complete effect allocation.
    // Keep its packed value at birth; particle movement never resamples it.
    packed_lighting: u16,
    // Original particle +0x0a starts at 255. A collision-free local stop
    // records its quantized lifetime fraction on the first positive update.
    at_rest: u8,
}

fn spawn(
    definitions: &[Definition],
    layer: usize,
    sequence: usize,
    scheduled: i64,
    effect_seed: u32,
) -> Particle {
    let definition = &definitions[layer];
    let begin = scheduled + definition.delay[0] as i64;
    let delay_seed = (effect_seed as i64 + begin + sequence as i64 * 296).rem_euclid(479) as usize;
    let birth = begin
        + if definition.delay[1] != 0 {
            ((definition.delay[1] as i64 + 1) * random_word(delay_seed, 18)) >> 16
        } else {
            0
        };
    // WaW keeps the full request sequence while sampling spawn delay, then
    // truncates it to the stored byte before choosing lifespan/origin randomness.
    // The distinction matters at birth 256, even before the first visual draw.
    let stored_sequence = sequence & 255;
    let seed = (effect_seed as i64 + birth + stored_sequence as i64 * 296).rem_euclid(479) as usize;
    let life = integer_random(definition.life, seed, 17);
    let origin = std::array::from_fn(|axis| {
        // Original 44E7E0 retains product + base until one f32 store.
        crate::fx::offset_math::sample_range(definition.origin[axis], random(seed, 6 + axis))
    });
    Particle {
        effect: 0,
        layer,
        sequence: stored_sequence,
        scheduled,
        birth,
        life,
        seed,
        origin,
        frame_origin: [0.0; 3],
        frame_axis: frames::IDENTITY,
        emit_residual: 0,
        motion: None,
        cloud_motion: None,
        cloud_current_frame: None,
        packed_lighting: definition.preview_packed_lighting.unwrap_or(0x00ff),
        at_rest: 255,
    }
}

fn orientation_axis(definition: &Definition, particle: &Particle) -> Axis {
    if definition.bits & 0xC0 == 0 {
        frames::IDENTITY
    } else if definition.bits & 0xC0 == 0x80 {
        particle
            .cloud_current_frame
            .map(|frame| frame.axis)
            .unwrap_or(particle.frame_axis)
    } else {
        particle.frame_axis
    }
}

/// Native sphere offsets are world-directed; cylinders follow the emitter's
/// forward axis, with their circular section in the left/up plane. Offsets are
/// independent of the spawnRelative flag, which affects the authored box only.
fn spawn_offset(definition: &Definition, seed: usize, axis: Axis) -> [f32; 3] {
    if definition.bits & 0x30 != 0 {
        return crate::fx::offset_math::apply_offset(
            [0.0; 3],
            axis,
            definition.runtime.contains("spawnOffsetSphere"),
            definition.offset_radius,
            definition.offset_height,
            random(seed, 9),
            random(seed, 10),
            random(seed, 11),
        );
    }
    [0.0; 3]
}

fn offset_axis(offset: [f32; 3], sphere: bool) -> Axis {
    if sphere {
        return crate::fx::offset_math::sphere_basis(offset);
    }
    // Original Vec3Normalize stores dot, length and reciprocal separately.
    // Dividing by length instead changes the repeatedly reused frame's bits.
    let normalize = |vector: [f32; 3]| {
        let squared = ((vector[0] as f64 * vector[0] as f64 + vector[1] as f64 * vector[1] as f64)
            + vector[2] as f64 * vector[2] as f64) as f32;
        let length = (squared as f64).sqrt() as f32;
        let reciprocal = if length == 0.0 {
            1.0
        } else {
            (1.0_f64 / length as f64) as f32
        };
        (vector.map(|value| value * reciprocal), length)
    };
    let (normalized, length) = normalize(offset);
    let forward = if length == 0.0 {
        [1.0, 0.0, 0.0]
    } else {
        normalized
    };
    if sphere {
        // Vec3Basis_RightHanded places PerpendicularVector in the third axis.
        let basis = frames::emission_axis(forward).expect("normalized offset axis");
        return [forward, basis[2].map(|value| -value), basis[1]];
    }
    let up = if forward[2].abs() < 0.9990000128746033 {
        [0.0, 0.0, 1.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    let cross = |a: [f32; 3], b: [f32; 3]| {
        [
            (a[1] as f64 * b[2] as f64 - a[2] as f64 * b[1] as f64) as f32,
            (a[2] as f64 * b[0] as f64 - a[0] as f64 * b[2] as f64) as f32,
            (a[0] as f64 * b[1] as f64 - a[1] as f64 * b[0] as f64) as f32,
        ]
    };
    let (left, _) = normalize(cross(up, forward));
    [forward, left, cross(forward, left)]
}

fn initialize_offset_particle(
    definition: &Definition,
    particle: &mut Particle,
    parent_origin: [f32; 3],
    parent_axis: Axis,
) {
    let parent = crate::fx::cloud_motion::Frame {
        origin: parent_origin,
        axis: parent_axis,
    };
    let local = std::array::from_fn(|component| {
        crate::fx::offset_math::sample_range(
            definition.origin[component],
            random(particle.seed, 6 + component),
        )
    });
    let box_frame = crate::fx::cloud_motion::Frame {
        origin: parent_origin,
        axis: if definition.runtime.contains("spawnRelative") {
            parent_axis
        } else {
            frames::IDENTITY
        },
    };
    let box_world = crate::fx::cloud_motion::spawn_to_world(box_frame, local);
    let sphere = definition.runtime.contains("spawnOffsetSphere");
    let apply_offset = |base| {
        crate::fx::offset_math::apply_offset(
            base,
            parent_axis,
            sphere,
            definition.offset_radius,
            definition.offset_height,
            random(particle.seed, 9),
            random(particle.seed, 10),
            random(particle.seed, 11),
        )
    };
    let (selected, stored) = if definition.bits & 0xC0 == 0xC0 {
        let offset = apply_offset([0.0; 3]);
        let selected = crate::fx::cloud_motion::Frame {
            origin: std::array::from_fn(|component| {
                (box_world[component] as f64 + offset[component] as f64) as f32
            }),
            axis: offset_axis(offset, sphere),
        };
        (selected, [0.0; 3])
    } else {
        let world = apply_offset(box_world);
        let selected = if definition.bits & 0xC0 == 0 {
            crate::fx::cloud_motion::Frame::IDENTITY
        } else {
            parent
        };
        (
            selected,
            crate::fx::cloud_motion::from_world(selected, world),
        )
    };
    // World mode still keeps its parent allocation axis in the particle for
    // native velocity orientation; select_frame chooses identity for storage.
    particle.frame_origin = if definition.bits & 0xC0 == 0xC0 {
        selected.origin
    } else {
        parent_origin
    };
    particle.frame_axis = if definition.bits & 0xC0 == 0xC0 {
        selected.axis
    } else {
        parent_axis
    };
    particle.origin = stored;
    particle.motion = None;
    let world = crate::fx::cloud_motion::to_world(selected, stored);
    particle.cloud_motion = Some(CloudMotion {
        age: 0,
        previous_age: 0,
        stored,
        world,
        previous_world: world,
        update_world: world,
        base_velocity: [0.0; 3],
    });
}

fn initialize_particle_frame(
    definition: &Definition,
    particle: &mut Particle,
    origin: [f32; 3],
    axis: Axis,
) {
    if definition.bits & 0x30 != 0
        && (definition.emission.is_some()
            || (definition.bits & 0xC0 == 0xC0
                && definition.kind != "runner"
                && (definition.gravity != [0.0; 2]
                    || definition.runtime.contains("useCollision")
                    || definition.bits & 0x07000000 == 0x01000000)))
    {
        // Original birth always stores zero in an offset-relative particle;
        // its full birth position lives in the derived frame. This applies to
        // force-updated particles as well as particles that emit child effects.
        // Local-only motion also retains its integral. The motion helper keeps
        // this local state without an unnecessary inverse-frame roundtrip.
        initialize_offset_particle(definition, particle, origin, axis);
        return;
    }
    particle.frame_origin = origin;
    particle.frame_axis = axis;
    if definition.runtime.contains("spawnRelative") {
        particle.origin = frames::transform(axis, particle.origin);
    }
    if definition.bits & 0x30 == 0 {
        return;
    }
    let offset = spawn_offset(definition, particle.seed, axis);
    if definition.bits & 0xC0 != 0xC0 {
        // Original sphere/cylinder birth adds its contribution to the existing
        // box point before the final float store. Adding an already-rounded
        // zero-based offset introduces a second store and can change a bit.
        particle.origin = crate::fx::offset_math::apply_offset(
            particle.origin,
            axis,
            definition.runtime.contains("spawnOffsetSphere"),
            definition.offset_radius,
            definition.offset_height,
            random(particle.seed, 9),
            random(particle.seed, 10),
            random(particle.seed, 11),
        );
    } else {
        for component in 0..3 {
            particle.origin[component] += offset[component];
        }
    }
    if definition.bits & 0xC0 == 0xC0 && definition.kind != "runner" {
        particle.frame_axis = offset_axis(offset, definition.runtime.contains("spawnOffsetSphere"));
        if definition.emission.is_some() && definition.runtime.contains("spawnOffsetCylinder") {
            // Original 0x43cff0 stores zero for offset-relative birth. Its
            // 0x44e900 orientation carries the complete birth world position.
            // The current bounded emission route validates the parent frame
            // before using this representation for incremental movement.
            particle.frame_origin =
                std::array::from_fn(|axis| particle.frame_origin[axis] + particle.origin[axis]);
            particle.origin = [0.0; 3];
        }
    }
}

fn selected_cloud_frame(
    definition: &Definition,
    particle: &Particle,
) -> crate::fx::cloud_motion::Frame {
    let frame = crate::fx::cloud_motion::Frame {
        origin: particle.frame_origin,
        axis: particle.frame_axis,
    };
    let current = particle.cloud_current_frame.unwrap_or(frame);
    crate::fx::cloud_motion::select_frame(definition.bits, frame, current)
        .expect("compiled cloud frame mode was validated")
}

/// Native birth first produces a world point, then stores it in the selected
/// run-relative frame. Static child frames have already passed quaternion storage.
fn initialize_cloud_motion(definition: &Definition, particle: &mut Particle) -> Result<(), String> {
    // Offset emitters already own the native birth/frame store. Repeating the
    // inverse transform here would add rounding absent from the native path.
    if particle.cloud_motion.is_some() {
        return Ok(());
    }
    if definition.bits & 0xC0 == 0xC0
        && definition.emission.is_some()
        && definition.runtime.contains("spawnOffsetCylinder")
    {
        let frame = selected_cloud_frame(definition, particle);
        let world = crate::fx::cloud_motion::to_world(frame, [0.0; 3]);
        particle.motion = None;
        particle.cloud_motion = Some(CloudMotion {
            age: 0,
            previous_age: 0,
            stored: [0.0; 3],
            world,
            previous_world: world,
            update_world: world,
            base_velocity: [0.0; 3],
        });
        return Ok(());
    }
    let world = if definition.bits & 0x30 == 0 {
        let local = std::array::from_fn(|axis| {
            (definition.origin[axis][0] as f64
                + definition.origin[axis][1] as f64 * random(particle.seed, 6 + axis) as f64)
                as f32
        });
        let frame = crate::fx::cloud_motion::Frame {
            origin: particle.frame_origin,
            axis: if definition.runtime.contains("spawnRelative") {
                particle.frame_axis
            } else {
                frames::IDENTITY
            },
        };
        crate::fx::cloud_motion::spawn_to_world(frame, local)
    } else {
        if particle.frame_axis != frames::IDENTITY || particle.frame_origin != [0.0; 3] {
            return Err("Transformed cloud sphere/cylinder birth offsets remain unverified".into());
        }
        particle.origin
    };
    let frame = selected_cloud_frame(definition, particle);
    let stored = crate::fx::cloud_motion::from_world(frame, world);
    // Native drawing transforms the stored point again, including its rounding.
    let world = crate::fx::cloud_motion::to_world(frame, stored);
    particle.origin = stored;
    particle.motion = None;
    particle.cloud_motion = Some(CloudMotion {
        age: 0,
        previous_age: 0,
        stored,
        world,
        previous_world: world,
        update_world: world,
        base_velocity: [0.0; 3],
    });
    Ok(())
}

fn particle_position(definition: &Definition, particle: &Particle, age: f32) -> [f32; 3] {
    if let Some(motion) = &particle.cloud_motion {
        if age == motion.age as f32 {
            return motion.world;
        }
        if age == motion.previous_age as f32 {
            return motion.previous_world;
        }
    }
    if let Some(motion) = &particle.motion {
        if age == motion.age as f32 {
            return motion.position;
        }
        if age == motion.previous_age as f32 {
            return motion.previous_position;
        }
    }
    let intervals = definition.velocity.len() - 1;
    let norm = age / particle.life as f32;
    let mut position =
        std::array::from_fn(|axis| particle.origin[axis] + particle.frame_origin[axis]);
    let mut displacement = [[0.0; 3]; 2];
    let frame_axis = orientation_axis(definition, particle);
    for interval in 0..intervals {
        let start = interval as f32 / intervals as f32;
        let duration = (norm - start).min(1.0 / intervals as f32);
        if duration <= 0.0 {
            break;
        }
        let fraction = duration * intervals as f32;
        for world in 0..2 {
            let enabled = if world == 0 { 0x01000000 } else { 0x02000000 };
            if definition.bits & enabled == 0 {
                continue;
            }
            let offset = world * 12;
            for axis in 0..3 {
                let scale = intervals as f32 * 1000.0;
                let a = (definition.velocity[interval][offset + axis]
                    + definition.velocity[interval][offset + 3 + axis]
                        * random(particle.seed, axis))
                    * scale;
                let b = (definition.velocity[interval + 1][offset + axis]
                    + definition.velocity[interval + 1][offset + 3 + axis]
                        * random(particle.seed, axis))
                    * scale;
                let contribution =
                    (a + (a + (b - a) * fraction)) * 0.5 * duration * particle.life as f32 / 1000.0;
                if frame_axis == frames::IDENTITY {
                    // Preserve the already-verified identity-frame accumulation
                    // order; regrouping interval sums changes f32 low bits.
                    position[axis] += contribution;
                } else {
                    displacement[world][axis] += contribution;
                }
            }
        }
    }
    if frame_axis != frames::IDENTITY {
        let local = frames::transform(frame_axis, displacement[0]);
        for component in 0..3 {
            position[component] += local[component] + displacement[1][component];
        }
    }
    let gravity = definition.gravity[0] + definition.gravity[1] * random(particle.seed, 15);
    let seconds = age / 1000.0;
    position[2] -= 400.0 * gravity * seconds * seconds;
    position
}

// FX_GetVelocityAtTime samples the velocity curve itself. Displacement divided
// by age is not its derivative and produces the wrong tail/line facing.
fn particle_velocity(definition: &Definition, particle: &Particle, age: f32) -> [f32; 3] {
    let intervals = definition.velocity.len() - 1;
    let sample = age / particle.life as f32 * intervals as f32;
    let index = (sample.floor() as usize).min(intervals - 1);
    let fraction = sample - index as f32;
    if let Some(motion) = &particle.cloud_motion {
        return crate::fx::native_velocity::evaluate(
            &definition.velocity[index],
            &definition.velocity[index + 1],
            intervals as f32,
            fraction,
            definition.bits,
            std::array::from_fn(|axis| random(particle.seed, axis)),
            orientation_axis(definition, particle),
            motion.base_velocity,
        );
    }
    // 44EA80 stores weighted endpoints, not each randomized endpoint.
    // Use the same native sampler for ordinary tails/lines as for clouds;
    // premature f32 endpoint rounding changes real packed normal bytes.
    let mut velocity = crate::fx::native_velocity::evaluate(
        &definition.velocity[index],
        &definition.velocity[index + 1],
        intervals as f32,
        fraction,
        definition.bits,
        std::array::from_fn(|axis| random(particle.seed, axis)),
        orientation_axis(definition, particle),
        [0.0; 3],
    );
    let gravity = definition.gravity[0] + definition.gravity[1] * random(particle.seed, 15);
    if let Some(motion) = &particle.motion {
        if age == motion.age as f32 {
            velocity[2] += motion.gravity_velocity;
        } else {
            velocity[2] -= 800.0 * gravity * age / 1000.0;
        }
    } else {
        velocity[2] -= 800.0 * gravity * age / 1000.0;
    }
    velocity
}

/// Preserve native per-update float stores. Clouds use their selected static
/// or explicitly supplied moving frame. The separate legacy emission branch
/// remains bounded: its one-ms delta also determines the child's direction.
fn prime_negative_root_birth(definition: &Definition, particle: &mut Particle, now: u32) {
    if now != 0 || particle.effect != 0 || particle.birth >= 0 || definition.delay[0] >= 0 {
        return;
    }
    let age = -particle.birth;
    if let Some(motion) = &mut particle.cloud_motion {
        motion.age = age;
        motion.previous_age = age;
    }
    if let Some(motion) = &mut particle.motion {
        motion.age = age;
        motion.previous_age = age;
    }
}

fn advance_emitter(definition: &Definition, particle: &mut Particle, end_age: i64) {
    if particle.cloud_motion.is_some() {
        let frame = selected_cloud_frame(definition, particle);
        let amplitude = std::array::from_fn(|axis| random(particle.seed, axis));
        let motion = particle.cloud_motion.as_mut().unwrap();
        if end_age <= motion.age {
            // A delayed element can first become drawable at age zero after
            // its owning effect has moved since the element was allocated.
            motion.world = crate::fx::cloud_motion::to_world(frame, motion.stored);
            return;
        }
        motion.previous_age = motion.age;
        motion.previous_world = motion.world;
        let previous_local = motion.stored;
        motion.update_world = crate::fx::cloud_motion::integrate_step_with_gravity(
            &mut motion.stored,
            &mut motion.base_velocity,
            &definition.velocity,
            definition.bits,
            particle.life as f32,
            amplitude,
            definition.gravity,
            random(particle.seed, 15),
            motion.age,
            end_age,
            frame,
        )
        .expect("validated native cloud motion interval");
        motion.world = crate::fx::cloud_motion::to_world(frame, motion.stored);
        motion.age = end_age;
        if particle.at_rest == 255
            && end_age > 0
            && particle.life > 0
            && !definition.runtime.contains("useCollision")
            && previous_local == motion.stored
        {
            particle.at_rest =
                crate::fx::rotation_math::quantized_rest_time(end_age as f32, particle.life as f32)
                    as u8;
        }
        return;
    }
    let Some(motion) = &mut particle.motion else {
        return;
    };
    if end_age <= motion.age {
        return;
    }
    let count = definition.velocity.len() - 1;
    let t0 = motion.age as f32 / particle.life as f32;
    let t1 = end_age as f32 / particle.life as f32;
    let start_point = (count as f64 * t0 as f64) as f32;
    let end_point = (count as f64 * t1 as f64) as f32;
    let start = (start_point.floor() as usize).min(count - 1);
    let end = (end_point.ceil() as usize).saturating_sub(1).min(count - 1);
    let a = (start_point as f64 - start as f64) as f32;
    let b = (end_point as f64 - end as f64) as f32;
    motion.previous_age = motion.age;
    motion.previous_position = motion.position;
    for world in 0..2 {
        if definition.bits & (0x01000000 << world) == 0 {
            continue;
        }
        let offset = world * 12;
        for axis in 0..3 {
            let amplitude = random(particle.seed, axis);
            let sample = |index: usize, integral: bool| -> f64 {
                let channel = offset + axis + if integral { 6 } else { 0 };
                definition.velocity[index][channel] as f64
                    + amplitude as f64 * definition.velocity[index][channel + 3] as f64
            };
            let scale = particle.life as f64;
            if start == end {
                let w1 = (scale * 0.5 * (b as f64 * b as f64 - a as f64 * a as f64)) as f32;
                let w0 = ((b as f64 - a as f64) * scale - w1 as f64) as f32;
                motion.position[axis] =
                    (motion.position[axis] as f64 + sample(start, false) * w0 as f64) as f32;
                motion.position[axis] =
                    (motion.position[axis] as f64 + sample(start + 1, false) * w1 as f64) as f32;
            } else {
                for (index, t, sign) in [(end, b, 1.0_f64), (start, a, -1.0)] {
                    let integral_scale = scale * sign;
                    // Original 0x44BEE0 stores this product before taking
                    // the triangular weight, including on emitter updates.
                    let product = (t as f64 * integral_scale) as f32;
                    let w1 = (0.5 * t as f64 * product as f64) as f32;
                    let w0 = (product as f64 - w1 as f64) as f32;
                    motion.position[axis] = (motion.position[axis] as f64
                        + sample(index, true) * integral_scale)
                        as f32;
                    motion.position[axis] =
                        (motion.position[axis] as f64 + sample(index, false) * w0 as f64) as f32;
                    motion.position[axis] = (motion.position[axis] as f64
                        + sample(index + 1, false) * w1 as f64)
                        as f32;
                }
            }
        }
    }
    let seconds = ((end_age - motion.age) as f64 * 0.0010000000474974513) as f32;
    motion.position[2] =
        (motion.position[2] as f64 + motion.gravity_velocity as f64 * seconds as f64) as f32;
    let gravity = (definition.gravity[0] as f64
        + definition.gravity[1] as f64 * random(particle.seed, 15) as f64) as f32;
    let delta = (gravity as f64 * 800.0 * seconds as f64) as f32;
    motion.gravity_velocity = (motion.gravity_velocity as f64 - delta as f64) as f32;
    motion.position[2] = (motion.position[2] as f64 - seconds as f64 * delta as f64 * 0.5) as f32;
    motion.age = end_age;
}

fn canonical(name: &str) -> String {
    name.replace('\\', "/")
        .trim_end_matches(".efx")
        .to_ascii_lowercase()
}

fn active_child(
    element: &Value,
    editor: &BTreeSet<String>,
    flag: &str,
    field: &str,
) -> Option<String> {
    if !editor.contains(flag) {
        return None;
    }
    tokens(element, field)
        .into_iter()
        .find(|name| !name.is_empty())
        .map(|name| canonical(&name))
}

fn host_word(state: &mut u32) -> u32 {
    *state = state.wrapping_mul(214013).wrapping_add(2531011);
    (*state >> 16) & 32767
}

/// Native FX_ProcessEmitting's residual is a quantized fraction of the largest
/// interval. Its random draw happens even on steps which create no children.
/// Its directional child axis is reconstructed from each actual movement step
/// using the native perpendicular-vector rule, not the parent emitter axis.
fn emission_end_position(definition: &Definition, particle: &Particle, age: f32) -> [f32; 3] {
    if let Some(motion) = &particle.cloud_motion {
        if age == motion.age as f32 {
            return motion.update_world;
        }
    }
    particle_position(definition, particle, age)
}

fn emission_events(
    definition: &Definition,
    particle: &mut Particle,
    begin: i64,
    end: i64,
    random_state: &mut u32,
) -> Result<Vec<(i64, [f32; 3], u32)>, String> {
    let from = particle_position(definition, particle, (begin - particle.birth) as f32);
    let to = emission_end_position(definition, particle, (end - particle.birth) as f32);
    let delta: [f32; 3] = std::array::from_fn(|axis| to[axis] - from[axis]);
    let distance = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
    if distance == 0.0 {
        return Ok(Vec::new());
    }
    let distance_base = (random(particle.seed, 20) as f64 * definition.emit_distance[1] as f64
        + definition.emit_distance[0] as f64) as f32;
    let base = distance_base + definition.emit_variance[0];
    let maximum = base + definition.emit_variance[1];
    if base <= 0.0 || maximum <= 0.0 || definition.emit_variance[1] < 0.0 {
        return Err("emission requires positive bounded spacing".into());
    }
    let mut next = -(particle.emit_residual as f32 * maximum * 0.00390625);
    let mut events = Vec::new();
    let last;
    loop {
        let previous = next;
        let word = host_word(random_state);
        next = (word as f64 * 0.000030517578125 * definition.emit_variance[1] as f64
            + base as f64
            + next as f64) as f32;
        if next > distance {
            last = previous;
            break;
        }
        next = next.max(0.0);
        let fraction = next / distance;
        let scheduled = begin + ((end - begin) as f32 * fraction).floor() as i64;
        let origin = std::array::from_fn(|axis| from[axis] + fraction * delta[axis]);
        // A successful native allocation obtains its child seed before the
        // scheduler draws the next interval. Preserve that interleaving.
        events.push((scheduled, origin, host_word(random_state)));
        if events.len() > 2048 {
            return Err("emission interval child budget exceeded".into());
        }
    }
    let residual = distance - last;
    if residual < -0.001 || residual > maximum + 0.001 {
        return Err("emission residual outside native bounds".into());
    }
    let fraction = (256.0_f64 * residual as f64 / maximum as f64) as f32;
    // WaW uses FISTP with its normal nearest-even control word here. The
    // upstream C++ truncating int cast is not equivalent at e.g. 12.8 -> 13.
    particle.emit_residual =
        (fraction as f64 + 9.313225746154785e-10).round_ties_even() as i32 as u8;
    Ok(events)
}

fn atlas_uv(definition: &Definition, particle: &Particle, age: f32) -> [f32; 4] {
    let [behavior, index, fps, loops, column_bits, row_bits, count] = definition.atlas;
    if count <= 1 {
        return [0.0, 1.0, 0.0, 1.0];
    }
    let columns = 1_u32 << column_bits;
    let rows = 1_u32 << row_bits;
    let mut frame = match behavior & 3 {
        0 => index,
        1 => (random_word(particle.seed, 22) as u32 * count) >> 16,
        2 => particle.sequence as u32 & (count - 1),
        _ => unreachable!(),
    };
    frame += if behavior & 4 != 0 {
        (age / particle.life as f32 * count as f32) as u32
    } else {
        (age as u32 * fps) / 1000
    };
    if behavior & 8 != 0 && frame >= count * loops {
        frame = count - 1;
    }
    frame &= count - 1;
    [
        (frame % columns) as f32 / columns as f32,
        1.0 / columns as f32,
        (frame / columns) as f32 / rows as f32,
        1.0 / rows as f32,
    ]
}

// The stock writer's bit operation is intentionally retained, rather than a
// generic half-float library whose rounding at ties can differ.
fn pack_uv(value: f32) -> u16 {
    let word = value.to_bits();
    let mantissa = (((word.wrapping_shl(1) ^ 0x80003fff) as i32) >> 14).clamp(-16384, 16383);
    ((mantissa as u32 & 0x3fff) | ((word >> 16) & 0xc000)) as u16
}

fn distance_squared(position: [f32; 3], camera: [f32; 3]) -> f32 {
    (camera[0] - position[0]).powi(2)
        + (camera[1] - position[1]).powi(2)
        + (camera[2] - position[2]).powi(2)
}

/// Native sort-key helpers 0x44f410/0x4182f0 store the three deltas,
/// evaluate their squared sum in x87 precision, then store one f32 key.
/// Keeping each product in f32 breaks real camonet allocation ties.
fn particle_sort_distance(position: [f32; 3], camera: [f32; 3]) -> f32 {
    let delta: [f32; 3] = std::array::from_fn(|axis| camera[axis] - position[axis]);
    (delta[0] as f64 * delta[0] as f64
        + delta[1] as f64 * delta[1] as f64
        + delta[2] as f64 * delta[2] as f64) as f32
}

fn distance_fade(definition: &Definition, position: [f32; 3], camera: [f32; 3]) -> u8 {
    let distance = distance_squared(position, camera).sqrt();
    let falloff = |range: [f32; 2]| -> f32 {
        if distance < range[0] {
            return 1.0;
        }
        if distance >= range[0] + range[1] {
            return 0.0;
        }
        1.0 - (distance - range[0]) / range[1]
    };
    let incoming = if definition.fade_in[1] != 0.0 {
        falloff(definition.fade_in)
    } else {
        1.0
    };
    let outgoing = if definition.fade_out[1] != 0.0 {
        1.0 - falloff(definition.fade_out)
    } else {
        1.0
    };
    (incoming.min(outgoing) * 255.0 + 0.5).floor() as u8
}

fn sample_visual(
    definition: &Definition,
    particle: &Particle,
    age: f32,
    position: [f32; 3],
    camera: [f32; 3],
) -> ([u8; 4], f32, f32, f32, [f32; 4]) {
    // 452770 explicitly selects the final visual sample at a zero-life
    // callback birth. Its draw age is -0.01; normalized age is separately 1.
    let normalized = if particle.life == 0 {
        1.0
    } else {
        age / particle.life as f32
    };
    let point = normalized * (definition.visual.len() - 1) as f32;
    let index = if particle.life == 0 {
        definition.visual.len() - 1
    } else {
        (point.floor() as usize).min(definition.visual.len() - 2)
    };
    let fraction = point - index as f32;
    let inverse = 1.0 - fraction;
    let previous = &definition.visual[index];
    let next = &definition.visual[(index + 1).min(definition.visual.len() - 1)];
    let color_random = random(particle.seed, 23);
    let mut color = [0_u8; 4];
    for channel in 0..4 {
        let inverse_random = 1.0 - color_random;
        // Both random-endpoint products are evaluated in the native x87
        // register precision before their sum is stored as one float. Rounding
        // each product first shifts some exact half-byte ties by one channel.
        let a = (previous.colors[1][channel] as f64 * color_random as f64
            + previous.colors[0][channel] as f64 * inverse_random as f64) as f32;
        let b = (next.colors[1][channel] as f64 * color_random as f64
            + next.colors[0][channel] as f64 * inverse_random as f64) as f32;
        // The native x87 writer retains the previous weighted product until
        // the sum is stored; preserve that intermediate with f64 here.
        let mixed = (b as f64 * fraction as f64 + a as f64 * inverse as f64) as f32;
        color[channel] = (mixed + 0.5).floor().clamp(0.0, 255.0) as u8;
    }
    if definition.lighting_fraction > 0 {
        color = crate::fx::preview_colour::apply(
            particle.packed_lighting,
            definition.lighting_fraction,
            color,
        );
    }
    color[3] = ((color[3] as u16 * distance_fade(definition, position, camera) as u16) >> 8) as u8;
    let scalar_at = |component: usize, random_index: usize| -> f32 {
        // Native FX_EvaluateSize / 0x447B80 stores each randomized endpoint
        // as f32, then keeps both weighted products until the final f32 store.
        // Rounding either multiply/add early shifts real stock radii by an ULP.
        let value = random(particle.seed, random_index) as f64;
        let a = (previous.values[0][component] as f64
            + previous.values[1][component] as f64 * value) as f32;
        let b =
            (next.values[0][component] as f64 + next.values[1][component] as f64 * value) as f32;
        (a as f64 * inverse as f64 + b as f64 * fraction as f64) as f32
    };
    let width = scalar_at(2, 26);
    // Native FX_EvaluateSize checks only NONUNIFORM_SCALE here. Tail/line
    // conversion still compiles sizeGraph1 unconditionally, but that stored
    // height is dormant at draw time when the runtime flag is absent.
    let height = if definition.runtime.contains("nonUniformScale") {
        scalar_at(3, 27)
    } else {
        width
    };
    let rotation_random = random(particle.seed, 25) as f64;
    // Original 0x450810 stores the partial weights, accumulated sample and
    // each completed contribution separately. The randomized rates remain in
    // the x87 accumulator until their weighted contribution is added.
    let partial = (fraction as f64 * fraction as f64 * 0.5) as f32;
    let remaining = (fraction as f64 - partial as f64) as f32;
    let accumulated =
        (previous.values[0][1] as f64 + previous.values[1][1] as f64 * rotation_random) as f32;
    let first = ((previous.values[0][0] as f64 + previous.values[1][0] as f64 * rotation_random)
        * remaining as f64
        + accumulated as f64) as f32;
    let integrated = ((next.values[0][0] as f64 + next.values[1][0] as f64 * rotation_random)
        * partial as f64
        + first as f64) as f32;
    let integrated = (integrated as f64 * particle.life as f64) as f32;
    let initial = (definition.initial_rotation[0] as f64
        + definition.initial_rotation[1] as f64 * random(particle.seed, 24) as f64)
        as f32;
    let rotation = (integrated as f64 + initial as f64) as f32;
    let [u, du, v, dv] = atlas_uv(definition, particle, age);
    (color, width, height, rotation, [u, du, v, dv])
}

/// Original 0x4525f0 first samples byte colour through 0x450ae0, then
/// expands B/G/R with the widened f32 reciprocal at native 0x538988. It does
/// not preserve sub-byte colour or multiply light RGB by the alpha channel.
fn light_particle(
    definition: &Definition,
    particle: &Particle,
    now: u32,
    camera: [f32; 3],
) -> Option<Value> {
    let age = now as f32 - particle.birth as f32;
    let origin = particle_position(definition, particle, age);
    let (bgra, radius, _, _, _) = sample_visual(definition, particle, age, origin, camera);
    if radius <= 0.0 {
        return None;
    }
    let reciprocal = 0.003921568859368563_f64;
    let rgb = [bgra[2], bgra[1], bgra[0]].map(|byte| (byte as f64 * reciprocal) as f32);
    let mut light = json!({
        "origin":origin, "rgb":rgb, "radius":radius.min(2000.0),
        "unclampedRadius":radius, "sourceIndex":definition.source,
        "layer":particle.layer, "sequence":particle.sequence,
        "birth":particle.birth, "life":particle.life, "seed":particle.seed,
    });
    if definition.kind == "spotLight" {
        // Original renderer registration selects these defaults. They are
        // explicit preview-preview settings, not fields authored in the EFX.
        // The cone's virtual origin moves backwards along its emitter axis.
        let input_radius = radius.min(2000.0);
        let ratio = (160.0_f64 / input_radius as f64) as f32;
        let outer_angle = (ratio as f64).atan() as f32;
        let inner_angle = (outer_angle as f64 * 0.699999988079071_f64) as f32;
        let near = (36.0_f64 / (outer_angle as f64).tan()) as f32;
        let direction = orientation_axis(definition, particle)[0].map(|value| -value);
        let scene_origin = std::array::from_fn::<_, 3, _>(|axis| {
            (direction[axis] as f64 * near as f64 + origin[axis] as f64) as f32
        });
        let scene_rgb = rgb.map(|value| (value as f64 * 14.0) as f32);
        light["type"] = json!("spot");
        light["inputOrigin"] = json!(origin);
        light["inputRadius"] = json!(input_radius);
        light["origin"] = json!(scene_origin);
        light["rgb"] = json!(scene_rgb);
        light["radius"] = json!((input_radius as f64 + near as f64) as f32);
        light["direction"] = json!(direction);
        light["cosHalfFovOuter"] = json!((outer_angle as f64).cos() as f32);
        light["cosHalfFovInner"] = json!((inner_angle as f64).cos() as f32);
        light["spotExponent"] = json!(1);
        light["previewSpotSettings"] = json!({"startRadius":36.0,"endRadius":196.0,
            "innerFraction":0.699999988079071_f32,"brightness":14.0,"nativeShadowRequest":true,"shadowMap":false});
    }
    Some(light)
}

fn model_particle(definition: &Definition, particle: &Particle, time: u32) -> Option<Value> {
    let age = (time as i64 - particle.birth) as f32;
    let rotation_age =
        crate::fx::rotation_math::rotation_time(age, particle.life as f32, particle.at_rest);
    let point = age / particle.life as f32 * (definition.visual.len() - 1) as f32;
    let index = (point.floor() as usize).min(definition.visual.len() - 2);
    let fraction = point - index as f32;
    let endpoint = |sample: &VisualSample| {
        (sample.values[1][4] as f64 * random(particle.seed, 28) as f64 + sample.values[0][4] as f64)
            as f32
    };
    let scale = (endpoint(&definition.visual[index]) as f64 * (1.0 - fraction) as f64
        + endpoint(&definition.visual[index + 1]) as f64 * fraction as f64) as f32;
    if scale == 0.0 || definition.materials.is_empty() {
        return None;
    }
    let angles = std::array::from_fn(|axis| {
        let base = (definition.spawn_angles[axis][1] as f64
            * random(particle.seed, 12 + axis) as f64
            + definition.spawn_angles[axis][0] as f64) as f32;
        let rate = (definition.angular_velocity[axis][1] as f64
            * random(particle.seed, 3 + axis) as f64
            + definition.angular_velocity[axis][0] as f64) as f32;
        (rate as f64 * rotation_age as f64 + base as f64) as f32
    });
    let axis =
        crate::fx::cloud_state::oriented_axis(angles, orientation_axis(definition, particle));
    // Models and clouds use the same original scaled-placement writer.
    let origin = particle_position(definition, particle, age);
    let placement =
        crate::fx::cloud_state::scene_record(origin, [0.0; 3], axis, scale, [0; 4], [0.0; 2]);
    let bytes = STANDARD
        .decode(placement["sceneRecordBytes"].as_str().unwrap())
        .unwrap();
    let visual = (definition.materials.len() * random_word(particle.seed, 21) as usize) >> 16;
    Some(
        json!({"model":definition.materials[visual],"origin":origin,"quat":placement["quat"],
        "scale":scale,"placementBytes":STANDARD.encode(&bytes[..32]),"angles":angles,
        "sourceIndex":definition.source,"layer":particle.layer,"sequence":particle.sequence,
        "birth":particle.birth,"life":particle.life,"seed":particle.seed}),
    )
}

fn draw_particle(
    definition: &Definition,
    particle: &Particle,
    now: u32,
    camera: [f32; 3],
    camera_axis: crate::fx::camera::Axis,
) -> Result<(Value, Value, Vec<[f32; 3]>), String> {
    let raw_age = now as f32 - particle.birth as f32;
    let position = particle_position(definition, particle, raw_age);
    let age = if particle.life == 0 {
        -0.01_f32
    } else {
        raw_age
    };
    let (color, width, height, rotation, [u, du, v, dv]) =
        sample_visual(definition, particle, age, position, camera);
    if definition.kind == "cloud" {
        let rotation_age =
            crate::fx::rotation_math::rotation_time(age, particle.life as f32, particle.at_rest);
        let point = age / particle.life as f32 * (definition.visual.len() - 1) as f32;
        let index = (point.floor() as usize).min(definition.visual.len() - 2);
        let fraction = point - index as f32;
        let a = &definition.visual[index];
        let b = &definition.visual[index + 1];
        // WaW reserves random offset 28 for cloud scale. CoD4's equivalent
        // source uses 27; the original WaW instructions settle the difference.
        let evaluate = |sample: &VisualSample| {
            (sample.values[1][4] as f64 * random(particle.seed, 28) as f64
                + sample.values[0][4] as f64) as f32
        };
        let scale = (evaluate(a) as f64 * (1.0 - fraction) as f64
            + evaluate(b) as f64 * fraction as f64) as f32;
        if scale == 0.0 {
            return Ok((Value::Null, Value::Null, Vec::new()));
        }
        let angles = std::array::from_fn(|axis| {
            let base = (definition.spawn_angles[axis][1] as f64
                * random(particle.seed, 12 + axis) as f64
                + definition.spawn_angles[axis][0] as f64) as f32;
            let velocity = (definition.angular_velocity[axis][1] as f64
                * random(particle.seed, 3 + axis) as f64
                + definition.angular_velocity[axis][0] as f64) as f32;
            (velocity as f64 * rotation_age as f64 + base as f64) as f32
        });
        let axis =
            crate::fx::cloud_state::oriented_axis(angles, orientation_axis(definition, particle));
        let cloud = crate::fx::cloud_state::scene_record(
            position,
            particle_velocity(definition, particle, age),
            axis,
            scale,
            color,
            [width, height],
        );
        let material_index =
            (definition.materials.len() * random_word(particle.seed, 21) as usize) >> 16;
        let material = &definition.materials[material_index];
        let draw = json!({"texture":material,"vertexFormat":"cloud20","cloud":cloud});
        let diagnostic = json!({"layer":particle.layer,"sourceIndex":definition.source,
            "sequence":particle.sequence,"scheduled":particle.scheduled,"birth":particle.birth,
            "life":particle.life,"seed":particle.seed,"localOrigin":particle.origin,
            "position":position,"bgra":color,"size":[width,height],"angles":angles,
            "material":material,"cloud":cloud});
        // Camera fitting consumes the volume record, never fabricated quad
        // corners. The backend separately creates the native volume vertices.
        return Ok((draw, diagnostic, Vec::new()));
    }
    let coordinates = [[u, v + dv], [u, v], [u + du, v], [u + du, v + dv]];
    let orientation = if definition.kind == "billboardSprite" {
        camera_axis
    } else {
        orientation_axis(definition, particle)
    };
    let basis = crate::fx::geometry::basis(
        &definition.kind,
        position,
        particle_velocity(definition, particle, age),
        height,
        camera,
        orientation,
    )?;
    let mut corners = crate::fx::geometry::corners(&basis, rotation, width, height);
    if definition.kind == "billboardSprite" && camera_axis == crate::fx::camera::DEFAULT_AXIS {
        // Keep the previously verified billboard expression order. Equivalent
        // regrouping changes a few f32 low bits before GPU rasterisation.
        let signs = [[-1.0, -1.0], [-1.0, 1.0], [1.0, 1.0], [1.0, -1.0]];
        let cosine = (rotation as f64).cos() as f32;
        let sine = (rotation as f64).sin() as f32;
        for (index, [sx, sy]) in signs.into_iter().enumerate() {
            corners[index] = [
                position[0],
                position[1] + sx * width * cosine - sy * height * sine,
                position[2] + sx * width * sine + sy * height * cosine,
            ];
        }
    }
    let mut vertices = Vec::<u8>::with_capacity(128);
    let normal = crate::fx::geometry::pack_unit_vector(basis.normal);
    let (rotated_tangent, _) = crate::fx::geometry::rotated_axes(&basis, rotation);
    let tangent = crate::fx::geometry::pack_unit_vector(rotated_tangent);
    let mut points = Vec::new();
    let mut diagnostic_vertices = Vec::new();
    for (vertex, point) in corners.into_iter().enumerate() {
        points.push(point);
        for value in point.into_iter().chain([-1.0]) {
            vertices.extend_from_slice(&value.to_le_bytes());
        }
        vertices.extend_from_slice(&color);
        let packed = ((pack_uv(coordinates[vertex][0]) as u32) << 16)
            | pack_uv(coordinates[vertex][1]) as u32;
        vertices.extend_from_slice(&packed.to_le_bytes());
        vertices.extend_from_slice(&normal);
        vertices.extend_from_slice(&tangent);
        diagnostic_vertices.push(json!({"position":point,"bgra":color,"uvPacked":packed}));
    }
    let material_index =
        (definition.materials.len() * random_word(particle.seed, 21) as usize) >> 16;
    let material = &definition.materials[material_index];
    let draw = json!({"texture":material, "vertices":STANDARD.encode(vertices)});
    let diagnostic = json!({"layer":particle.layer,"sourceIndex":definition.source,
        "sequence":particle.sequence,"scheduled":particle.scheduled,"birth":particle.birth,
        "life":particle.life,"seed":particle.seed,"localOrigin":particle.origin,
        "position":position,"bgra":color,"size":[width,height],"rotation":rotation,
        "uv":[u,du,v,dv],"material":material,"vertices":diagnostic_vertices});
    Ok((draw, diagnostic, points))
}

/// Opaque access to the same authored graph compiler used by sprite playback.
/// Trail lifecycle allocation, point retention and quantized gravity remain in
/// the separate trail module; there is no duplicated adaptive curve compiler.
pub(crate) struct TrailLayer {
    definition: Definition,
}

impl TrailLayer {
    pub(crate) fn apply_spawn_offset(&self, seed: usize, axis: Axis, base: [f32; 3]) -> [f32; 3] {
        if self.definition.bits & 0x30 == 0 {
            return base;
        }
        crate::fx::offset_math::apply_offset(
            base,
            axis,
            self.definition.runtime.contains("spawnOffsetSphere"),
            self.definition.offset_radius,
            self.definition.offset_height,
            random(seed, 9),
            random(seed, 10),
            random(seed, 11),
        )
    }

    pub(crate) fn spawn_offset(&self, seed: usize, axis: Axis) -> [f32; 3] {
        spawn_offset(&self.definition, seed, axis)
    }

    pub(crate) fn compile(
        element: &Value,
        source: usize,
        effect: &Value,
        metadata_optional: bool,
    ) -> Result<Self, String> {
        if element.get("trail").is_none() {
            return Err("trail sampler requires an authored trail element".into());
        }
        let mut authored = element.clone();
        let material = authored
            .as_object_mut()
            .ok_or("invalid trail element")?
            .remove("trail")
            .unwrap();
        authored["orientedSprite"] = material;
        let mut definition = compile(&authored, source, effect, metadata_optional)?
            .ok_or("disabled trail element")?;
        if definition.bits & 0xC0 != 0
            || definition.death.is_some()
            || definition.emission.is_some()
        {
            return Err(
                "trail sampler currently requires world-relative motion and no child dispatch"
                    .into(),
            );
        }
        definition.kind = "trail".into();
        Ok(Self { definition })
    }

    pub(crate) fn config(&self) -> Value {
        let mut result = definition_json(&self.definition);
        result["looping"] = json!(self.definition.looping);
        result["runtimeFlags"] = json!(self.definition.runtime);
        result
    }

    pub(crate) fn visual(
        &self,
        origin: [f32; 3],
        life: i32,
        random_seed: usize,
        age: i32,
        camera: [f32; 3],
    ) -> Result<Value, String> {
        if life <= 0 || random_seed >= 479 {
            return Err("invalid trail visual life/seed".into());
        }
        let mut particle = spawn(std::slice::from_ref(&self.definition), 0, 0, 0, 0);
        particle.seed = random_seed;
        particle.life = life as i64;
        let (color, width, height, rotation, uv) =
            sample_visual(&self.definition, &particle, age as f32, origin, camera);
        Ok(json!({"bgra":color,"size":[width,height],"rotation":rotation,"uv":uv}))
    }

    pub(crate) fn random(&self, seed: usize, offset: usize) -> f32 {
        random(seed, offset)
    }

    pub(crate) fn random_word(&self, seed: usize, offset: usize) -> i64 {
        random_word(seed, offset)
    }
}

#[derive(Clone)]
struct Instance {
    serial: u64,
    retired: bool,
    name: String,
    seed: u32,
    begin: i64,
    origin: [f32; 3],
    axis: Axis,
    copy_sort_orders: Vec<u8>,
    // The native runner assigns this to the child effect, not its elements.
    sort: u8,
    ancestry: Vec<String>,
}

const STREAM_SCHEDULE: u32 = u32::MAX;

#[derive(Clone, Debug, Default)]
pub(crate) struct LifecycleReport {
    pub completion_time: Option<u32>,
    pub naturally_infinite: bool,
    pub examined_through: u32,
    pub horizon_exhausted: bool,
    pub sample_times: Vec<u32>,
    pub host_random_state: u32,
}

pub(crate) struct StreamFrame {
    pub frame: Value,
    pub lifecycle: LifecycleReport,
}

struct StreamContext<'a> {
    callback: Option<&'a mut dyn FnMut(StreamFrame) -> Result<bool, String>>,
    report: LifecycleReport,
    interval: u32,
    next_sample: u32,
    cancelled: bool,
    visual_layers: BTreeSet<(u64, usize)>,
    extra_samples: BTreeSet<u32>,
    requested_times: Option<VecDeque<u32>>,
}

fn instance_slot(instances: &[Instance], streaming: bool) -> Result<(usize, u64), String> {
    let serial = instances
        .iter()
        .map(|instance| instance.serial)
        .max()
        .unwrap_or(0)
        + 1;
    if instances.len() < 1024 {
        return Ok((instances.len(), serial));
    }
    if streaming {
        if let Some(index) = instances
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(index, instance)| instance.retired.then_some(index))
        {
            return Ok((index, serial));
        }
    }
    Err("The active effect-instance pool capacity was exceeded".into())
}

fn store_instance(
    instances: &mut Vec<Instance>,
    order: &mut Vec<usize>,
    index: usize,
    child: Instance,
) {
    if index == instances.len() {
        instances.push(child);
    } else {
        order.retain(|previous| *previous != index);
        instances[index] = child;
    }
    order.push(index);
}

#[derive(Clone)]
struct Request {
    effect: usize,
    layer: usize,
    sequence: usize,
    scheduled: i64,
}

struct ChildEvent {
    parent: usize,
    name: String,
    scheduled: i64,
    origin: [f32; 3],
    axis: Axis,
    sort: u8,
    source: usize,
    kind: &'static str,
    random_word: Option<u32>,
}

fn allocate_child(
    event: ChildEvent,
    instances: &mut Vec<Instance>,
    effect_order: &mut Vec<usize>,
    definitions: &BTreeMap<String, Vec<Definition>>,
    copy_sort_orders: &BTreeMap<String, Vec<u8>>,
    random_state: &mut u32,
    queue: &mut BTreeMap<u32, VecDeque<Request>>,
    pending: &mut VecDeque<Request>,
    diagnostics: &mut Vec<Value>,
    now: u32,
    horizon: u32,
) -> Result<(), String> {
    let parent = &instances[event.parent];
    if parent.ancestry.contains(&event.name) || parent.ancestry.len() >= 16 {
        return Err(format!(
            "child cycle/depth budget reached: {} -> {}",
            parent.name, event.name
        ));
    }
    let (index, serial) = instance_slot(instances, horizon == STREAM_SCHEDULE)?;
    let word = event.random_word.unwrap_or_else(|| host_word(random_state));
    let seed = 479 * word / 32768;
    let mut ancestry = parent.ancestry.clone();
    ancestry.push(event.name.clone());
    let child = Instance {
        serial,
        retired: false,
        name: event.name.clone(),
        seed,
        begin: event.scheduled,
        origin: event.origin,
        axis: frames::stored_axis(event.axis),
        copy_sort_orders: copy_sort_orders[&event.name].clone(),
        sort: event.sort,
        ancestry,
    };
    let initial = queue_instance(
        index,
        &child,
        &definitions[&event.name],
        now,
        horizon,
        queue,
    )?;
    diagnostics.push(
        json!({"parent":event.parent,"effect":index,"name":event.name,
        "scheduled":event.scheduled,"origin":event.origin,"axis":event.axis,"storedAxis":child.axis,"seed":seed,"hostRandomWord":word,
        "sourceIndex":event.source,"kind":event.kind}),
    );
    store_instance(instances, effect_order, index, child);
    for request in initial.into_iter().rev() {
        pending.push_front(request);
    }
    Ok(())
}

fn compile_effect(effect: &Value, metadata_optional: bool) -> Result<Vec<Definition>, String> {
    let elements = effect["elements"].as_array().ok_or("missing elements")?;
    let mut definitions = Vec::new();
    for (index, element) in elements.iter().enumerate() {
        if let Some(definition) = compile(element, index, effect, metadata_optional)? {
            if definition.kind == "runner" && definition.children.is_empty() {
                return Err(format!("element {index}: runner has no effect references"));
            }
            definitions.push(definition);
        }
    }
    if definitions
        .iter()
        .any(|definition| definition.kind == "cloud")
    {
        if definitions.iter().any(|definition| {
            definition.kind == "cloud" && ![0, 0x40, 0x80].contains(&(definition.bits & 0xC0))
        }) {
            return Err(
                "cloud offset-relative motion remains outside the verified frame guards".into(),
            );
        }
    }
    definitions.sort_by_key(|definition| !definition.looping);
    Ok(definitions)
}

// Initial native StartNewEffect allocates one-shots immediately, even when
// their msecBegin lies in the future. Later looping requests use absolute time.
fn queue_instance(
    index: usize,
    instance: &Instance,
    definitions: &[Definition],
    now: u32,
    horizon: u32,
    queue: &mut BTreeMap<u32, VecDeque<Request>>,
) -> Result<Vec<Request>, String> {
    let mut initial = Vec::new();
    for (layer, definition) in definitions.iter().enumerate() {
        let count = if definition.looping {
            1
        } else {
            integer_random(definition.spawn, instance.seed as usize, 19)
        };
        if !(0..=2048).contains(&count) {
            return Err("preview one-shot particle limit exceeded".into());
        }
        for sequence in 0..count as usize {
            initial.push(Request {
                effect: index,
                layer,
                sequence,
                scheduled: instance.begin,
            });
        }
        if definition.looping && horizon != STREAM_SCHEDULE {
            let interval = definition.spawn[0];
            let available =
                ((horizon as i64 - instance.begin).max(0) / interval + 1).min(definition.spawn[1]);
            for sequence in 1..available as usize {
                let scheduled = instance.begin + sequence as i64 * interval;
                let execution = scheduled.max(now as i64) as u32;
                queue.entry(execution).or_default().push_back(Request {
                    effect: index,
                    layer,
                    sequence,
                    scheduled,
                });
            }
        }
    }
    Ok(initial)
}

/// Future native batches were registered in instance/layer order. Lazy
/// scheduling keeps that order, rather than depending on callback timing.
fn queue_next_loop(
    request: &Request,
    definition: &Definition,
    instances: &[Instance],
    queue: &mut BTreeMap<u32, VecDeque<Request>>,
    now: u32,
) -> Result<(), String> {
    if !definition.looping || definition.sound || definition.kind == "decal" {
        return Ok(());
    }
    let next = request
        .sequence
        .checked_add(1)
        .ok_or("Loop sequence overflow")?;
    if definition.spawn[1] != i64::from(i32::MAX) && next as i64 >= definition.spawn[1] {
        return Ok(());
    }
    let scheduled = instances[request.effect]
        .begin
        .checked_add(
            (next as i64)
                .checked_mul(definition.spawn[0])
                .ok_or("Loop time overflow")?,
        )
        .ok_or("Loop time overflow")?;
    let execution = u32::try_from(scheduled.max(now as i64)).map_err(|error| error.to_string())?;
    let next_request = Request {
        effect: request.effect,
        layer: request.layer,
        sequence: next,
        scheduled,
    };
    let priority = (instances[request.effect].serial, request.layer, next);
    let batch = queue.entry(execution).or_default();
    let position = batch
        .iter()
        .position(|other| (instances[other.effect].serial, other.layer, other.sequence) > priority)
        .unwrap_or(batch.len());
    batch.insert(position, next_request);
    Ok(())
}

fn effect_sort(instance: &Instance, definitions: &[Definition]) -> u8 {
    if instance.sort != 255 {
        return instance.sort;
    }
    let ordinary: Vec<&Definition> = definitions
        .iter()
        .filter(|definition| definition.kind != "runner")
        .collect();
    let count = ordinary.len() + instance.copy_sort_orders.len();
    if count == 0 {
        return 0;
    }
    let total = ordinary
        .iter()
        .map(|definition| definition.sort as usize)
        .sum::<usize>()
        + instance
            .copy_sort_orders
            .iter()
            .map(|sort| *sort as usize)
            .sum::<usize>();
    (total / count).min(254) as u8
}

fn first_effect_is_further(
    first: usize,
    second: usize,
    instances: &mut [Instance],
    definitions: &BTreeMap<String, Vec<Definition>>,
) -> bool {
    // Preserve the native 255/255 shortcut and cache resolved runner orders.
    // Resolving both to averages unconditionally changes equal-distance order.
    if instances[first].sort == 255 {
        if instances[second].sort == 255 {
            return false;
        }
        instances[first].sort =
            effect_sort(&instances[first], &definitions[&instances[first].name]);
    }
    if instances[second].sort == 255 {
        instances[second].sort =
            effect_sort(&instances[second], &definitions[&instances[second].name]);
    }
    instances[first].sort < instances[second].sort
}

/// Two-generation native controls prove this narrow static death/runner route.
/// Moving emitters, child runners, emissions and other families stay guarded.
fn bounded_static_nested_graph(effect: &Value) -> bool {
    if [
        "previewEmitterFrame",
        "previewEmitterFrames",
        "previewEmitterKeyframes",
        "previewSourceFrames",
    ]
    .iter()
    .any(|key| effect.get(*key).is_some())
    {
        return false;
    }
    let registry = effect["dependencies"].as_object();
    if registry.is_some_and(|definitions| definitions.len() > 2) {
        return false;
    }
    let mut cloud_leaf = false;
    for (is_root, source) in std::iter::once((true, effect)).chain(
        registry
            .into_iter()
            .flat_map(|definitions| definitions.values().map(|source| (false, source))),
    ) {
        let Ok(definitions) = compile_effect(source, true) else {
            return false;
        };
        for definition in definitions {
            if !["billboardSprite", "runner", "cloud"].contains(&definition.kind.as_str())
                || definition.looping || definition.bits & 0xF0 != 0
                || definition.gravity != [0.0; 2] || definition.emission.is_some()
                // These two controls prove stationary nested placement. A
                // stop followed by later acceleration needs native rest-state
                // lifecycle coverage before this admission can be broader.
                || definition.velocity.iter().flatten().any(|value| *value != 0.0)
                || definition.impact.is_some() || definition.runtime.contains("useCollision")
                || (!is_root && definition.kind == "runner")
                || (definition.kind == "cloud" && definition.death.is_some())
            {
                return false;
            }
            cloud_leaf |= definition.kind == "cloud";
        }
    }
    cloud_leaf
}

/// Validate the active graph before adapters remove layers or empty timelines
/// skip allocation. Cloud-containing graphs are bounded to one child generation.
fn nested_diagnostic(_effect: &Value) -> bool {
    false
}

/// Retain the earlier stationary controls and the independently proved static
/// death -> emission chain. Other graph shapes and custom frames stay guarded.
fn bounded_nested_graph(effect: &Value) -> bool {
    bounded_static_nested_graph(effect) || dense_vehicle_admission::accepts(effect)
}

pub fn validate_cloud_scope(effect: &Value) -> Result<(), String> {
    let root_name = canonical(effect["name"].as_str().unwrap_or("root"));
    let registry = effect["dependencies"].as_object();
    let mut queue = VecDeque::from([(root_name.clone(), effect, 0_usize)]);
    let mut visited = BTreeSet::new();
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut unresolved = Vec::new();
    while let Some((name, source, depth)) = queue.pop_front() {
        if !visited.insert(name.clone()) {
            continue;
        }
        if visited.len() > 512 {
            return Err("Cloud graph exceeds the preview definition budget".into());
        }
        let elements = source["elements"]
            .as_array()
            .ok_or("Missing graph elements")?;
        for element in elements {
            let editor = flags(element, "editorFlags");
            if editor.contains("disabled")
                || element.get("decal").is_some()
                || element.get("sound").is_some()
            {
                continue;
            }
            nodes.push(element);
            for (field, enabled) in [
                ("runner", true),
                ("emission", editor.contains("playOnRun")),
                ("fxOnDeath", editor.contains("playOnDeath")),
                // Original EffectsEd callback always returns fraction 1.
                // Presence remains in compiled metadata; this edge never runs.
                ("fxOnImpact", false),
            ] {
                if !enabled {
                    continue;
                }
                for child in tokens(element, field)
                    .into_iter()
                    .filter(|value| !value.is_empty())
                {
                    let child = canonical(&child);
                    edges.push((depth, child.clone()));
                    let definition = if child == root_name {
                        Some(effect)
                    } else {
                        registry.and_then(|definitions| definitions.get(&child))
                    };
                    if let Some(definition) = definition {
                        queue.push_back((child, definition, depth + 1));
                    } else {
                        unresolved.push(child);
                    }
                }
            }
        }
    }
    if !nodes.iter().any(|element| element.get("cloud").is_some()) {
        return Ok(());
    }
    if !unresolved.is_empty() {
        return Err(format!(
            "Cloud graph has missing active child definitions: {unresolved:?}"
        ));
    }
    if edges
        .iter()
        .any(|(depth, child)| *depth >= 1 || child == &root_name)
        && !bounded_nested_graph(effect)
        && !nested_diagnostic(effect)
    {
        return Err("Cloud child checkpoint supports one generation; nested/reentrant child graphs remain unverified".into());
    }
    for element in nodes {
        if ["trail"].iter().any(|kind| element.get(*kind).is_some()) {
            return Err("Cloud child checkpoint does not yet support trail companions".into());
        }
        if element.get("cloud").is_some()
            && [
                "billboardSprite",
                "orientedSprite",
                "tail",
                "line",
                "runner",
                "sound",
            ]
            .iter()
            .any(|kind| element.get(*kind).is_some())
        {
            return Err("A cloud element cannot also contain another visual type".into());
        }
    }
    Ok(())
}

/// Explicit static frame input for controlled preview. These axes are already
/// stored/native-decoded values; no unproved axis-to-quaternion conversion is
/// hidden in the fixture adapter. Ordinary installed-file playback uses identity.
fn preview_emitter_frame(effect: &Value) -> Result<([f32; 3], Axis), String> {
    let Some(frame) = effect.get("previewEmitterFrame") else {
        return Ok(([0.0; 3], frames::IDENTITY));
    };
    if frame.get("frames").is_some() || frame.get("times").is_some() {
        return Err("Moving cloud emitter frames remain unverified in the player".into());
    }
    let vector = |value: &Value| -> Result<[f32; 3], String> {
        let values = value
            .as_array()
            .ok_or("Frame vector needs three coordinates")?;
        if values.len() != 3 {
            return Err("Frame vector needs three coordinates".into());
        }
        let mut result = [0.0; 3];
        for (index, number) in values.iter().enumerate() {
            result[index] = number.as_f64().ok_or("Invalid frame coordinate")? as f32;
            if !result[index].is_finite() {
                return Err("Frame coordinates must be finite".into());
            }
        }
        Ok(result)
    };
    let origin = vector(&frame["origin"])?;
    let raw = frame["storedAxis"]
        .as_array()
        .ok_or("Fixture needs explicit native-stored axes")?;
    if raw.len() != 3 {
        return Err("Fixture axes need three basis vectors".into());
    }
    let axis = [vector(&raw[0])?, vector(&raw[1])?, vector(&raw[2])?];
    for first in 0..3 {
        for second in 0..3 {
            let dot: f64 = (0..3)
                .map(|component| axis[first][component] as f64 * axis[second][component] as f64)
                .sum();
            let expected = if first == second { 1.0 } else { 0.0 };
            if (dot - expected).abs() > 0.001 {
                return Err("Fixture stored axes must be orthonormal".into());
            }
        }
    }
    Ok((origin, axis))
}

/// Owned, visual-only preview timeline. Runner children share the same clock,
/// deterministic host random stream, particle budget and ordered draw sink.
/// No native process or Python helper is needed to play an effect.
pub fn simulate(effect: &Value, seed: u32, times: &[u32]) -> Result<Value, String> {
    simulate_internal(effect, seed, times, None, None)
}

/// Blocking CPU producer. The callback may block on a bounded channel; all
/// scheduler state remains owned by this stack until continuation or cancel.
pub(crate) fn stream(
    effect: &Value,
    seed: u32,
    sample_ms: u32,
    on_frame: &mut dyn FnMut(StreamFrame) -> Result<bool, String>,
) -> Result<LifecycleReport, String> {
    run_lifecycle(
        effect,
        seed,
        i32::MAX as u32,
        sample_ms.max(1),
        Some(on_frame),
        None,
    )
}

/// Follow real allocations/children/trail tails without retaining render frames.
pub(crate) fn probe_lifecycle(
    effect: &Value,
    seed: u32,
    max_ms: u32,
) -> Result<LifecycleReport, String> {
    run_lifecycle(effect, seed, max_ms.min(60000), 1, None, None)
}

/// Render exactly the requested samples on the retained scheduler. Thumbnail
/// and camera fitting must not hit the old batch recorder's cumulative spawn
/// or instance-history limits for effects that live playback can already run.
pub(crate) fn sample_frames(
    effect: &Value,
    seed: u32,
    times: &[u32],
) -> Result<Vec<Value>, String> {
    let budget = sample_byte_budget(effect);
    let mut retained = 0_usize;
    let mut frames = Vec::with_capacity(times.len());
    let mut collect = |frame: Value| -> Result<(), String> {
        retained = retained.saturating_add(crate::fx::memory_cost(&frame));
        if retained > budget {
            return Err("This effect exceeds the preview memory limit".into());
        }
        frames.push(frame);
        Ok(())
    };
    visit_samples(effect, seed, times, &mut collect)?;
    Ok(frames)
}

fn sample_byte_budget(effect: &Value) -> usize {
    effect["previewFrameByteBudget"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(128 * 1024 * 1024)
}

/// Visit every requested sample without retaining its geometry. A camera fit
/// needs exact bounds over the full timeline, not a recording of all frames.
/// The caller owns one frame at a time and normally drops it after inspection.
pub(crate) fn visit_samples(
    effect: &Value,
    seed: u32,
    times: &[u32],
    visit: &mut dyn FnMut(Value) -> Result<(), String>,
) -> Result<LifecycleReport, String> {
    if times.windows(2).any(|pair| pair[1] < pair[0]) {
        return Err("sample times must be nondecreasing".into());
    }
    let Some(&horizon) = times.last() else {
        return Ok(LifecycleReport::default());
    };
    if horizon > 60000 {
        return Err("Camera/thumbnail samples are limited to 60 seconds".into());
    }
    let budget = sample_byte_budget(effect);
    let mut on_frame = |packet: StreamFrame| -> Result<bool, String> {
        if crate::fx::memory_cost(&packet.frame) > budget {
            return Err("This effect exceeds the preview frame memory limit".into());
        }
        visit(packet.frame)?;
        Ok(true)
    };
    run_lifecycle(effect, seed, horizon, 1, Some(&mut on_frame), Some(times))
}

fn run_lifecycle(
    effect: &Value,
    seed: u32,
    horizon: u32,
    interval: u32,
    callback: Option<&mut dyn FnMut(StreamFrame) -> Result<bool, String>>,
    requested_times: Option<&[u32]>,
) -> Result<LifecycleReport, String> {
    let mut context = StreamContext {
        callback,
        report: LifecycleReport::default(),
        interval,
        next_sample: 0,
        cancelled: false,
        visual_layers: BTreeSet::new(),
        extra_samples: BTreeSet::new(),
        requested_times: requested_times.map(|times| times.iter().copied().collect()),
    };
    let has_trail = effect["elements"]
        .as_array()
        .ok_or("Missing elements")?
        .iter()
        .any(|element| {
            element.get("trail").is_some() && !flags(element, "editorFlags").contains("disabled")
        });
    if has_trail {
        let source_frames = if let Some(raw) = effect.get("previewSourceFrames") {
            let mut frames = Vec::new();
            for frame in raw.as_array().ok_or("Invalid controlled source frames")? {
                frames.push(crate::fx::trail_state::SourceFrame {
                    time: u32::try_from(frame["time"].as_u64().ok_or("Missing source time")?)
                        .map_err(|error| error.to_string())?,
                    origin: crate::fx::array(&frame["origin"], "source origin")?,
                });
            }
            Some(frames)
        } else {
            None
        };
        simulate_shared_trails_internal(
            effect,
            seed,
            &[horizon],
            source_frames.as_deref(),
            Some(&mut context),
        )?;
    } else {
        simulate_internal(effect, seed, &[horizon], None, Some(&mut context))?;
    }
    context.report.horizon_exhausted = context.report.completion_time.is_none()
        && !context.cancelled
        && context.report.examined_through == horizon;
    Ok(context.report)
}

/// Keep root trails on the same clock as ordinary particles and child births.
/// Child-owned trails remain explicitly rejected; this is not composition of
/// separately simulated timelines or replay of captured native geometry.
pub fn simulate_shared_trails(
    effect: &Value,
    seed: u32,
    times: &[u32],
    source_frames: Option<&[crate::fx::trail_state::SourceFrame]>,
) -> Result<Value, String> {
    simulate_shared_trails_internal(effect, seed, times, source_frames, None)
}

fn simulate_shared_trails_internal(
    effect: &Value,
    seed: u32,
    times: &[u32],
    source_frames: Option<&[crate::fx::trail_state::SourceFrame]>,
    mut streaming: Option<&mut StreamContext<'_>>,
) -> Result<Value, String> {
    validate_cloud_scope(effect)?;
    if let Some(dependencies) = effect["dependencies"].as_object() {
        for child in dependencies.values() {
            if child["elements"]
                .as_array()
                .ok_or("Missing child elements")?
                .iter()
                .any(|element| {
                    element.get("trail").is_some()
                        && !flags(element, "editorFlags").contains("disabled")
                })
            {
                return Err(
                    "Child-owned trails require a separate multi-instance pool proof".into(),
                );
            }
        }
    }
    let mut source = effect.clone();
    let horizon = times.last().copied().unwrap_or(0);
    if horizon > 60000 && streaming.is_none() {
        return Err("Shared trail preview horizon is limited to 60 seconds".into());
    }
    if let Some(keys) = source_frames {
        if keys.len() < 2
            || keys[0].time != 0
            || keys.last().unwrap().time < horizon
            || keys.windows(2).any(|pair| pair[0].time >= pair[1].time)
            || keys
                .iter()
                .flat_map(|key| key.origin)
                .any(|value| !value.is_finite())
            || source.get("previewEmitterFrame").is_some()
            || source.get("previewEmitterFrames").is_some()
            || source.get("previewEmitterKeyframes").is_some()
        {
            return Err("Shared trail source needs one finite increasing translation track".into());
        }
        let mut dense = Vec::new();
        let mut end = 1;
        for now in 0..=horizon {
            while keys[end].time < now {
                end += 1;
            }
            let a = &keys[end - 1];
            let b = &keys[end];
            // The declared native fixture writes these source positions as
            // f32 after its host's interpolation; share those exact inputs.
            let fraction = (now - a.time) as f64 / (b.time - a.time) as f64;
            let origin: [f32; 3] = std::array::from_fn(|axis| {
                (a.origin[axis] as f64 + (b.origin[axis] as f64 - a.origin[axis] as f64) * fraction)
                    as f32
            });
            dense.push(json!({"time":now,"origin":origin,"storedQuat":[0,0,0,1],"storedAxis":frames::IDENTITY}));
        }
        source["previewEmitterFrames"] = json!(dense);
    }
    let track = crate::fx::moving_frames::Timeline::from_effect(&source, horizon)?;
    let initial = if let Some(track) = &track {
        let initial = track.at(0);
        for now in 0..=horizon {
            if track.at(now).axis != frames::IDENTITY {
                return Err(
                    "Shared trail proof currently requires identity-oriented source frames".into(),
                );
            }
        }
        (initial.origin, initial.axis)
    } else {
        preview_emitter_frame(&source)?
    };
    let mut trail = crate::fx::trail_state::State::new(&source, seed, 0, initial.0, initial.1)?;
    if streaming.is_some() {
        trail.enable_streaming_clock();
    }
    let trail_compiled = trail.compiled();
    let mut copy_sorts = source["copySortOrders"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for element in source["elements"]
        .as_array_mut()
        .ok_or("Missing elements")?
    {
        if element.get("trail").is_some() && !flags(element, "editorFlags").contains("disabled") {
            let sort = tokens(element, "sortOrder")
                .first()
                .map(|value| value.parse::<i32>())
                .transpose()
                .map_err(|_| "Invalid trail sort order")?
                .unwrap_or(5)
                .clamp(0, 255);
            copy_sorts.push(json!(sort));
            element["editorFlags"]
                .as_array_mut()
                .ok_or("Missing trail editor flags")?
                .push(json!("disabled"));
        }
    }
    source["copySortOrders"] = json!(copy_sorts);
    let mut playback = simulate_internal(
        &source,
        seed,
        times,
        Some(&mut trail),
        streaming.as_deref_mut(),
    )?;
    let compiled = playback["compiled"]
        .as_array_mut()
        .ok_or("Missing compiled definitions")?;
    compiled.extend(trail_compiled);
    compiled.sort_by_key(|entry| entry["sourceIndex"].as_u64());
    playback["sharedRootTrailTimeline"] = json!(true);
    Ok(playback)
}

fn simulate_internal(
    effect: &Value,
    seed: u32,
    times: &[u32],
    mut root_trail: Option<&mut crate::fx::trail_state::State>,
    mut streaming: Option<&mut StreamContext<'_>>,
) -> Result<Value, String> {
    validate_cloud_scope(effect)?;
    if seed >= 479 {
        return Err("effect seed must be in 0..479".into());
    }
    if times.windows(2).any(|pair| pair[1] < pair[0]) {
        return Err("sample times must be nondecreasing; restart with a new simulation".into());
    }
    // This environment is deliberate, rather than silently replacing authored
    // influence. Original EffectsEd3 starts with wind [0,0,0] (other environment
    // parameters [0,1,0]); its complete native wind branch proves no force for
    // every tested nonzero influence. A nonzero requested environment is outside
    // this bounded player and must fail instead of being ignored.
    if let Some(wind) = effect.get("previewWind") {
        let values = wind
            .as_array()
            .ok_or("previewWind must contain three zero numbers")?;
        if values.len() != 3 || values.iter().any(|value| value.as_f64() != Some(0.0)) {
            return Err("preview preview supports only fixed zero editor wind".into());
        }
    }
    let camera = if let Some(raw) = effect.get("previewCamera") {
        let values = raw
            .as_array()
            .ok_or("previewCamera must contain three finite numbers")?;
        if values.len() != 3 {
            return Err("previewCamera must contain three finite numbers".into());
        }
        let mut camera = [0.0; 3];
        for axis in 0..3 {
            camera[axis] = values[axis].as_f64().ok_or("invalid preview camera")? as f32;
            if !camera[axis].is_finite() {
                return Err("invalid preview camera".into());
            }
        }
        camera
    } else {
        [200.0, 0.0, 0.0]
    };
    let camera_axis = crate::fx::camera::parse_axis(effect.get("previewCameraAxis"))?;
    let horizon = times.last().copied().unwrap_or(0);
    if horizon > 60000 && streaming.is_none() {
        return Err("preview horizon limited to 60 seconds".into());
    }
    // Owned lifecycle control for comparing native StopEffect. It clears
    // future root looping births; already allocated particles finish normally.
    let stop_time = effect
        .get("previewStopTime")
        .map(|value| {
            value
                .as_u64()
                .filter(|value| *value <= 60000)
                .map(|value| value as i64)
                .ok_or_else(|| {
                    "previewStopTime must be an unsigned millisecond value <= 60000".to_string()
                })
        })
        .transpose()?;
    let root_name = canonical(effect["name"].as_str().unwrap_or("root"));
    let copy_sorts = |source: &Value| -> Result<Vec<u8>, String> {
        match source.get("copySortOrders") {
            None => Ok(Vec::new()),
            Some(values) => values
                .as_array()
                .ok_or("invalid copied-class sort metadata")?
                .iter()
                .map(|value| {
                    value
                        .as_u64()
                        .filter(|value| *value <= 255)
                        .map(|value| value as u8)
                        .ok_or("invalid copied-class sort order".into())
                })
                .collect(),
        }
    };
    let mut copy_sort_orders = BTreeMap::<String, Vec<u8>>::new();
    copy_sort_orders.insert(root_name.clone(), copy_sorts(effect)?);
    if let Some(dependencies) = effect["dependencies"].as_object() {
        for (name, child) in dependencies {
            copy_sort_orders.insert(canonical(name), copy_sorts(child)?);
        }
    }
    let mut definitions = BTreeMap::<String, Vec<Definition>>::new();
    definitions.insert(
        root_name.clone(),
        compile_effect(
            effect,
            times.is_empty() && effect.get("materialInfo").is_none(),
        )?,
    );
    if let Some(dependencies) = effect["dependencies"].as_object() {
        for (name, child) in dependencies {
            let name = canonical(name);
            if name == root_name {
                continue;
            }
            let mut context = child.clone();
            if context.get("previewPackedLighting").is_none() {
                if let Some(packed) = effect.get("previewPackedLighting") {
                    context["previewPackedLighting"] = packed.clone();
                }
            }
            definitions.insert(
                name.clone(),
                compile_effect(
                    &context,
                    times.is_empty() && child.get("materialInfo").is_none(),
                )
                .map_err(|error| format!("child {name}: {error}"))?,
            );
        }
    }
    let spot_definitions = definitions
        .iter()
        .flat_map(|(name, layers)| {
            layers
                .iter()
                .filter(|layer| layer.kind == "spotLight")
                .map(move |layer| (name, layer))
        })
        .collect::<Vec<_>>();
    if spot_definitions.len() > 1 || spot_definitions.iter().any(|(name, _)| **name != root_name) {
        return Err("The native selected-spotlight checkpoint supports one root spotlight definition; child/multiple spotlight replacement remains unverified".into());
    }
    for (name, layers) in &definitions {
        for layer in layers {
            // Type 9/10 exits before consulting child handles. Compiled sort
            // metadata remains intact, but no child runtime assets are needed.
            if layer.sound || layer.kind == "decal" {
                continue;
            }
            for child in layer
                .children
                .iter()
                .chain(layer.death.iter())
                .chain(layer.emission.iter())
            {
                if !definitions.contains_key(child) {
                    return Err(format!("{name}: missing child dependency {child}"));
                }
            }
            // These children allocate synchronously inside a particle update.
            // Nested runners can consume the host stream before the parent
            // resumes its next interval. That re-entrant ordering is a separate
            // check, so do not accept such a tree through this bounded path.
            for child in layer.death.iter().chain(layer.emission.iter()) {
                if !bounded_nested_graph(effect)
                    && !nested_diagnostic(effect)
                    && definitions[child].iter().any(|nested| {
                        !nested.children.is_empty()
                            || nested.death.is_some()
                            || nested.emission.is_some()
                    })
                {
                    return Err(format!("{name}: synchronous child {child} contains an unverified nested child tree"));
                }
            }
            // Native startup reads the original child definition's looping and
            // one-shot classes. The parent's extra emitted-definition copies
            // are not substituted for that pointer (see emitted-checks).
        }
    }
    let moving_frames = crate::fx::moving_frames::Timeline::from_effect(effect, horizon)?;
    if moving_frames.is_some() {
        // Moving child frames have native evidence for one generation only,
        // including graphs that contain no clouds.
        if definitions.iter().any(|(name, layers)| {
            name != &root_name
                && layers.iter().any(|layer| {
                    !layer.children.is_empty() || layer.death.is_some() || layer.emission.is_some()
                })
        }) {
            return Err("Moving-frame preview supports one child generation; nested dispatch remains unverified".into());
        }
        if definitions.iter().any(|(name,layers)| layers.iter().any(|layer| {
            (!layer.sound && !["cloud", "billboardSprite", "orientedSprite", "tail", "line", "runner"].contains(&layer.kind.as_str()))
                || layer.bits & 0x30 != 0 || layer.bits & 0xC0 == 0xC0
                  // Unbolted children retain a static allocation frame. The
                  // shared raygun proof exercises their native gravity kernel;
                  // this does not admit gravity on the moving root itself.
                  || (layer.gravity != [0.0, 0.0]
                      && !(root_trail.is_some() && name != &root_name))
                || (layer.death.is_some() && layer.emission.is_some())
        })) {
            return Err("Moving-frame preview requires simple free-flight visuals, no offsets/forces or combined death/emission dispatch".into());
        }
    }
    let (root_origin, root_axis) = if let Some(track) = &moving_frames {
        let initial = track.at(0);
        (initial.origin, initial.axis)
    } else {
        preview_emitter_frame(effect)?
    };
    let compiled: Vec<Value> = definitions[&root_name]
        .iter()
        .map(definition_json)
        .collect();
    let dependency_compiled: BTreeMap<String, Vec<Value>> = definitions
        .iter()
        .filter(|(name, _)| **name != root_name)
        .map(|(name, layers)| (name.clone(), layers.iter().map(definition_json).collect()))
        .collect();
    if times.is_empty() {
        return Ok(
            json!({"name":effect["name"], "compiled":compiled, "dependencyCompiled":dependency_compiled,
            "frames":[], "particles":[], "effectInstances":[]}),
        );
    }
    let root = Instance {
        serial: 0,
        retired: false,
        name: root_name.clone(),
        seed,
        begin: 0,
        origin: root_origin,
        axis: root_axis,
        copy_sort_orders: copy_sort_orders[&root_name].clone(),
        sort: 255,
        ancestry: vec![root_name.clone()],
    };
    let mut queue = BTreeMap::<u32, VecDeque<Request>>::new();
    let queue_horizon = if streaming.is_some() {
        STREAM_SCHEDULE
    } else {
        horizon
    };
    let initial = queue_instance(
        0,
        &root,
        &definitions[&root_name],
        0,
        queue_horizon,
        &mut queue,
    )?;
    queue.entry(0).or_default().extend(initial);
    let mut instances = vec![root];
    let mut effect_order = vec![0_usize];
    let mut host_random_state = seed;
    // Mixed-cloud fixtures use the original per-tick stored positions for
    // every root particle. Analytic displacement loses native f32 stores.
    let root_cloud_motion = definitions
        .values()
        .flatten()
        .any(|layer| layer.kind == "cloud");
    let mut active = Vec::<Particle>::new();
    let mut frames = Vec::<Value>::new();
    let mut all_particles = Vec::<Value>::new();
    let mut child_requests = Vec::<Value>::new();
    let mut bounds = [[f32::INFINITY; 2], [f32::NEG_INFINITY; 2]];
    let mut next_sample = 0;
    let mut total_requests = 0;
    let compact_output =
        streaming.is_some() || effect["compactPreviewFrames"].as_bool() == Some(true);
    let frame_budget = effect["previewFrameByteBudget"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok());
    let mut retained_frame_bytes = 0_usize;
    if let (Some(context), Some(trail)) = (streaming.as_deref_mut(), root_trail.as_deref()) {
        context
            .report
            .sample_times
            .extend(trail.lifecycle_samples());
    }
    for now in 0..=horizon {
        if now > 0 {
            if let Some(trail) = root_trail.as_deref_mut() {
                let origin = moving_frames
                    .as_ref()
                    .map(|track| track.at(now).origin)
                    .unwrap_or(root_origin);
                trail.update(
                    now,
                    origin,
                    stop_time.is_some_and(|stop| now as i64 >= stop),
                )?;
            }
        }
        let mut pending = queue.remove(&now).unwrap_or_default();
        let mut events = Vec::new();
        for particle in &mut active {
            let definition = &definitions[&instances[particle.effect].name][particle.layer];
            if let Some(track) = &moving_frames {
                // Native unbolted children keep their own allocation frame;
                // the source motion track belongs only to the root effect.
                if particle.effect == 0 {
                    particle.cloud_current_frame = Some(track.at(now));
                }
            }
            let end = particle.birth + particle.life;
            let begin = (now as i64 - 1).max(particle.birth);
            if now > 0 && begin < end && now as i64 >= particle.birth {
                advance_emitter(definition, particle, (now as i64).min(end) - particle.birth);
                if let Some(child) = &definition.emission {
                    for (scheduled, origin, word) in emission_events(
                        definition,
                        particle,
                        begin,
                        (now as i64).min(end),
                        &mut host_random_state,
                    )? {
                        events.push(ChildEvent {
                            parent: particle.effect,
                            name: child.clone(),
                            scheduled,
                            origin,
                            axis: frames::emission_axis({
                                let from = particle_position(
                                    definition,
                                    particle,
                                    (begin - particle.birth) as f32,
                                );
                                let to = emission_end_position(
                                    definition,
                                    particle,
                                    ((now as i64).min(end) - particle.birth) as f32,
                                );
                                std::array::from_fn(|axis| to[axis] - from[axis])
                            })?,
                            sort: 255,
                            source: definition.source,
                            kind: "emission",
                            random_word: Some(word),
                        });
                    }
                }
            }
            if now as i64 >= end {
                if let Some(child) = &definition.death {
                    // Without emission the native terminal update discards the
                    // particle before movement; emission updates it to its end.
                    let age = if definition.emission.is_some() {
                        particle.life
                    } else {
                        (begin - particle.birth).max(0)
                    };
                    events.push(ChildEvent {
                        parent: particle.effect,
                        name: child.clone(),
                        scheduled: begin,
                        origin: particle_position(definition, particle, age as f32),
                        axis: if let Some(track) = &moving_frames {
                            if particle.effect == 0
                                && definition.bits & 0xC0 == 0x80
                                && definition.emission.is_none()
                            {
                                // The original death-without-emission route
                                // exits before the terminal current-frame update.
                                track.at(begin as u32).axis
                            } else {
                                orientation_axis(definition, particle)
                            }
                        } else {
                            orientation_axis(definition, particle)
                        },
                        sort: 255,
                        source: definition.source,
                        kind: "death",
                        random_word: Some(host_word(&mut host_random_state)),
                    });
                }
            }
        }
        active.retain(|particle| (now as i64) < particle.birth + particle.life);
        for event in events {
            allocate_child(
                event,
                &mut instances,
                &mut effect_order,
                &definitions,
                &copy_sort_orders,
                &mut host_random_state,
                &mut queue,
                &mut pending,
                &mut child_requests,
                now,
                queue_horizon,
            )?;
        }
        let mut new_particles = Vec::new();
        let mut tick_requests = 0_usize;
        while let Some(request) = pending.pop_front().or_else(|| {
            // A newly started child can already have its next one-millisecond
            // looping birth due now. Drain it in this tick, never strand a
            // past queue key and falsely keep a finished graph alive forever.
            if streaming.is_some() {
                if let Some(mut due) = queue.remove(&now) {
                    pending.append(&mut due);
                }
            }
            pending.pop_front()
        }) {
            total_requests += 1;
            tick_requests += 1;
            if streaming.is_some() && tick_requests > 20000 {
                return Err("The per-tick particle request capacity was exceeded".into());
            }
            if streaming.is_none() && total_requests > 20000 {
                return Err("preview spawn request limit exceeded".into());
            }
            let instance = instances[request.effect].clone();
            let layers = &definitions[&instance.name];
            let definition = &layers[request.layer];
            // Original EffectsEd FX_SpawnElem returns before pool allocation
            // for both type 9 (decal) and type 10 (sound). Keep each compiled
            // definition for scheduling/sort metadata, but create no visual.
            if definition.sound || definition.kind == "decal" {
                continue;
            }
            if request.effect == 0
                && definition.looping
                && stop_time.is_some_and(|stop| request.scheduled >= stop)
            {
                continue;
            }
            if streaming.is_some() {
                queue_next_loop(&request, definition, &instances, &mut queue, now)?;
            }
            let mut particle = spawn(
                layers,
                request.layer,
                request.sequence,
                request.scheduled,
                instance.seed,
            );
            particle.effect = request.effect;
            // Original 43CBC0 culls stale births before motion initialization
            // only when no impact, death or emission callback is installed.
            // Runners dispatch before this check; callbacks must still allocate
            // and reach the normal reap path even when sampled life is zero.
            if definition.kind != "runner"
                && definition.impact.is_none()
                && definition.death.is_none()
                && definition.emission.is_none()
                && particle.birth + particle.life <= now as i64
            {
                continue;
            }
            let placement = moving_frames
                .as_ref()
                .filter(|_| request.effect == 0)
                .map(|track| {
                    // Spawn-relative placement uses the owning EFFECT's initial
                    // frame. Other modes use the request's frameWhenPlayed;
                    // authored spawn delay does not change that allocation frame.
                    let time = if definition.bits & 0xC0 == 0x40 {
                        0
                    } else {
                        request.scheduled.max(0).min(horizon as i64) as u32
                    };
                    if definition.looping && request.sequence > 0 && definition.bits & 0xC0 != 0x40
                    {
                        track.when_played(time)
                    } else {
                        track.at(time)
                    }
                });
            let origin = placement
                .map(|frame| frame.origin)
                .unwrap_or(instance.origin);
            let axis = placement.map(|frame| frame.axis).unwrap_or(instance.axis);
            initialize_particle_frame(definition, &mut particle, origin, axis);
            let offset_emitter = definition.emission.is_some() && definition.bits & 0x30 != 0;
            if offset_emitter {
                if moving_frames.is_some() || request.effect != 0 {
                    return Err(
                        "Nonzero offset emission currently requires a static root frame".into(),
                    );
                }
                initialize_cloud_motion(definition, &mut particle)?;
                prime_negative_root_birth(definition, &mut particle, now);
                let initial_age = (now as i64 - particle.birth).max(0).min(particle.life);
                advance_emitter(definition, &mut particle, initial_age);
            }
            if !offset_emitter && particle.cloud_motion.is_some() {
                prime_negative_root_birth(definition, &mut particle, now);
                let initial_age = (now as i64 - particle.birth).max(0).min(particle.life);
                advance_emitter(definition, &mut particle, initial_age);
            }
            if !offset_emitter && moving_frames.is_none() && definition.emission.is_some()
                && instance.axis == frames::IDENTITY
                && instance.origin == [0.0; 3]
                // Native captures prove zero-sized cylinder offsets take the
                // same incremental emitter path. Preserve the authored flags;
                // nonzero offsets and offset-relative frames remain guarded.
                && (definition.bits & 0x30 == 0
                    || (definition.bits & 0xC0 != 0xC0
                        && definition.offset_radius == [0.0; 2]
                        && definition.offset_height == [0.0; 2]))
            {
                particle.origin = std::array::from_fn(|axis| {
                    (definition.origin[axis][0] as f64
                        + definition.origin[axis][1] as f64
                            * random(particle.seed, 6 + axis) as f64) as f32
                });
                particle.motion = Some(MotionState {
                    age: 0,
                    previous_age: 0,
                    position: particle.origin,
                    previous_position: particle.origin,
                    gravity_velocity: 0.0,
                });
                prime_negative_root_birth(definition, &mut particle, now);
                let initial_age = (now as i64 - particle.birth).max(0).min(particle.life);
                advance_emitter(definition, &mut particle, initial_age);
            }
            // Native updates retain position/base velocity even for ordinary
            // static sprites. Real snow-footstep captures expose drift when
            // gravity is instead recomputed analytically from total age.
            let persistent_static_motion = moving_frames.is_none()
                && !matches!(definition.kind.as_str(), "runner" | "sound")
                // Identity sphere/cylinder births already provide a stored
                // world point. Native gravity still advances it each update;
                // an analytic total-age fall shifts real electrical FX pixels.
                && definition.bits & 0xC0 != 0xC0
                && definition.emission.is_none()
                && ((instance.axis == frames::IDENTITY && instance.origin == [0.0; 3])
                    // The real flare's translated/rotated emitted children
                    // use retained per-ms free-flight state too. This bounded
                    // proof covers world-relative children without offsets.
                    || (request.effect != 0
                        && definition.bits & 0x30 == 0
                        && definition.bits & 0xC0 == 0))
                && (definition.gravity != [0.0; 2] || definition.runtime.contains("useCollision")
                    || (request.effect == 0 && definition.delay[0] < 0)
                    || matches!(definition.kind.as_str(), "light" | "spotLight"));
            let analytic_offset = definition.bits & 0xC0 == 0xC0
                && particle.cloud_motion.is_none()
                && definition.emission.is_none()
                && definition.gravity == [0.0; 2]
                && !definition.runtime.contains("useCollision");
            if (root_cloud_motion || moving_frames.is_some() || persistent_static_motion)
                && !analytic_offset
                && definition.kind != "runner"
            {
                if particle.cloud_motion.is_some()
                    || definition.kind == "cloud"
                    || moving_frames.is_some()
                    || persistent_static_motion
                {
                    // The shared native free-flight updater stores local/world
                    // positions identically for these simple particle types.
                    // Legacy static routes remain separate for regression checks.
                    initialize_cloud_motion(definition, &mut particle)?;
                    if let Some(track) = &moving_frames {
                        if request.effect == 0 {
                            particle.cloud_current_frame = Some(track.at(now));
                        }
                    }
                } else {
                    if instance.axis != frames::IDENTITY
                        || (definition.bits & 0xC0 != 0 && instance.origin != [0.0; 3])
                    {
                        return Err(
                            "Transformed non-cloud companions in a cloud graph remain unverified"
                                .into(),
                        );
                    }
                    if definition.bits & 0x30 == 0 {
                        particle.origin = std::array::from_fn(|axis| {
                            (definition.origin[axis][0] as f64
                                + definition.origin[axis][1] as f64
                                    * random(particle.seed, 6 + axis) as f64)
                                as f32
                        });
                    }
                    let world_origin = std::array::from_fn(|axis| {
                        (particle.origin[axis] as f64 + particle.frame_origin[axis] as f64) as f32
                    });
                    particle.motion = Some(MotionState {
                        age: 0,
                        previous_age: 0,
                        position: world_origin,
                        previous_position: world_origin,
                        gravity_velocity: 0.0,
                    });
                }
                prime_negative_root_birth(definition, &mut particle, now);
                let initial_age = (now as i64 - particle.birth).max(0).min(particle.life);
                advance_emitter(definition, &mut particle, initial_age);
            }
            if definition.kind == "runner" {
                let selected =
                    (definition.children.len() * random_word(particle.seed, 21) as usize) >> 16;
                let child_name = &definition.children[selected];
                if instance.ancestry.contains(child_name) || instance.ancestry.len() >= 16 {
                    return Err(format!(
                        "runner cycle/depth budget reached: {} -> {child_name}",
                        instance.name
                    ));
                }
                let (index, serial) = instance_slot(&instances, streaming.is_some())?;
                // FX_SetEffectRandomSeed uses 479 * rand() / 32768. The preview
                // supplies an owned deterministic CRT-compatible host stream.
                host_random_state = host_random_state.wrapping_mul(214013).wrapping_add(2531011);
                let random_word = (host_random_state >> 16) & 32767;
                let child_seed = 479 * random_word / 32768;
                let (origin, child_axis) = if moving_frames.is_some() {
                    let birth_frame = placement.unwrap_or(crate::fx::cloud_motion::Frame {
                        origin: instance.origin,
                        axis: instance.axis,
                    });
                    let local = std::array::from_fn(|axis| {
                        (definition.origin[axis][0] as f64
                            + definition.origin[axis][1] as f64
                                * random(particle.seed, 6 + axis) as f64)
                            as f32
                    });
                    let position_frame = crate::fx::cloud_motion::Frame {
                        origin: birth_frame.origin,
                        axis: if definition.runtime.contains("spawnRelative") {
                            birth_frame.axis
                        } else {
                            frames::IDENTITY
                        },
                    };
                    (
                        crate::fx::cloud_motion::spawn_to_world(position_frame, local),
                        birth_frame.axis,
                    )
                } else {
                    (
                        std::array::from_fn(|axis| instance.origin[axis] + particle.origin[axis]),
                        instance.axis,
                    )
                };
                let mut ancestry = instance.ancestry.clone();
                ancestry.push(child_name.clone());
                let child = Instance {
                    serial,
                    retired: false,
                    name: child_name.clone(),
                    seed: child_seed,
                    begin: particle.birth,
                    origin,
                    axis: frames::stored_axis(child_axis),
                    copy_sort_orders: copy_sort_orders[child_name].clone(),
                    sort: definition.sort,
                    ancestry,
                };
                let initial = queue_instance(
                    index,
                    &child,
                    &definitions[child_name],
                    now,
                    queue_horizon,
                    &mut queue,
                )?;
                child_requests.push(json!({"parent":request.effect,"effect":index,"name":child_name,
                    "scheduled":particle.birth,"origin":origin,"axis":child_axis,"storedAxis":child.axis,"seed":child_seed,"hostRandomWord":random_word,
                    "runnerElementSeed":particle.seed,"sourceIndex":definition.source}));
                store_instance(&mut instances, &mut effect_order, index, child);
                // Native child StartNewEffect runs before the parent resumes
                // its next definition. Preserve that allocation/random order.
                for item in initial.into_iter().rev() {
                    pending.push_front(item);
                }
                continue;
            }
            if let Some(context) = streaming.as_deref_mut() {
                let middle = particle
                    .birth
                    .saturating_add(particle.life.max(0) / 2)
                    .max(0);
                if context.report.sample_times.len() < 128
                    && context
                        .visual_layers
                        .insert((instance.serial, request.layer))
                {
                    if let Ok(time) = u32::try_from(middle) {
                        context.report.sample_times.push(time);
                    }
                }
                if context.callback.is_some() && context.requested_times.is_none() {
                    for time in [particle.birth.max(now as i64), middle] {
                        if let Ok(time) = u32::try_from(time) {
                            if time >= now {
                                context.extra_samples.insert(time);
                            }
                        }
                    }
                }
            }
            all_particles.push(json!({"effect":request.effect,"effectName":instance.name,
                "layer":request.layer,"sourceIndex":definition.source,"sequence":particle.sequence,
                "scheduled":particle.scheduled,"birth":particle.birth,"life":particle.life,
                "seed":particle.seed,"localOrigin":particle.origin,"frameOrigin":particle.frame_origin}));
            new_particles.push(particle);
        }
        // Native allocation prepends. Each effect retains its own persistent
        // sorted particle list; particles from different effects never mix.
        let mut new_clouds = Vec::new();
        for particle in new_particles.into_iter().rev() {
            if active.len() + new_clouds.len() >= 2048 {
                return Err("preview active particle limit exceeded".into());
            }
            let layers = &definitions[&instances[particle.effect].name];
            if matches!(
                layers[particle.layer].kind.as_str(),
                "model" | "cloud" | "light" | "spotLight"
            ) {
                new_clouds.push(particle);
                continue;
            }
            let sort = layers[particle.layer].sort;
            let position = particle_position(
                &layers[particle.layer],
                &particle,
                (now as i64 - particle.birth).max(0) as f32,
            );
            let distance = particle_sort_distance(position, camera);
            let index = active
                .iter()
                .position(|existing| {
                    if existing.effect != particle.effect {
                        return false;
                    }
                    // Class-1 lights and class-2 clouds have separate prepend-only native
                    // list; they must never participate in sprite insertion.
                    if matches!(
                        layers[existing.layer].kind.as_str(),
                        "model" | "cloud" | "light" | "spotLight"
                    ) {
                        return false;
                    }
                    let other = particle_position(
                        &layers[existing.layer],
                        existing,
                        (now as i64 - existing.birth).max(0) as f32,
                    );
                    let other_distance = particle_sort_distance(other, camera);
                    layers[existing.layer].sort > sort
                        || (layers[existing.layer].sort == sort && other_distance <= distance)
                })
                .unwrap_or(active.len());
            active.insert(index, particle);
        }
        // Lights/clouds use native WaW classes 1/2, prepend-only lists. They never enter the
        // sprite priority/distance insertion sorter. Reverse this temporary
        // collection back to allocation order before reproducing prepends.
        for particle in new_clouds.into_iter().rev() {
            active.insert(0, particle);
        }
        // Sorting is an update operation, not dependent on requested screenshots.
        // Preserve last tick's order rather than recreating allocation order.
        let mut order = Vec::<usize>::new();
        for index in effect_order.iter().copied() {
            let origin = if index == 0 {
                moving_frames
                    .as_ref()
                    .map(|track| track.at(now).origin)
                    .unwrap_or(instances[index].origin)
            } else {
                instances[index].origin
            };
            let distance = distance_squared(origin, camera);
            let mut insertion = order.len();
            while insertion > 0 {
                let previous = order[insertion - 1];
                let origin = if previous == 0 {
                    moving_frames
                        .as_ref()
                        .map(|track| track.at(now).origin)
                        .unwrap_or(instances[previous].origin)
                } else {
                    instances[previous].origin
                };
                let previous_distance = distance_squared(origin, camera);
                if previous_distance >= distance * 0.99989998 {
                    if previous_distance >= distance * 1.0001
                        || !first_effect_is_further(index, previous, &mut instances, &definitions)
                    {
                        break;
                    }
                }
                insertion -= 1;
            }
            order.insert(insertion, index);
        }
        effect_order = order;
        let sample_count = if let Some(context) = streaming.as_deref_mut() {
            context.report.examined_through = now;
            context.report.host_random_state = host_random_state;
            context.report.naturally_infinite =
                instances.iter().enumerate().any(|(index, instance)| {
                    !instance.retired
                        && (index != 0 || stop_time.is_none())
                        && definitions[&instance.name].iter().any(|layer| {
                            layer.looping
                                && !layer.sound
                                && layer.kind != "decal"
                                && layer.spawn[1] == i64::from(i32::MAX)
                        })
                }) || (stop_time.is_none()
                    && root_trail
                        .as_deref()
                        .is_some_and(|trail| trail.naturally_infinite()));
            let completed = active.is_empty()
                && queue.is_empty()
                && root_trail
                    .as_deref()
                    .is_none_or(|trail| trail.is_complete());
            if completed && context.report.completion_time.is_none() {
                context.report.completion_time = Some(now);
            }
            // Only reclaim a slot after its actual children have been started,
            // particles reaped and future looping requests exhausted.
            let occupied = active
                .iter()
                .map(|particle| particle.effect)
                .chain(
                    queue
                        .values()
                        .flat_map(|requests| requests.iter().map(|request| request.effect)),
                )
                .collect::<BTreeSet<_>>();
            for (index, instance) in instances.iter_mut().enumerate().skip(1) {
                instance.retired = !occupied.contains(&index);
            }
            effect_order.retain(|index| *index == 0 || !instances[*index].retired);
            if let Some(requested) = context.requested_times.as_mut() {
                let count = requested.iter().take_while(|time| **time == now).count();
                requested.drain(..count);
                count
            } else {
                let extra = context.extra_samples.remove(&now);
                usize::from(
                    context.callback.is_some()
                        && (now >= context.next_sample || extra || completed),
                )
            }
        } else {
            times[next_sample..]
                .iter()
                .take_while(|time| **time == now)
                .count()
        };
        for _ in 0..sample_count {
            let order = effect_order.clone();
            let mut draws = Vec::new();
            let mut particles = Vec::new();
            let mut cloud_count = 0;
            let mut lights = Vec::new();
            let mut models = Vec::new();
            let mut selected_spot = None;
            for instance_index in &order {
                let instance = &instances[*instance_index];
                let layers = &definitions[&instance.name];
                // EffectsEd submits the class-0 sprite list before class-2
                // non-sprites. This is submission order; material render-queue
                // ordering is checked independently by the graphics fixture.
                for visual_class in [0, 1, 2] {
                    for particle in active.iter().filter(|particle| {
                        particle.effect == *instance_index
                            && match layers[particle.layer].kind.as_str() {
                                "model" | "light" | "spotLight" => visual_class == 1,
                                "cloud" => visual_class == 2,
                                _ => visual_class == 0,
                            }
                    }) {
                        let definition = &layers[particle.layer];
                        if (now as i64) < particle.birth {
                            continue;
                        }
                        if definition.kind == "model" {
                            if let Some(mut model) = model_particle(definition, particle, now) {
                                model["effect"] = json!(particle.effect);
                                model["effectName"] = json!(instance.name);
                                model["bounds"] = effect["modelInfo"]
                                    [model["model"].as_str().unwrap()]["bounds"]
                                    .clone();
                                models.push(model);
                            }
                            continue;
                        }
                        if matches!(definition.kind.as_str(), "light" | "spotLight") {
                            // Native type 7 submits a scene light, never a quad.
                            // EffectsEd's owned scene pool retains the first 32
                            // positive-radius submissions, clamping only radius.
                            if lights.len() < 32 {
                                if let Some(mut light) =
                                    light_particle(definition, particle, now, camera)
                                {
                                    light["effect"] = json!(particle.effect);
                                    light["effectName"] = json!(instance.name);
                                    if definition.kind == "spotLight" {
                                        // Original 0x453260 submits its selected
                                        // system spotlight after ordinary FX.
                                        selected_spot = Some(light);
                                    } else {
                                        lights.push(light);
                                    }
                                }
                            }
                            continue;
                        }
                        if definition.materials.is_empty() {
                            continue;
                        }
                        let (draw, mut diagnostic, points) =
                            draw_particle(definition, particle, now, camera, camera_axis)?;
                        if draw.is_null() {
                            continue;
                        }
                        if definition.kind == "cloud" {
                            cloud_count += 1;
                            if cloud_count > 256 {
                                return Err(
                                    "Native scene cloud submission limit of 256 exceeded".into()
                                );
                            }
                        }
                        diagnostic["effect"] = json!(particle.effect);
                        diagnostic["effectName"] = json!(instance.name);
                        for point in points {
                            for axis in 0..2 {
                                bounds[0][axis] = bounds[0][axis].min(point[axis + 1]);
                                bounds[1][axis] = bounds[1][axis].max(point[axis + 1]);
                            }
                        }
                        draws.push(draw);
                        particles.push(diagnostic);
                    }
                }
            }
            if lights.len() < 32 {
                if let Some(spot) = selected_spot {
                    lights.push(spot);
                }
            }
            let trail_frame = root_trail
                .as_deref()
                .map(|trail| trail.draw(now, camera))
                .transpose()?;
            if let Some(trail) = &trail_frame {
                // Original global traversal completes every instance's normal
                // sprite pass before collected trail passes. Only the root
                // owns trails in this bounded shared-pool proof.
                draws.extend(
                    trail["draws"]
                        .as_array()
                        .ok_or("Missing trail draws")?
                        .iter()
                        .cloned(),
                );
            }
            let mut frame = json!({"name":effect["name"],"time":now,"background":0xff202020_u32,
                "draws":draws,"particles":particles,"effectOrder":order});
            if let Some(trail) = trail_frame {
                frame["trailParticles"] = trail["particles"].clone();
                frame["trailMeshes"] = trail["trailMeshes"].clone();
                frame["drawPasses"] = json!(["all-instance sprites", "root trails"]);
                frame["activeTrailPoints"] = json!(root_trail.as_deref().unwrap().point_count());
            }
            if !models.is_empty() {
                frame["models"] = json!(models);
            }
            if definitions
                .values()
                .flatten()
                .any(|layer| matches!(layer.kind.as_str(), "light" | "spotLight"))
            {
                frame["lights"] = json!(lights);
                frame["lightPolicy"] = json!({"type":"omni", "limit":32,
                    "maximumRadius":2000.0,"culling":false});
                if !spot_definitions.is_empty() {
                    frame["lightPolicy"]["selectedSpotlight"] = json!(true);
                    frame["lightPolicy"]["spotScope"] =
                        json!("one non-overlapping root spotlight definition");
                    let allocated = active
                        .iter()
                        .filter(|particle| {
                            definitions[&instances[particle.effect].name][particle.layer].kind
                                == "spotLight"
                        })
                        .count();
                    frame["spotState"] = json!({"activeAllocations":allocated});
                }
            }
            if let Some(packed) = effect.get("previewPackedLighting") {
                frame["previewPackedLighting"] = packed.clone();
            }
            if let Some(plane) = effect.get("previewPlane") {
                frame["previewPlane"] = plane.clone();
                frame["background"] = json!(0xff191e23_u32);
            }
            if compact_output {
                crate::fx::compact_frame(&mut frame);
            }
            if let Some(budget) = frame_budget.filter(|_| streaming.is_none()) {
                retained_frame_bytes =
                    retained_frame_bytes.saturating_add(crate::fx::memory_cost(&frame));
                if retained_frame_bytes > budget {
                    return Err("This effect exceeds the preview memory limit".into());
                }
            }
            if let Some(context) = streaming.as_deref_mut() {
                if now >= context.next_sample {
                    context.next_sample = now.saturating_add(context.interval);
                }
                if let Some(callback) = context.callback.as_deref_mut() {
                    if !callback(StreamFrame {
                        frame,
                        lifecycle: context.report.clone(),
                    })? {
                        context.cancelled = true;
                    }
                }
            } else {
                frames.push(frame);
            }
            next_sample += 1;
        }
        if compact_output {
            // These report-only histories do not influence births or sorting.
            all_particles.clear();
            child_requests.clear();
        }
        if streaming.as_deref().is_some_and(|context| {
            context.cancelled
                || (context.report.completion_time.is_some() && context.requested_times.is_none())
                || (context.callback.is_none()
                    && context.report.naturally_infinite
                    && !context.report.sample_times.is_empty())
        }) {
            break;
        }
    }
    let mut radius = 1.0_f32;
    let mut center = [0.0; 2];
    if bounds[0][0].is_finite() {
        for axis in 0..2 {
            radius = radius.max((bounds[1][axis] - bounds[0][axis]) * 0.6);
            center[axis] = (bounds[1][axis] + bounds[0][axis]) * 0.5;
        }
    }
    let matrix = json!([
        [0.0, 1.0 / radius, 0.0, -center[0] / radius],
        [0.0, 0.0, 1.0 / radius, -center[1] / radius],
        [0.0, 0.0, 0.0, 0.5],
        [0.0, 0.0, 0.0, 1.0]
    ]);
    for frame in &mut frames {
        frame["matrixRows"] = matrix.clone();
    }
    let instance_diagnostics: Vec<Value> = instances
        .iter()
        .enumerate()
        .map(|(index, instance)| {
            json!({
                "id":index,"name":instance.name,"seed":instance.seed,"begin":instance.begin,
                "origin":instance.origin,"axis":instance.axis,"runnerSort":instance.sort,
            })
        })
        .collect();
    Ok(
        json!({"name":effect["name"],"seed":seed,"sourceSha256":effect["sha256"],
        "compiled":compiled,"dependencyCompiled":dependency_compiled,"particles":all_particles,
        "effectInstances":instance_diagnostics,"childRequests":child_requests,"frames":frames,"previewCamera":camera,
        "hostRandomFinalState":host_random_state,
        "metadataUnresolved":effect.get("materialInfo").is_none(),
        "limits":["bounded cloud/sprite/oriented/tail/line preview; backend mixed order verified separately",
            "one-generation runner/death/emission children and native directional child frames; no cloud gravity or trail companions",
            "identity/static emitters or native-normalized dense/sparse moving roots; bounded free-flight cloud/sprite/tail/line visuals and one-generation runner/death/emission children",
            if camera_axis == crate::fx::camera::DEFAULT_AXIS {
                "fixed -X camera axis, configurable origin"
            } else {
                "configurable camera axis and origin"
            }, "spawn culling disabled for preview",
            "no scene depth/fog/lighting","audio slots preserved as no-ops",
            "fixed zero editor wind; authored wind influence retained, nonzero preview wind rejected",
            "owned CRT-compatible child random stream","one millisecond scheduling"]}),
    )
}

// Filled below from the pinned native executable table verified in attempt #2.
// These are immutable random-table constants, not captured animation vertices.
const RANDOM_BITS: [u32; 512] = [
    0x3edc2faf, 0x3f15fb4f, 0x3e0f8545, 0x3eba4fb6, 0x3f60af04, 0x3f2d07bf, 0x3e3be3d6, 0x3e937454,
    0x3f2f02e0, 0x3d91f676, 0x3f732b6e, 0x3ee90349, 0x3f5f563a, 0x3f576da8, 0x3ec07691, 0x3f260a18,
    0x3f5ab690, 0x3c2010c2, 0x3efe528c, 0x3edd4503, 0x3eddc44c, 0x3e70cb5c, 0x3f0984e9, 0x3f1d9832,
    0x3cf7acf5, 0x3d6a4192, 0x3f2845cf, 0x3e94b895, 0x3f673cd3, 0x3e6fef52, 0x3d544e0b, 0x3f6ad7a9,
    0x3f6b5b26, 0x3ef42324, 0x3e5dba48, 0x3ee99289, 0x3f31e449, 0x3d351ea8, 0x3f21c0d2, 0x3f689520,
    0x3dbc9e2a, 0x3f712fa9, 0x3ee033b9, 0x3ec5a954, 0x3f008a12, 0x3ee60334, 0x3d9e8986, 0x3ce3e1c7,
    0x3f36cb82, 0x3eefbcdb, 0x3e295c30, 0x3f597a39, 0x3f166dfc, 0x3f412e8d, 0x3e013727, 0x3f571d02,
    0x3e4aac7d, 0x3f20cacf, 0x3f2681dd, 0x3f2ef8b8, 0x3f74ae42, 0x3f07da68, 0x3f354e58, 0x3e8dda1f,
    0x3f65c38e, 0x3e4c46d3, 0x3eeeea63, 0x3eeb10e3, 0x3f79ed1b, 0x3f7a0d23, 0x3f62447f, 0x3f72ab3c,
    0x3e97c7d1, 0x3f5c7f72, 0x3f0d034d, 0x3e13f976, 0x3ee74b7d, 0x3e7632ec, 0x3f4f6b9c, 0x3f541aa1,
    0x3f06697d, 0x3ebf6fc6, 0x3f3150fd, 0x3e0652bc, 0x3eb1c50d, 0x3ea15ebd, 0x3c082b07, 0x3ee657cb,
    0x3effc037, 0x3f667f13, 0x3f48863a, 0x3f509b0a, 0x3ee5944e, 0x3eef38d6, 0x3f706991, 0x3f645aa7,
    0x3e36ceba, 0x3ea1afc2, 0x3f549024, 0x3f7ed384, 0x3f7ee281, 0x3f558db3, 0x3f6fce5e, 0x3f17cccf,
    0x3f53b400, 0x3e8188dc, 0x3f286aaa, 0x3f19ebdd, 0x3f37a8fa, 0x3e2580b8, 0x3efba975, 0x3f3e3f4c,
    0x3f52f5dc, 0x3e8ee0d1, 0x3e138ffa, 0x3ed2c2a4, 0x3e372653, 0x3e772069, 0x3ea47f4b, 0x3dc92c59,
    0x3f63a342, 0x3f48f1d9, 0x3edf1d51, 0x3ded430a, 0x3f11100f, 0x3e9b95e9, 0x3f7b6fba, 0x3e93ffd7,
    0x3efd04c8, 0x3de197e1, 0x3f456501, 0x3f4e6d73, 0x3e6ed98d, 0x3f4cdd36, 0x3f3b5039, 0x3eae8e55,
    0x3f4f0212, 0x3eb8e81d, 0x3ee885d0, 0x3f229466, 0x3e962c43, 0x3f3e753d, 0x3f475231, 0x3f375ef6,
    0x3ed8d0e9, 0x3e8646b0, 0x3e68ed13, 0x3f0fea41, 0x3e6fe407, 0x3e5174c5, 0x3f47cf4a, 0x3f3cd7ff,
    0x3e889720, 0x3f6dfbe5, 0x3e006446, 0x3d127f29, 0x3f2bf60c, 0x3f745371, 0x3f3cd320, 0x3f1d1fdf,
    0x3f12104e, 0x3ed40a7f, 0x3e879744, 0x3f177f54, 0x3f18cfce, 0x3eda9035, 0x3ee580e8, 0x3e6dd900,
    0x3da4aa7f, 0x3f38d04c, 0x3f1ccd58, 0x3f1736af, 0x3f4a39c7, 0x3f0940bd, 0x3f524a6e, 0x3ba050d8,
    0x3e866b50, 0x3f39006a, 0x3ebd6f9e, 0x3f791e69, 0x3f227921, 0x3f5062d8, 0x3f217c66, 0x3f64c7cc,
    0x3f3569e7, 0x3d0b38c9, 0x3ef84e86, 0x3c45bcc3, 0x3f6df3bb, 0x3e21f4ee, 0x3dcc14c2, 0x3f57a979,
    0x3f790239, 0x3f2f165b, 0x3f759e6b, 0x3ee6e6c9, 0x3f522d27, 0x3e397b91, 0x3c1d8165, 0x3f6bc31e,
    0x3eadfa32, 0x3f651fc7, 0x3d8717e3, 0x3f682aff, 0x3eb60555, 0x3e83e5b6, 0x3de39a46, 0x3f126525,
    0x3e40f38b, 0x3ead52c3, 0x3d3fda61, 0x3f75b447, 0x3f4e3aaf, 0x3f6d5400, 0x3f186202, 0x3eeb0818,
    0x3edd482a, 0x3f070ea4, 0x3e56d908, 0x3f4e5370, 0x3f1307ba, 0x3f1322c0, 0x3da4bf9f, 0x3f79621d,
    0x3eafac39, 0x3f47e7b5, 0x3f0a34fe, 0x3e16a664, 0x3e575876, 0x3f30c33f, 0x3f26299c, 0x3b75ea27,
    0x3e8356c7, 0x3d890bcd, 0x3ed9d3c0, 0x3f19ffc8, 0x3f7cb391, 0x3edcf213, 0x3efa917f, 0x3d687a68,
    0x3e74c1ea, 0x3f125094, 0x3c815f61, 0x3f0bd139, 0x3e901467, 0x3d14ba14, 0x3e58cae0, 0x3f601da2,
    0x3e668bd2, 0x3e6f388f, 0x3e97be68, 0x3de14b77, 0x3f2cbc9e, 0x3f29e83e, 0x3e66d824, 0x3e2b27c7,
    0x3e6e7be0, 0x3ee178d8, 0x3ee206ea, 0x3f1a8107, 0x3e4c3e2f, 0x3eeecff6, 0x3f0d0d09, 0x3eeb9ba8,
    0x3c5fbfce, 0x3f3591c8, 0x3f395b6f, 0x3f54af51, 0x3f566bbe, 0x3e52bb04, 0x3f270048, 0x3e06cdc5,
    0x3f15bdce, 0x3eb52089, 0x3e94e859, 0x3e4098ef, 0x3f3c3bda, 0x3f52e6e5, 0x3f3d05a0, 0x3ea61cad,
    0x3f4c5a4d, 0x3f625853, 0x3e972b3d, 0x3f165f5e, 0x3f054536, 0x3ecf8207, 0x3d2cf628, 0x3f642dbf,
    0x3f3edb1c, 0x3e209616, 0x3ec49cf2, 0x3f6cc037, 0x3f605926, 0x3f49e6f8, 0x3f5c75e0, 0x3ec85d95,
    0x3f36d5de, 0x3e01a454, 0x3f039c39, 0x3f3a5bfb, 0x3eb54a6c, 0x3f02e536, 0x3ecc5763, 0x3e3a1ab6,
    0x3da4396f, 0x3f3247a4, 0x3f0bee6b, 0x3f06a63b, 0x3f0cbd4d, 0x3e9387cc, 0x3e0c0bb7, 0x3f51a01b,
    0x3f525a54, 0x3e49a1d5, 0x3e9e15c1, 0x3f5e4857, 0x3ee40e27, 0x3d353237, 0x3e7d7184, 0x3d5e68a4,
    0x3dffed36, 0x3df0eecc, 0x3e99b6c7, 0x3f602911, 0x3f16e9ee, 0x3f15445a, 0x3e2e071d, 0x3f307e3e,
    0x3e4b3586, 0x3e242137, 0x3f3b94e0, 0x3f3ea058, 0x3f066923, 0x3f5c3965, 0x3f2800de, 0x3f5e0e1e,
    0x3d13ff74, 0x3e50efdd, 0x3edaa535, 0x3f482529, 0x3f57f281, 0x3ecfd3ad, 0x3f600a00, 0x3f1d58b8,
    0x3ea57796, 0x3f375b91, 0x3e2c730d, 0x3e9430c1, 0x3eb4f102, 0x3d87cff6, 0x3e406b67, 0x3f6dbf71,
    0x3f12f31d, 0x3eaa86ca, 0x3e5dea29, 0x3d9b06a5, 0x3c33715c, 0x3efe7728, 0x3f7fbaa3, 0x3e833ae8,
    0x3ea9bbbe, 0x3e94c707, 0x3e0269d2, 0x3ebec4b7, 0x3ef8b44f, 0x3c6e2989, 0x3ea18495, 0x3f6f5757,
    0x3e689f86, 0x3e8b9708, 0x3e214511, 0x3e7b647e, 0x3e3c25c5, 0x3f7b8f8f, 0x3e9f3ccd, 0x3e9e31db,
    0x3e871fb4, 0x3da74dad, 0x3f5480c0, 0x3f05b749, 0x3f274a75, 0x3f7d866a, 0x3f60ac1d, 0x3ccbc8b6,
    0x3def1061, 0x3f575df9, 0x3f554b15, 0x3f29d196, 0x3e8d0112, 0x3e817c18, 0x3f63ecb7, 0x3eb3fc9e,
    0x3f4adbf4, 0x3d7f0bbc, 0x3da1b10a, 0x3efa2175, 0x3e39973a, 0x3eb68897, 0x3f5db15d, 0x3f209ad2,
    0x3f4323a7, 0x3e72434c, 0x3f0df339, 0x3e700b5a, 0x3d772cb8, 0x3e8a9836, 0x3f49d810, 0x3f273fac,
    0x3f64e879, 0x3f0823ab, 0x3e662edf, 0x3c759ea8, 0x3e8dee8f, 0x3f07e2c7, 0x3e50d7df, 0x3f4b47c4,
    0x3e6636a0, 0x3f6de664, 0x3f56dca2, 0x3f7ec5f6, 0x3eb3789f, 0x3e21ecd9, 0x3f0373d1, 0x3d1fb659,
    0x3f760803, 0x3e66d392, 0x3f02af6a, 0x3f6eb5a1, 0x3f4a39a3, 0x3f05b64a, 0x3eb35ece, 0x3ea5a409,
    0x3f638788, 0x3ee77e5a, 0x3f50f75d, 0x3f0debd0, 0x3d417ca5, 0x3f565860, 0x3dd298af, 0x3da418a8,
    0x3f078d10, 0x3f605990, 0x3e9ae7a7, 0x3f19346c, 0x3db97cf9, 0x3f353bb2, 0x3e9c1a3a, 0x3f5158ce,
    0x3d539fa5, 0x3f38d2a5, 0x3ec12892, 0x3f441bf1, 0x3eb4fc48, 0x3dc0390f, 0x3eb2c03b, 0x3e9e1343,
    0x3f7026e9, 0x3e884060, 0x3eda0f14, 0x3f33a104, 0x3e9c276d, 0x3f7e497e, 0x3ee91dd5, 0x3f6f3758,
    0x3d29da11, 0x3f3cd176, 0x3eb7223f, 0x3f5e815a, 0x3f313077, 0x3f7faa1f, 0x3f02a8ae, 0x3ec42415,
    0x3f14a200, 0x3d826b4f, 0x3d1e7c0a, 0x3f379a51, 0x3f7bfd64, 0x3f38b662, 0x3f74293b, 0x3edc2faf,
    0x3f15fb4f, 0x3e0f8545, 0x3eba4fb6, 0x3f60af04, 0x3f2d07bf, 0x3e3be3d6, 0x3e937454, 0x3f2f02e0,
    0x3d91f676, 0x3f732b6e, 0x3ee90349, 0x3f5f563a, 0x3f576da8, 0x3ec07691, 0x3f260a18, 0x3f5ab690,
    0x3c2010c2, 0x3efe528c, 0x3edd4503, 0x3eddc44c, 0x3e70cb5c, 0x3f0984e9, 0x3f1d9832, 0x3cf7acf5,
    0x3d6a4192, 0x3f2845cf, 0x3e94b895, 0x645f7866, 0x5f776172, 0x70616577, 0x745f6e6f, 0x00736761,
];
