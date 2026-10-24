//! Conservative geometric visibility for choosing a preview camera.
//!
//! A projected face can cover a large area yet be entirely back-facing. Honour
//! the material's native culling before ranking camera candidates. This is a
//! camera heuristic, not a claim that an atlas tile contains visible texels.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;

type Result<T> = std::result::Result<T, String>;

/// Sum front-facing NDC area, weighted by packed opacity or additive colour.
/// Input is a projected frame from FrameProjection; model placements remain
/// explicit and are applied here before the camera matrix. The ordinary view
/// spans [-1, 1] on both screen axes, so a full-screen opaque quad scores four.
/// Clouds and lights use a conservative positive score instead of interpreting
/// their shader-expanded geometry as sprite vertices.
pub(crate) fn score(frame: &Value, settings: &Value, materials: &Value) -> Result<f64> {
    let matrix = frame
        .get("matrixRows")
        .cloned()
        .unwrap_or_else(|| super::camera::matrix(settings));
    let rows = [
        super::array::<4>(&matrix[0], "camera row")?,
        super::array::<4>(&matrix[1], "camera row")?,
        super::array::<4>(&matrix[2], "camera row")?,
        super::array::<4>(&matrix[3], "camera row")?,
    ];
    let mut total = 0.0_f64;
    let draws = frame["draws"].as_array().ok_or("Missing projected draws")?;
    for draw in draws {
        let name = draw["texture"].as_str().ok_or("Missing draw material")?;
        let plain_name = name.strip_prefix("__model__/").unwrap_or(name);
        let material = materials.get(name).or_else(|| materials.get(plain_name));
        if material.is_some_and(proven_transparent) {
            continue;
        }
        let additive = material.is_some_and(|material| {
            material["sourceBlend"].as_u64() == Some(2)
                && material["destinationBlend"].as_u64() == Some(2)
        });
        if let Some(cloud) = draw.get("cloud") {
            let channel = cloud["bgra"]
                .as_array()
                .map(|colour| {
                    if additive {
                        colour
                            .iter()
                            .take(3)
                            .filter_map(Value::as_u64)
                            .max()
                            .unwrap_or(0)
                    } else {
                        colour.get(3).and_then(Value::as_u64).unwrap_or(0)
                    }
                })
                .unwrap_or(0);
            let alpha = cloud["bgra"][3].as_u64().unwrap_or(0);
            if channel > 0 && alpha > 0 {
                total += 1.0;
            }
            continue;
        }
        let cull = material
            .and_then(|material| material["cullModeD3D9"].as_u64())
            .unwrap_or(1);
        if !(1..=3).contains(&cull) {
            return Err("Invalid preview culling mode".into());
        }
        let bytes = STANDARD
            .decode(
                draw["vertices"]
                    .as_str()
                    .ok_or("Missing projected vertices")?,
            )
            .map_err(|error| error.to_string())?;
        if bytes.is_empty() || bytes.len() % 32 != 0 {
            return Err("Camera scoring requires complete packed vertices".into());
        }
        let placement = if draw["vertexFormat"] == "model32" {
            Some(
                serde_json::from_value::<super::model_pose::Placement>(
                    draw["modelPlacement"].clone(),
                )
                .map_err(|error| error.to_string())?,
            )
        } else {
            None
        };
        let mut vertices = Vec::with_capacity(bytes.len() / 32);
        for vertex in bytes.chunks_exact(32) {
            let mut point: [f32; 3] = std::array::from_fn(|axis| {
                f32::from_le_bytes(vertex[axis * 4..axis * 4 + 4].try_into().unwrap())
            });
            if let Some(placement) = &placement {
                point = super::model_pose::world_point(placement, point);
            }
            if !point.into_iter().all(f32::is_finite) {
                return Err("Cannot score non-finite preview geometry".into());
            }
            let clip: [f64; 4] = std::array::from_fn(|row| {
                f64::from(rows[row][3])
                    + (0..3)
                        .map(|axis| f64::from(rows[row][axis]) * f64::from(point[axis]))
                        .sum::<f64>()
            });
            let mut strength = if additive {
                // The effect shaders apply packed alpha before ONE/ONE
                // blending. In particular, native distance fades live here.
                f64::from(*vertex[16..19].iter().max().unwrap()) / 255.0 * f64::from(vertex[19])
                    / 255.0
            } else {
                f64::from(vertex[19]) / 255.0
            };
            if placement.is_none() {
                if let Some(material) = material {
                    strength *= falloff_weight(vertex, point, frame, settings, material)?;
                }
            }
            vertices.push((clip, strength));
        }
        let indices = if let Some(raw) = draw["indices"].as_array() {
            raw.iter()
                .map(|value| {
                    value
                        .as_u64()
                        .and_then(|value| usize::try_from(value).ok())
                        .filter(|index| *index < vertices.len())
                        .ok_or_else(|| "Invalid camera-scoring index".to_owned())
                })
                .collect::<Result<Vec<_>>>()?
        } else if vertices.len() == 4 {
            vec![0, 1, 2, 2, 3, 0]
        } else {
            return Err("Non-quad camera scoring requires explicit indices".into());
        };
        if indices.len() % 3 != 0 {
            return Err("Incomplete camera-scoring triangle".into());
        }
        for triangle in indices.chunks_exact(3) {
            let points = triangle
                .iter()
                .map(|index| vertices[*index].0)
                .collect::<Vec<_>>();
            let polygon = clipped(points);
            if polygon.len() < 3 {
                continue;
            }
            let projected = polygon
                .iter()
                .map(|point| [point[0] / point[3], point[1] / point[3]])
                .collect::<Vec<_>>();
            let signed = projected
                .iter()
                .zip(projected.iter().cycle().skip(1))
                .take(projected.len())
                .map(|(left, right)| left[0] * right[1] - left[1] * right[0])
                .sum::<f64>()
                * 0.5;
            // The installed shaders' native front face is negative in these
            // NDC coordinates. Actual D3D9 index-reversal checks establish
            // mode 3 retains negative winding; mode 2 retains positive.
            if cull == 1 || (cull == 3 && signed < 0.0) || (cull == 2 && signed > 0.0) {
                let weight = triangle.iter().map(|index| vertices[*index].1).sum::<f64>() / 3.0;
                total += signed.abs() * weight;
            }
        }
    }
    // A light can make a light-only scene meaningful. In a mixed scene it
    // must not conceal that every principal particle face is invisible.
    if let Some(lights) = frame["lights"].as_array().filter(|_| draws.is_empty()) {
        for light in lights {
            let has_colour = light["rgb"].as_array().is_some_and(|rgb| {
                rgb.iter()
                    .filter_map(Value::as_f64)
                    .any(|value| value > 0.0)
            });
            if has_colour && light["radius"].as_f64().is_some_and(|radius| radius > 0.0) {
                total += 1.0;
            }
        }
    }
    Ok(total)
}

/// A zero-alpha image is not universally invisible: opaque passes may ignore
/// its alpha. Only skip it when the real material's alpha test rejects zero.
/// This affects camera ranking only; the draw and material remain unchanged.
fn proven_transparent(material: &Value) -> bool {
    if material["previewImageAlphaZero"].as_bool() != Some(true) {
        return false;
    }
    // This admitted, byte-pinned pass multiplies texture and vertex alpha.
    // An unfamiliar opaque shader might manufacture alpha one instead, so its
    // alpha test alone would not establish that the image is invisible.
    if material["pass"].as_str() != Some("vertcol_simple_dtex") {
        return false;
    }
    let Some(test) = material["alphaTest"].as_array() else {
        return false;
    };
    let reference = test.get(1).and_then(Value::as_u64);
    match test.first().and_then(Value::as_u64) {
        // D3DCMP_GREATER rejects alpha zero for every legal reference.
        Some(5) => reference.is_some_and(|reference| reference <= 255),
        // LESS, EQUAL and NOTEQUAL require their exact comparison at zero.
        Some(2) => reference == Some(0),
        Some(3) => reference.is_some_and(|reference| reference > 0 && reference <= 255),
        Some(6) => reference == Some(0),
        _ => false,
    }
}

/// Conservative screen area of the largest active cloud volume, in NDC units
/// (the whole viewport is four). This guides detail framing, not pixel-coverage
/// claims. A volume touching the eye is treated as full-screen/uncertain.
pub(crate) fn cloud_screen_area(frame: &Value, materials: &Value) -> Result<f64> {
    let matrix = &frame["matrixRows"];
    let mut largest = 0.0_f64;
    for draw in frame["draws"].as_array().ok_or("Missing projected draws")? {
        let Some(cloud) = draw.get("cloud") else {
            continue;
        };
        let name = draw["texture"].as_str().ok_or("Missing cloud material")?;
        let material = materials.get(name);
        if material.is_some_and(proven_transparent) {
            continue;
        }
        if cloud["bgra"][3].as_u64().unwrap_or(0) == 0 {
            continue;
        }
        let additive = material.is_some_and(|material| {
            material["sourceBlend"].as_u64() == Some(2)
                && material["destinationBlend"].as_u64() == Some(2)
        });
        if additive
            && cloud["bgra"].as_array().is_some_and(|colour| {
                colour
                    .iter()
                    .take(3)
                    .all(|channel| channel.as_u64() == Some(0))
            })
        {
            continue;
        }
        let bounds = super::cloud_geometry::bounds(cloud)?;
        let mut minimum = [f64::INFINITY; 2];
        let mut maximum = [f64::NEG_INFINITY; 2];
        let mut uncertain = false;
        for corner in 0..8 {
            let point: [f64; 3] =
                std::array::from_fn(|axis| f64::from(bounds[(corner >> axis) & 1][axis]));
            let mut clip = [0.0_f64; 4];
            for row in 0..4 {
                clip[row] = matrix[row][3].as_f64().ok_or("Invalid camera matrix")?;
                for axis in 0..3 {
                    clip[row] +=
                        matrix[row][axis].as_f64().ok_or("Invalid camera matrix")? * point[axis];
                }
            }
            if clip[3] <= 1.0 || !clip.into_iter().all(f64::is_finite) {
                uncertain = true;
                break;
            }
            for axis in 0..2 {
                let projected = clip[axis] / clip[3];
                minimum[axis] = minimum[axis].min(projected);
                maximum[axis] = maximum[axis].max(projected);
            }
        }
        if uncertain {
            return Ok(4.0);
        }
        let width = (maximum[0].clamp(-1.0, 1.0) - minimum[0].clamp(-1.0, 1.0)).max(0.0);
        let height = (maximum[1].clamp(-1.0, 1.0) - minimum[1].clamp(-1.0, 1.0)).max(0.0);
        largest = largest.max(width * height);
    }
    Ok(largest)
}

/// The byte-pinned falloff sprite shaders attenuate vertices according to
/// their normal and the physical eye. Ignoring this can rank a broad face
/// that the real shader makes black. This only guides camera selection; the
/// native shader remains authoritative for the final pixels.
fn falloff_weight(
    vertex: &[u8],
    position: [f32; 3],
    frame: &Value,
    settings: &Value,
    material: &Value,
) -> Result<f64> {
    if !material["pass"]
        .as_str()
        .is_some_and(|pass| pass.contains("falloff"))
    {
        return Ok(1.0);
    }
    let constants = &material["materialConstants"];
    let parameters = super::array::<4>(&constants["falloffParms"], "falloff parameters")?;
    let begin = super::array::<4>(&constants["falloffBeginColor"], "falloff begin colour")?;
    let end = super::array::<4>(&constants["falloffEndColor"], "falloff end colour")?;
    let eye = super::array::<3>(
        frame.get("cameraOrigin").unwrap_or(&settings["origin"]),
        "camera eye",
    )?;
    let normal: [f64; 3] = std::array::from_fn(|axis| f64::from(vertex[24 + axis]) / 127.0 - 1.0);
    let delta: [f64; 3] =
        std::array::from_fn(|axis| f64::from(position[axis]) - f64::from(eye[axis]));
    let normal_squared = normal.iter().map(|value| value * value).sum::<f64>();
    let distance_squared = delta.iter().map(|value| value * value).sum::<f64>();
    if normal_squared == 0.0 || distance_squared == 0.0 {
        return Ok(0.0);
    }
    let dot = normal
        .iter()
        .zip(delta)
        .map(|(normal, delta)| normal * delta)
        .sum::<f64>();
    let facing_squared = dot * dot / (normal_squared * distance_squared);
    let factor =
        (facing_squared * f64::from(parameters[2]) + f64::from(parameters[3])).clamp(0.0, 1.0);
    let colour = (0..3)
        .map(|axis| f64::from(end[axis]) + factor * f64::from(begin[axis] - end[axis]))
        .fold(0.0_f64, f64::max)
        .max(0.0);
    Ok(factor * colour)
}

/// Homogeneous clipping keeps behind-eye and offscreen faces from winning a
/// camera score. Intersections preserve input winding and do not alter the FX.
fn clipped(mut polygon: Vec<[f64; 4]>) -> Vec<[f64; 4]> {
    for plane in 0..7 {
        if polygon.is_empty() {
            break;
        }
        let distance = |point: [f64; 4]| match plane {
            0 => point[3] - 0.000001,
            1 => point[0] + point[3],
            2 => point[3] - point[0],
            3 => point[1] + point[3],
            4 => point[3] - point[1],
            5 => point[2],
            _ => point[3] - point[2],
        };
        let mut output = Vec::new();
        let mut previous = *polygon.last().unwrap();
        let mut previous_distance = distance(previous);
        for current in polygon {
            let current_distance = distance(current);
            if (current_distance >= 0.0) != (previous_distance >= 0.0) {
                let fraction = previous_distance / (previous_distance - current_distance);
                output.push(std::array::from_fn(|axis| {
                    previous[axis] + fraction * (current[axis] - previous[axis])
                }));
            }
            if current_distance >= 0.0 {
                output.push(current);
            }
            previous = current;
            previous_distance = current_distance;
        }
        polygon = output;
    }
    polygon
}
