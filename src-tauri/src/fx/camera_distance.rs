//! Physical camera alternatives for authored particle distance fades.
//!
//! A large effect can fit the ordinary 60-degree camera while every particle
//! fades out before that distance. These candidates move only the eye and, when
//! necessary, widen the perspective. They never rewrite EFX ranges or alpha.

use serde_json::{json, Value};

use super::loader;

type Result<T> = std::result::Result<T, String>;

const MAX_CANDIDATES: usize = 8;
const MAX_FIELD_OF_VIEW: f32 = 150.0;
const FIT_EXTENT: f32 = 0.8;

#[derive(Clone, Copy)]
struct FadeWindow {
    incoming: [f32; 2],
    outgoing: [f32; 2],
}

impl FadeWindow {
    fn amount(self, distance: f32) -> f32 {
        let falloff = |range: [f32; 2]| {
            if distance < range[0] {
                1.0
            } else if distance >= range[0] + range[1] {
                0.0
            } else {
                1.0 - (distance - range[0]) / range[1]
            }
        };
        let incoming = if self.incoming[1] == 0.0 {
            1.0
        } else {
            falloff(self.incoming)
        };
        let outgoing = if self.outgoing[1] == 0.0 {
            1.0
        } else {
            1.0 - falloff(self.outgoing)
        };
        incoming.min(outgoing).clamp(0.0, 1.0)
    }

    fn targets(self) -> Vec<f32> {
        let lower = if self.outgoing[1] == 0.0 {
            0.0
        } else {
            self.outgoing[0].max(0.0)
        };
        let plateau_start = if self.outgoing[1] == 0.0 {
            0.0
        } else {
            (self.outgoing[0] + self.outgoing[1]).max(0.0)
        };
        if self.incoming[1] == 0.0 {
            // Only a near fade is present. Cross its complete transition, then
            // leave a modest margin instead of inventing an infinite distance.
            return vec![plateau_start + 128.0, plateau_start * 1.25 + 16.0];
        }

        let upper = self.incoming[0] + self.incoming[1];
        if upper <= lower || upper <= 1.0 {
            return Vec::new();
        }
        let plateau_end = self.incoming[0];
        let mut targets = Vec::new();
        if plateau_end > plateau_start {
            targets.push((plateau_start + plateau_end) * 0.5);
            targets.push(plateau_start * 0.2 + plateau_end * 0.8);
        }
        // Overlapping near/far transitions can have no full-opacity plateau.
        // Samples inside that authored interval remain valid camera choices.
        targets.push(lower * 0.5 + upper * 0.5);
        targets.push(lower * 0.25 + upper * 0.75);
        targets
    }
}

fn range(element: &Value, name: &str) -> Result<[f32; 2]> {
    let words = loader::words(element, name);
    if words.is_empty() {
        return Ok([0.0, 0.0]);
    }
    if words.len() != 2 {
        return Err(format!("{name} must contain two finite numbers"));
    }
    let mut pair = [0.0; 2];
    for index in 0..2 {
        pair[index] = words[index]
            .parse::<f32>()
            .map_err(|_| format!("Invalid {name}"))?;
        if !pair[index].is_finite() {
            return Err(format!("Invalid {name}"));
        }
    }
    // This is the same range canonicalisation as the scheduler's compiler.
    if pair[1] < 0.0 {
        pair[0] += pair[1];
        pair[1] = -pair[1];
    }
    if !pair[0].is_finite() || !(pair[0] + pair[1]).is_finite() {
        return Err(format!("{name} exceeds the finite camera range"));
    }
    Ok(pair)
}

fn windows(source: &Value) -> Result<Vec<FadeWindow>> {
    let mut output = Vec::new();
    let definitions = std::iter::once(source).chain(
        source["dependencies"]
            .as_object()
            .into_iter()
            .flat_map(|definitions| definitions.values()),
    );
    for definition in definitions {
        for element in definition["elements"].as_array().into_iter().flatten() {
            if loader::words(element, "editorFlags")
                .iter()
                .any(|flag| flag == "disabled")
            {
                continue;
            }
            if element.get("sound").is_some() {
                continue;
            }
            let window = FadeWindow {
                incoming: range(element, "fadeInRange")?,
                outgoing: range(element, "fadeOutRange")?,
            };
            if window.incoming[1] != 0.0 || window.outgoing[1] != 0.0 {
                output.push(window);
            }
        }
    }
    Ok(output)
}

/// Bounded, deterministic distances suggested by the active authored ranges.
/// These are relative to the fitted geometry's centre, so a caller still needs
/// to resimulate and inspect the real per-particle result before accepting one.
pub(crate) fn preferred_distances(source: &Value) -> Result<Vec<f32>> {
    let windows = windows(source)?;
    let mut distances = windows
        .iter()
        .flat_map(|window| window.targets())
        .filter(|distance| distance.is_finite() && *distance > 1.0)
        .collect::<Vec<_>>();
    distances.sort_by(f32::total_cmp);
    distances.dedup_by(|left, right| (*left - *right).abs() < 0.01);
    distances.sort_by(|left, right| {
        let score = |distance| {
            windows
                .iter()
                .map(|window| window.amount(distance))
                .sum::<f32>()
        };
        score(*right)
            .total_cmp(&score(*left))
            .then_with(|| left.total_cmp(right))
    });
    distances.truncate(MAX_CANDIDATES);
    Ok(distances)
}

fn point(value: &Value, name: &str) -> Result<[f32; 3]> {
    let coordinates = value
        .as_array()
        .filter(|coordinates| coordinates.len() == 3)
        .ok_or_else(|| format!("Missing {name}"))?;
    let mut output = [0.0; 3];
    for axis in 0..3 {
        output[axis] = coordinates[axis]
            .as_f64()
            .ok_or_else(|| format!("Invalid {name}"))? as f32;
        if !output[axis].is_finite() {
            return Err(format!("Invalid {name}"));
        }
    }
    Ok(output)
}

fn dot(left: [f32; 3], right: [f32; 3]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

/// Position the physical eye at the requested distance while keeping every
/// corner of the current fitted bounds in front of the near plane and inside
/// the perspective frustum. Impossible complete fits are rejected explicitly.
pub(crate) fn at_distance(fitted: &Value, distance: f32) -> Result<Option<Value>> {
    if !distance.is_finite() || distance <= 1.0 {
        return Ok(None);
    }
    if fitted["emptyGeometry"] == true {
        return Ok(None);
    }
    let minimum = point(&fitted["bounds"][0], "camera minimum")?;
    let maximum = point(&fitted["bounds"][1], "camera maximum")?;
    if minimum.iter().zip(maximum).any(|(low, high)| *low > high) {
        return Err("Camera bounds must be ordered".into());
    }
    let default_axes = json!([[-1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0]]);
    let axes = fitted.get("axis").unwrap_or(&default_axes);
    let forward = point(&axes[0], "camera forward axis")?;
    let left = point(&axes[1], "camera left axis")?;
    let up = point(&axes[2], "camera up axis")?;
    if [forward, left, up]
        .iter()
        .any(|axis| (dot(*axis, *axis) - 1.0).abs() > 0.0001)
        || dot(forward, left).abs() > 0.0001
        || dot(forward, up).abs() > 0.0001
        || dot(left, up).abs() > 0.0001
    {
        return Err("Camera axes must be orthonormal".into());
    }
    let center = std::array::from_fn::<_, 3, _>(|axis| minimum[axis] * 0.5 + maximum[axis] * 0.5);
    let origin = std::array::from_fn::<_, 3, _>(|axis| center[axis] - forward[axis] * distance);
    let near = fitted["near"].as_f64().unwrap_or(1.0) as f32;
    if !near.is_finite() || near <= 0.0 || !origin.into_iter().all(f32::is_finite) {
        return Err("Invalid physical camera range".into());
    }
    let mut ratio = 0.0_f32;
    let mut maximum_depth = 0.0_f32;
    for corner in 0..8 {
        let position = std::array::from_fn::<_, 3, _>(|axis| {
            if (corner >> axis) & 1 == 0 {
                minimum[axis]
            } else {
                maximum[axis]
            }
        });
        let delta = std::array::from_fn(|axis| position[axis] - origin[axis]);
        let depth = dot(delta, forward);
        if depth <= near {
            return Ok(None);
        }
        maximum_depth = maximum_depth.max(depth);
        ratio = ratio.max(dot(delta, left).abs() / depth);
        ratio = ratio.max(dot(delta, up).abs() / depth);
    }
    let needed_fov = (ratio / FIT_EXTENT).atan().to_degrees() * 2.0;
    let field_of_view = needed_fov
        .max(fitted["fovDegrees"].as_f64().unwrap_or(60.0) as f32)
        .max(1.0);
    if !field_of_view.is_finite() || field_of_view > MAX_FIELD_OF_VIEW {
        return Ok(None);
    }
    let far = (maximum_depth + 128.0).max(8192.0);
    if !far.is_finite() || far <= near {
        return Ok(None);
    }
    let mut settings = fitted.clone();
    settings["origin"] = json!(origin);
    settings["axis"] = axes.clone();
    settings["fovDegrees"] = json!(field_of_view);
    settings["near"] = json!(near);
    settings["far"] = json!(far);
    settings["fitNdcExtent"] = json!(FIT_EXTENT);
    settings["fadeCameraDistance"] = json!(distance);
    settings["matrixRows"] = super::camera::matrix(&settings);
    Ok(Some(settings))
}

/// Only useful after the ordinary preview has no visible output. Callers keep
/// that fast default for unaffected effects and validate any alternative by
/// resampling under its actual camera origin and axes.
pub(crate) fn candidates(source: &Value, fitted: &Value) -> Result<Vec<Value>> {
    let mut output = Vec::new();
    for distance in preferred_distances(source)? {
        if let Some(settings) = at_distance(fitted, distance)? {
            output.push(settings);
        }
    }
    Ok(output)
}
