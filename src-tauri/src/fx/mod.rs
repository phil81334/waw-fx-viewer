//! Deterministic visual playback of supported WaW Modtools effects.
//!
//! Asset resolution and native rendering are separate from this bounded
//! compiler/scheduler. Unsupported behaviour rejects the complete effect.

pub(crate) mod camera;
pub(crate) mod camera_distance;
pub(crate) mod camera_focus;
pub(crate) mod camera_visibility;
pub(crate) mod cloud_geometry;
pub(crate) mod cloud_motion;
pub(crate) mod cloud_state;
pub(crate) mod frames;
pub(crate) mod geometry;
pub(crate) mod loader;
pub(crate) mod model_pose;
pub(crate) mod model_uv;
pub(crate) mod moving_frame_math;
pub(crate) mod moving_frames;
pub(crate) mod native_velocity;
pub(crate) mod offset_math;
pub(crate) mod prepare;
pub(crate) mod preview_colour;
pub(crate) mod renderer;
pub(crate) mod rotation_math;
pub(crate) mod simulation;
pub(crate) mod sparse_frame_math;
pub(crate) mod trail_state;
pub(crate) mod trails;

// Separate asset modules are owned by the read-only Modtools asset layer.
pub(crate) mod material_states;
pub(crate) mod model_assets;
pub(crate) mod model_material;
pub(crate) mod model_order;
pub(crate) mod model_reader;
pub(crate) mod techset;
pub(crate) mod wavelet;
pub(crate) mod wavelet_codes;

use serde_json::Value;

/// Conservative retained JSON allocation accounting. Object entries include a
/// generous tree-node allowance, so this is a bound rather than file size.
pub(crate) fn memory_cost(value: &Value) -> usize {
    let base = std::mem::size_of::<Value>();
    match value {
        Value::String(text) => base.saturating_add(text.capacity()),
        Value::Array(values) => values.iter().fold(
            base.saturating_add(values.capacity().saturating_mul(base)),
            |total, child| total.saturating_add(memory_cost(child).saturating_sub(base)),
        ),
        Value::Object(values) => values.iter().fold(base, |total, (key, child)| {
            total
                .saturating_add(256)
                .saturating_add(key.capacity())
                .saturating_add(memory_cost(child))
        }),
        _ => base,
    }
}

/// Diagnostic state never participates in rendering or the update clock.
pub(crate) fn compact_frame(frame: &mut Value) {
    if let Some(fields) = frame.as_object_mut() {
        for name in [
            "particles",
            "effectOrder",
            "trailParticles",
            "trailMeshes",
            "drawPasses",
            "activeTrailPoints",
            "lightPolicy",
            "spotState",
            "previewPackedLighting",
        ] {
            fields.remove(name);
        }
    }
}

/// Prepare a finite set of frame samples on the verified shared update clock.
/// The caller does this off the UI thread and keeps the resulting clip bounded.
pub(crate) fn simulate(effect: &Value, seed: u32, times: &[u32]) -> Result<Value, String> {
    simulation::validate_cloud_scope(effect)?;

    if let Some(wind) = effect.get("previewWind") {
        let values = wind
            .as_array()
            .ok_or("previewWind must contain three zero numbers")?;
        if values.len() != 3 || values.iter().any(|value| value.as_f64() != Some(0.0)) {
            return Err("Only the verified fixed-zero-wind preview is supported".into());
        }
    }

    let has_trail = effect["elements"]
        .as_array()
        .ok_or("Missing elements")?
        .iter()
        .any(|element| {
            element.get("trail").is_some()
                && !loader::words(element, "editorFlags")
                    .iter()
                    .any(|flag| flag == "disabled")
        });

    let mut playback = if has_trail {
        let source_frames = if let Some(raw) = effect.get("previewSourceFrames") {
            let mut frames = Vec::new();
            for frame in raw.as_array().ok_or("Invalid controlled source frames")? {
                frames.push(trail_state::SourceFrame {
                    time: u32::try_from(frame["time"].as_u64().ok_or("Missing source time")?)
                        .map_err(|error| error.to_string())?,
                    origin: array(&frame["origin"], "source origin")?,
                });
            }
            Some(frames)
        } else {
            None
        };

        // Root trails and ordinary particles must share one allocation clock.
        simulation::simulate_shared_trails(effect, seed, times, source_frames.as_deref())
    } else {
        if effect.get("previewSourceFrames").is_some() {
            return Err("Controlled moving-source playback is currently trail-only".into());
        }
        simulation::simulate(effect, seed, times)
    }?;

    if let Some(information) = effect.get("modelInfo") {
        playback["modelInfo"] = information.clone();
    }

    Ok(playback)
}

pub(crate) fn array<const N: usize>(value: &Value, label: &str) -> Result<[f32; N], String> {
    let values = value.as_array().ok_or_else(|| format!("Missing {label}"))?;
    if values.len() != N {
        return Err(format!("{label} must contain {N} numbers"));
    }

    let mut output = [0.0; N];
    for (index, number) in values.iter().enumerate() {
        output[index] = number.as_f64().ok_or("Value is not numeric")? as f32;
        if !output[index].is_finite() {
            return Err(format!("{label} contains a non-finite number"));
        }
    }
    Ok(output)
}
