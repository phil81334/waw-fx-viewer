//! Bounded automatic camera alternatives for otherwise invisible previews.
//!
//! The authored effect is never rotated or given different fade ranges. Move
//! the physical eye, regenerate view-dependent geometry, and verify the fit.

use serde_json::{json, Value};

use super::{camera, camera_contains, check_cancelled, loader, visit_camera_bounds_scored, Result};
use crate::fx::{camera_distance, camera_focus, camera_visibility};

// Reject degenerate or wholly faded/culled faces, without discarding thin
// tracers that can still touch real raster samples. Only the native renderer
// establishes whether a texture actually changes pixels.
const MIN_SCORE: f64 = 0.00000001;
// The view spans four NDC square units. Below this conservative weighted
// silhouette area, a model's complete trajectory may occupy less than one
// thumbnail pixel even though its original near-emitter view was visible.
const MODEL_PIXEL_SCORE: f64 = 4.0 / (192.0 * 192.0);

pub(super) fn apply(effect: &mut Value, settings: &Value) -> Result<()> {
    let axis = camera::parse_axis(settings.get("axis"))?;
    effect["previewCamera"] = settings["origin"].clone();
    effect["previewCameraAxis"] = json!(axis);
    Ok(())
}

/// Ordinary billboards and light-only scenes retain their established view.
/// Only authored directional geometry or distance ranges need alternatives.
pub(super) fn needs_alternatives(effect: &Value) -> bool {
    std::iter::once(effect)
        .chain(
            effect["dependencies"]
                .as_object()
                .into_iter()
                .flat_map(|items| items.values()),
        )
        .any(|definition| {
            definition["elements"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|element| {
                    if loader::words(element, "editorFlags")
                        .iter()
                        .any(|flag| flag == "disabled")
                    {
                        return false;
                    }
                    ["orientedSprite", "tail", "line", "model", "trail"]
                        .iter()
                        .any(|kind| element.get(*kind).is_some())
                        || ["fadeInRange", "fadeOutRange", "spawnRange"]
                            .iter()
                            .any(|name| {
                                loader::words(element, name)
                                    .get(1)
                                    .and_then(|word| word.parse::<f32>().ok())
                                    .is_some_and(|width| width != 0.0)
                            })
                })
        })
}

pub(super) fn playback_score(effect: &Value, playback: &Value, settings: &Value) -> Result<f64> {
    if !needs_alternatives(effect) {
        return Ok(1.0);
    }
    let mut best = 0.0_f64;
    for frame in playback["frames"]
        .as_array()
        .ok_or("Missing preview frames")?
    {
        best = best.max(camera_visibility::score(
            frame,
            settings,
            &effect["sharedMaterialInfo"],
        )?);
    }
    Ok(best)
}

pub(super) fn choose(
    effect: &mut Value,
    seed: u32,
    times: &[u32],
    original: &Value,
    original_score: f64,
    cancelled: &dyn Fn() -> bool,
) -> Result<Value> {
    if !needs_alternatives(effect) {
        return Ok(original.clone());
    }
    if original_score < MODEL_PIXEL_SCORE
        && original["bounds"].is_array()
        && model_only(effect)
    {
        if let Some(detail) = model_detail(effect, seed, times, original, original_score, cancelled)? {
            return Ok(detail);
        }
    }
    if original_score >= MIN_SCORE {
        return improve_detail(effect, seed, times, original, original_score, cancelled);
    }
    // An out-of-range full-volume camera can suppress every subsequent birth.
    // An empty result there is not proof that the original effect is empty.
    if original["emptyGeometry"] == true {
        return focused_fallback(effect, seed, times, original, cancelled);
    }
    let minimum = crate::fx::array(&original["bounds"][0], "camera minimum")?;
    let maximum = crate::fx::array(&original["bounds"][1], "camera maximum")?;
    let count = original["vertexSamples"].as_u64().unwrap_or(1) as usize;
    let distances = camera_distance::preferred_distances(effect)?;

    // Prefer a better physical distance before changing the familiar view.
    // Only invisible previews enter this bounded search; working ones return
    // above without another simulation pass.
    for candidate in camera_distance::candidates(effect, original)? {
        check_cancelled(cancelled)?;
        if let Some((settings, score)) = try_candidate(effect, seed, times, candidate, cancelled)? {
            return improve_detail(effect, seed, times, &settings, score, cancelled);
        }
    }
    let original_axis = camera::parse_axis(original.get("axis"))?;
    for axis in camera::CARDINAL_AXES {
        if axis == original_axis {
            continue;
        }
        check_cancelled(cancelled)?;
        let fitted = camera::fit_bounds_for_axis(minimum, maximum, count, axis)?;
        if let Some((settings, score)) =
            try_candidate(effect, seed, times, fitted.clone(), cancelled)?
        {
            return improve_detail(effect, seed, times, &settings, score, cancelled);
        }
        // Combined direction/fade cases get two highest-ranked distances per
        // direction, rather than an unbounded search through camera positions.
        for distance in distances.iter().take(2) {
            let Some(candidate) = camera_distance::at_distance(&fitted, *distance)? else {
                continue;
            };
            if let Some((settings, score)) =
                try_candidate(effect, seed, times, candidate, cancelled)?
            {
                return improve_detail(effect, seed, times, &settings, score, cancelled);
            }
        }
    }
    focused_fallback(effect, seed, times, original, cancelled)
}

/// A model-only falling object can travel thousands of units while remaining
/// physically tiny. Keep sprites, tracers, lights and mixed graphs on their
/// own proven camera paths; this fallback only restores a nearby model view.
fn model_only(effect: &Value) -> bool {
    let mut found_model = false;
    for definition in std::iter::once(effect).chain(
        effect["dependencies"].as_object().into_iter().flat_map(|items| items.values()),
    ) {
        for element in definition["elements"].as_array().into_iter().flatten() {
            if loader::words(element, "editorFlags").iter().any(|flag| flag == "disabled") {
                continue;
            }
            if ["billboardSprite", "orientedSprite", "tail", "line", "cloud", "trail", "light", "spotLight", "decal"]
                .iter().any(|kind| element.get(*kind).is_some())
            {
                return false;
            }
            found_model |= element.get("model").is_some();
        }
    }
    found_model
}

fn model_detail(
    effect: &mut Value,
    seed: u32,
    times: &[u32],
    original: &Value,
    original_score: f64,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<Value>> {
    let target = if let Some(origin) = effect.get("previewEmitterFrame").and_then(|frame| frame.get("origin")) {
        crate::fx::array(origin, "emitter origin")?
    } else {
        [0.0; 3]
    };
    // Match the established physical near-emitter eye, rather than enlarging
    // a model or shortening its fall. Its later trajectory may leave view,
    // which is why the existing UI labels this as a partial close-up.
    let detail = camera_focus::at_target(original, target, 200.0, 60.0)?;
    let score = match sampled_score(effect, seed, times, &detail, cancelled) {
        Ok(score) => score,
        Err(error) if error.starts_with("transient:") => return Err(error),
        Err(_) => {
            apply(effect, original)?;
            return Ok(None);
        }
    };
    if score.value >= MIN_SCORE && score.value >= original_score * 4.0 {
        return Ok(Some(detail));
    }
    apply(effect, original)?;
    Ok(None)
}

struct ViewScore {
    value: f64,
    positive_samples: u64,
    cloud_area: f64,
}

fn sampled_score(
    effect: &mut Value,
    seed: u32,
    times: &[u32],
    settings: &Value,
    cancelled: &dyn Fn() -> bool,
) -> Result<ViewScore> {
    apply(effect, settings)?;
    let (bounds, _, score) =
        visit_camera_bounds_scored(effect, seed, times, cancelled, Some(settings))?;
    Ok(ViewScore {
        value: score,
        positive_samples: bounds["cameraPositiveSamples"].as_u64().unwrap_or(0),
        cloud_area: bounds["cameraCloudArea"].as_f64().unwrap_or(0.0),
    })
}

/// Very wide fade-aware fits can reduce an otherwise useful effect to a few
/// pixels. First narrow only the lens, keeping the physical eye and all
/// camera-dependent simulation inputs unchanged. This is explicitly a detail
/// view: parts of the larger effect may fall outside the viewport.
fn improve_detail(
    effect: &mut Value,
    seed: u32,
    times: &[u32],
    original: &Value,
    original_score: f64,
    cancelled: &dyn Fn() -> bool,
) -> Result<Value> {
    if original["fovDegrees"].as_f64().unwrap_or(60.0) <= 100.0 {
        apply(effect, original)?;
        return Ok(original.clone());
    }
    let mut detail = original.clone();
    detail["fovDegrees"] = json!(60.0);
    detail["focusedView"] = json!(true);
    detail["partialScene"] = json!(true);
    detail["matrixRows"] = camera::matrix(&detail);
    let score = match sampled_score(effect, seed, times, &detail, cancelled) {
        Ok(score) => score,
        Err(error) if error.starts_with("transient:") => return Err(error),
        Err(_) => ViewScore {
            value: 0.0,
            positive_samples: 0,
            cloud_area: 0.0,
        },
    };
    let cloud = std::iter::once(&*effect)
        .chain(
            effect["dependencies"]
                .as_object()
                .into_iter()
                .flat_map(|items| items.values()),
        )
        .any(|definition| {
            definition["elements"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|element| element.get("cloud").is_some())
        });
    // Cloud ranking is conservative rather than a synthetic particle-area
    // estimate. Require its same positive score; real pixel/coverage checks
    // remain the evidence that the narrower detail view is useful.
    if score.value >= MIN_SCORE
        && (score.value >= original_score * 1.25
            || (cloud && original_score <= 1.01 && score.value >= original_score * 0.99))
    {
        return refine_cloud_detail(effect, seed, times, detail, &score, cancelled);
    }
    // A useful group may be off-centre in a very large full-effect fit.
    focused_fallback(effect, seed, times, original, cancelled)
}

/// Both a narrowed whole-volume fit and a nearby physical camera can produce
/// a detail view. Refine them through the same bounded policy, keeping large
/// surrounding volumes at 60 degrees and preserving sampled activity.
fn refine_cloud_detail(
    effect: &mut Value,
    seed: u32,
    times: &[u32],
    detail: Value,
    score: &ViewScore,
    cancelled: &dyn Fn() -> bool,
) -> Result<Value> {
    if score.cloud_area > 0.0 && score.cloud_area < 3.0 {
        let mut tighter = detail.clone();
        tighter["fovDegrees"] = json!(30.0);
        tighter["matrixRows"] = camera::matrix(&tighter);
        match sampled_score(effect, seed, times, &tighter, cancelled) {
            Ok(tighter_score)
                if tighter_score.value >= score.value
                    && tighter_score.positive_samples >= score.positive_samples
                    && tighter_score.cloud_area >= score.cloud_area =>
            {
                return Ok(tighter)
            }
            Err(error) if error.starts_with("transient:") => return Err(error),
            _ => apply(effect, &detail)?,
        }
    }
    Ok(detail)
}

fn focused_fallback(
    effect: &mut Value,
    seed: u32,
    times: &[u32],
    original: &Value,
    cancelled: &dyn Fn() -> bool,
) -> Result<Value> {
    for settings in camera_focus::candidates(effect, original, &[])? {
        check_cancelled(cancelled)?;
        let score = match sampled_score(effect, seed, times, &settings, cancelled) {
            Ok(score) => score,
            Err(error) if error.starts_with("transient:") => return Err(error),
            // Keep every original simulation/resource guard. One unavailable
            // camera route does not justify changing the effect to render it.
            Err(_) => continue,
        };
        if score.value >= MIN_SCORE {
            return refine_cloud_detail(effect, seed, times, settings, &score, cancelled);
        }
    }
    apply(effect, original)?;
    Ok(original.clone())
}

fn try_candidate(
    effect: &mut Value,
    seed: u32,
    times: &[u32],
    mut settings: Value,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<(Value, f64)>> {
    let axis = camera::parse_axis(settings.get("axis"))?;
    let distance = settings["fadeCameraDistance"]
        .as_f64()
        .map(|distance| distance as f32);
    for _ in 0..4 {
        check_cancelled(cancelled)?;
        apply(effect, &settings)?;
        let sampled = visit_camera_bounds_scored(effect, seed, times, cancelled, Some(&settings));
        let (bounds, _, score) = match sampled {
            Ok(value) => value,
            Err(error) if error.starts_with("transient:") => return Err(error),
            // A view-dependent child route can hit a genuine guard. It is not
            // permission to weaken that guard or reject the original view.
            Err(_) => return Ok(None),
        };
        if bounds["emptyGeometry"] == true {
            return Ok(None);
        }
        if camera_contains(&settings, &bounds["bounds"]) {
            if score >= MIN_SCORE {
                settings["automaticView"] = json!(true);
                return Ok(Some((settings, score)));
            }
            return Ok(None);
        }
        let minimum = crate::fx::array(&bounds["bounds"][0], "camera minimum")?;
        let maximum = crate::fx::array(&bounds["bounds"][1], "camera maximum")?;
        let count = bounds["vertexSamples"].as_u64().unwrap_or(1) as usize;
        let fitted = camera::fit_bounds_for_axis(minimum, maximum, count, axis)?;
        settings = if let Some(distance) = distance {
            let Some(adjusted) = camera_distance::at_distance(&fitted, distance)? else {
                return Ok(None);
            };
            adjusted
        } else {
            fitted
        };
    }
    Ok(None)
}
