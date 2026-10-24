//! Physical detail views for effects too large to frame as a whole.
//!
//! Ambient clouds are often intended to surround the camera. Requiring their
//! entire volume or long trajectory to fit on screen can move the eye beyond
//! their authored spawn/fade ranges. These explicitly labelled detail views
//! show part of the unchanged effect through an ordinary perspective camera.

use serde_json::{json, Value};

use super::{camera, camera_distance};

type Result<T> = std::result::Result<T, String>;

const MAX_CANDIDATES: usize = 16;
const MAX_ANCHORS: usize = 4;

fn point(value: &Value) -> Option<[f32; 3]> {
    let values = value.as_array().filter(|values| values.len() == 3)?;
    let coordinates = [
        values[0].as_f64()? as f32,
        values[1].as_f64()? as f32,
        values[2].as_f64()? as f32,
    ];
    coordinates
        .into_iter()
        .all(f32::is_finite)
        .then_some(coordinates)
}

fn dot(left: [f32; 3], right: [f32; 3]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

/// Construct an ordinary camera aimed at a genuine effect/sample position.
/// Parts of the larger effect may lie outside its frustum; no vertices, sizes,
/// lifetimes, emitter positions or fade values are replaced to make them fit.
pub(crate) fn at_target(
    fitted: &Value,
    target: [f32; 3],
    distance: f32,
    field_of_view: f32,
) -> Result<Value> {
    if !target.into_iter().all(f32::is_finite)
        || !distance.is_finite()
        || distance <= 2.0
        || !field_of_view.is_finite()
        || !(20.0..=90.0).contains(&field_of_view)
    {
        return Err("Invalid focused preview camera".into());
    }
    let axis = camera::parse_axis(fitted.get("axis"))?;
    let origin = std::array::from_fn::<_, 3, _>(|coordinate| {
        target[coordinate] - axis[0][coordinate] * distance
    });
    if !origin.into_iter().all(f32::is_finite) {
        return Err("Focused preview camera exceeds the finite range".into());
    }
    let mut far = 8192.0_f32.max(distance + 128.0);
    if let (Some(minimum), Some(maximum)) =
        (point(&fitted["bounds"][0]), point(&fitted["bounds"][1]))
    {
        for corner in 0..8 {
            let position = std::array::from_fn::<_, 3, _>(|coordinate| {
                if (corner >> coordinate) & 1 == 0 {
                    minimum[coordinate]
                } else {
                    maximum[coordinate]
                }
            });
            let delta = std::array::from_fn(|coordinate| position[coordinate] - origin[coordinate]);
            far = far.max(dot(delta, axis[0]) + 128.0);
        }
    }
    if !far.is_finite() {
        return Err("Focused preview depth range is not finite".into());
    }
    let mut settings = fitted.clone();
    if let Some(object) = settings.as_object_mut() {
        object.remove("emptyGeometry");
        object.remove("fadeCameraDistance");
    }
    settings["origin"] = json!(origin);
    settings["axis"] = json!(axis);
    settings["near"] = json!(1.0);
    settings["far"] = json!(far);
    settings["fovDegrees"] = json!(field_of_view);
    settings["focusedView"] = json!(true);
    settings["partialScene"] = json!(true);
    settings["focusTarget"] = json!(target);
    settings["focusDistance"] = json!(distance);
    settings["matrixRows"] = camera::matrix(&settings);
    Ok(settings)
}

/// Bounded alternatives around real sample anchors, then the emitter and
/// complete-bounds centre. Callers must resimulate under each physical eye and
/// verify visible output; candidate generation itself does not prove support.
pub(crate) fn candidates(
    source: &Value,
    fitted: &Value,
    anchors: &[[f32; 3]],
) -> Result<Vec<Value>> {
    let mut targets = Vec::new();
    let mut include = |target: [f32; 3]| {
        if target.into_iter().all(f32::is_finite)
            && !targets.iter().any(|previous: &[f32; 3]| {
                previous
                    .iter()
                    .zip(target)
                    .all(|(left, right)| (*left - right).abs() < 0.01)
            })
            && targets.len() < MAX_ANCHORS
        {
            targets.push(target);
        }
    };
    for &target in anchors {
        include(target);
    }
    include(point(&source["previewEmitterFrame"]["origin"]).unwrap_or([0.0; 3]));
    if let (Some(minimum), Some(maximum)) =
        (point(&fitted["bounds"][0]), point(&fitted["bounds"][1]))
    {
        include(std::array::from_fn(|coordinate| {
            minimum[coordinate] * 0.5 + maximum[coordinate] * 0.5
        }));
    }

    let mut distances = camera_distance::preferred_distances(source)?;
    distances.truncate(2);
    // A nearby ordinary camera also handles surrounding clouds and authored
    // ranges with no preferred positive-distance interval.
    distances.extend([128.0, 256.0]);
    distances.dedup_by(|left, right| (*left - *right).abs() < 0.01);
    let mut output = Vec::new();
    for target in targets {
        for &distance in &distances {
            if output.len() == MAX_CANDIDATES {
                return Ok(output);
            }
            if distance > 2.0 {
                output.push(at_target(fitted, target, distance, 60.0)?);
            }
        }
    }
    Ok(output)
}
