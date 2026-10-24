//! Physical perspective camera fitting for preview shader inputs.
//!
//! The default preview looks along -X with screen right +Y and screen up +Z.
//! Other views use the same forward/left/up convention and quad winding.
//! Clip W is actual camera distance, not a
//! constant orthographic placeholder; z-feather shaders depend on that value.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

pub(crate) type Axis = [[f32; 3]; 3];

/// Engine camera axes are forward, left and up. Screen right is negative left.
pub(crate) const DEFAULT_AXIS: Axis = [[-1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0]];

/// The legacy view comes first so an already useful preview can keep its view.
pub(crate) const CARDINAL_AXES: [Axis; 6] = [
    DEFAULT_AXIS,
    [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
    [[0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
    [[0.0, 1.0, 0.0], [-1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
    [[0.0, 0.0, -1.0], [0.0, -1.0, 0.0], [-1.0, 0.0, 0.0]],
    [[0.0, 0.0, 1.0], [0.0, -1.0, 0.0], [1.0, 0.0, 0.0]],
];

/// Camera axes are internal preview settings, not authored emitter rotations.
/// Reject a malformed basis before it can affect culling, fades or billboards.
pub(crate) fn parse_axis(value: Option<&Value>) -> Result<Axis, String> {
    let Some(value) = value else {
        return Ok(DEFAULT_AXIS);
    };
    let rows = value
        .as_array()
        .ok_or("camera axis must contain three vectors")?;
    if rows.len() != 3 {
        return Err("camera axis must contain three vectors".into());
    }
    let axis = [point(&rows[0])?, point(&rows[1])?, point(&rows[2])?];
    validate_axis(axis)?;
    Ok(axis)
}

fn dot(left: [f32; 3], right: [f32; 3]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn validate_axis(axis: Axis) -> Result<(), String> {
    if axis
        .iter()
        .flatten()
        .any(|component| !component.is_finite())
    {
        return Err("camera axis must be finite".into());
    }
    for row in 0..3 {
        if (dot(axis[row], axis[row]) - 1.0).abs() > 0.0001 {
            return Err("camera axis vectors must have unit length".into());
        }
        for other in row + 1..3 {
            if dot(axis[row], axis[other]).abs() > 0.0001 {
                return Err("camera axis vectors must be perpendicular".into());
            }
        }
    }
    let forward_cross_left = [
        axis[0][1] * axis[1][2] - axis[0][2] * axis[1][1],
        axis[0][2] * axis[1][0] - axis[0][0] * axis[1][2],
        axis[0][0] * axis[1][1] - axis[0][1] * axis[1][0],
    ];
    if dot(forward_cross_left, axis[2]) < 0.9999 {
        return Err("camera axis must use the forward/left/up handedness".into());
    }
    Ok(())
}

fn point(value: &Value) -> Result<[f32; 3], String> {
    let coordinates = value.as_array().ok_or("camera position must be an array")?;
    if coordinates.len() != 3 {
        return Err("camera position must contain three coordinates".into());
    }

    let mut output = [0.0; 3];
    for axis in 0..3 {
        output[axis] = coordinates[axis]
            .as_f64()
            .ok_or("camera coordinate must be numeric")? as f32;
        if !output[axis].is_finite() {
            return Err("camera coordinate is not finite".into());
        }
    }

    Ok(output)
}

/// Fit the complete rendered geometry across the supplied animation timeline.
/// Draw vertices are authoritative; diagnostic particle vertices are a fallback
/// when a native-comparison report has no encoded draw payload.
pub fn fit(frames: &[Value]) -> Result<Value, String> {
    let mut minimum = [f32::INFINITY; 3];
    let mut maximum = [f32::NEG_INFINITY; 3];
    let mut count = 0_usize;
    let mut include = |position: [f32; 3]| -> Result<(), String> {
        if !position.into_iter().all(f32::is_finite) {
            return Err("cannot fit non-finite geometry".into());
        }
        for axis in 0..3 {
            minimum[axis] = minimum[axis].min(position[axis]);
            maximum[axis] = maximum[axis].max(position[axis]);
        }
        count += 1;
        Ok(())
    };

    for frame in frames {
        if let Some(models) = frame["models"].as_array() {
            for model in models {
                let placement: crate::fx::model_pose::Placement =
                    serde_json::from_value(model.clone()).map_err(|error| error.to_string())?;
                let local: [[f32; 3]; 2] = serde_json::from_value(model["bounds"].clone())
                    .map_err(|error| format!("Missing model bounds: {error}"))?;
                let bounds = crate::fx::model_pose::bounds(&placement, local);
                include(bounds[0])?;
                include(bounds[1])?;
            }
        }
        // Scene lights have no render vertices. Their actual influence bounds
        // make a light-only preview fit possible without fabricating geometry.
        if let Some(lights) = frame["lights"].as_array() {
            for light in lights {
                let origin = point(&light["origin"])?;
                let radius = light["radius"]
                    .as_f64()
                    .ok_or("light radius must be numeric")? as f32;
                if !radius.is_finite() || radius <= 0.0 {
                    return Err("camera light radius must be finite and positive".into());
                }
                for sign in [-1.0, 1.0] {
                    include(origin.map(|coordinate| coordinate + sign * radius))?;
                }
            }
        }
        if let Some(plane) = frame
            .get("previewPlane")
            .filter(|plane| plane["depthOnly"].as_bool() != Some(true))
        {
            let height = plane["height"]
                .as_f64()
                .ok_or("plane height must be numeric")? as f32;
            let size = plane["size"].as_f64().ok_or("plane size must be numeric")? as f32;
            if !height.is_finite() || !size.is_finite() || size <= 0.0 {
                return Err("camera plane bounds must be finite and positive".into());
            }
            match plane["axis"].as_str().unwrap_or("z") {
                "z" => {
                    include([-size, -size, height])?;
                    include([size, size, height])?;
                }
                "x" => {
                    include([height, -size, -size])?;
                    include([height, size, size])?;
                }
                _ => return Err("camera plane axis must be x or z".into()),
            }
        }
        let mut found_draw = false;
        if let Some(draws) = frame["draws"].as_array() {
            for draw in draws {
                if let Some(cloud) = draw.get("cloud") {
                    // Native cloud geometry is a local 20-byte volume stream.
                    // Fit its transformed volume and shader-expanded particles,
                    // rather than interpreting it as packed sprite vertices.
                    for corner in crate::fx::cloud_geometry::bounds(cloud)? {
                        include(corner)?;
                    }
                    found_draw = true;
                    continue;
                }
                let encoded = draw["vertices"]
                    .as_str()
                    .ok_or("draw is missing encoded vertices")?;
                let bytes = STANDARD
                    .decode(encoded)
                    .map_err(|error| error.to_string())?;
                if bytes.len() % 32 != 0 {
                    return Err("draw vertices do not use the 32-byte packed layout".into());
                }
                for vertex in bytes.chunks_exact(32) {
                    let position = std::array::from_fn(|axis| {
                        f32::from_le_bytes(vertex[axis * 4..axis * 4 + 4].try_into().unwrap())
                    });
                    include(position)?;
                    found_draw = true;
                }
            }
        }

        if !found_draw {
            if let Some(particles) = frame["particles"].as_array() {
                for particle in particles {
                    if let Some(vertices) = particle["vertices"].as_array() {
                        for vertex in vertices {
                            include(point(&vertex["position"])?)?;
                        }
                    }
                }
            }
        }
    }

    fit_bounds(minimum, maximum, count)
}

/// Finish a fit from bounds accumulated one frame at a time. This is exactly
/// the same camera construction used by fit; only frame storage is avoided.
pub(crate) fn fit_bounds(
    minimum: [f32; 3],
    maximum: [f32; 3],
    count: usize,
) -> Result<Value, String> {
    if count == 0 {
        return Err("cannot fit a camera without rendered geometry".into());
    }
    if !minimum.into_iter().chain(maximum).all(f32::is_finite)
        || minimum
            .iter()
            .zip(maximum)
            .any(|(lower, upper)| *lower > upper)
    {
        return Err("camera bounds must be finite and ordered".into());
    }

    let center = std::array::from_fn::<_, 3, _>(|axis| minimum[axis] * 0.5 + maximum[axis] * 0.5);
    let radius_y = (maximum[1] - minimum[1]) * 0.5;
    let radius_z = (maximum[2] - minimum[2]) * 0.5;
    let field_of_view_degrees = 60.0_f32;
    let focal = 1.0 / (field_of_view_degrees.to_radians() * 0.5).tan();
    // At maximum X the denominator is smallest. Fitting both extents there
    // conservatively fits every point inside the complete world-space AABB.
    let clearance = (focal * radius_y.max(radius_z) / 0.8).max(128.0);
    let origin = [maximum[0] + clearance, center[1], center[2]];
    let near = 1.0_f32;
    let far = (origin[0] - minimum[0] + 128.0).max(8192.0);
    if !origin.into_iter().all(f32::is_finite) || !far.is_finite() {
        return Err("geometry exceeds the preview camera's finite range".into());
    }

    let mut camera = json!({
        "origin": origin,
        "axis": [[-1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0]],
        "bounds": [minimum, maximum],
        "fovDegrees": field_of_view_degrees,
        "near": near,
        "far": far,
        "viewport": [384, 384],
        "fitNdcExtent": 0.8,
        "vertexSamples": count,
    });
    camera["matrixRows"] = matrix(&camera);
    Ok(camera)
}

/// Fit the same world bounds from a different physical viewing direction.
/// World-space geometry remains unchanged; only the camera basis and origin move.
pub(crate) fn fit_bounds_for_axis(
    minimum: [f32; 3],
    maximum: [f32; 3],
    count: usize,
    axis: Axis,
) -> Result<Value, String> {
    validate_axis(axis)?;
    // Retain the established f32 expression order and signed zeroes verbatim.
    if axis == DEFAULT_AXIS {
        return fit_bounds(minimum, maximum, count);
    }
    if count == 0 {
        return Err("cannot fit a camera without rendered geometry".into());
    }
    if !minimum.into_iter().chain(maximum).all(f32::is_finite)
        || minimum
            .iter()
            .zip(maximum)
            .any(|(lower, upper)| *lower > upper)
    {
        return Err("camera bounds must be finite and ordered".into());
    }

    let center =
        std::array::from_fn::<_, 3, _>(|index| minimum[index] * 0.5 + maximum[index] * 0.5);
    let extent = std::array::from_fn::<_, 3, _>(|index| (maximum[index] - minimum[index]) * 0.5);
    // Projecting an AABB onto an axis uses the sum of its absolute components.
    // This also supports a validated non-cardinal axis without clipping corners.
    let radii = axis.map(|row| dot(row.map(f32::abs), extent));
    let field_of_view_degrees = 60.0_f32;
    let focal = 1.0 / (field_of_view_degrees.to_radians() * 0.5).tan();
    let clearance = (focal * radii[1].max(radii[2]) / 0.8).max(128.0);
    let distance = radii[0] + clearance;
    let origin = std::array::from_fn::<_, 3, _>(|index| center[index] - axis[0][index] * distance);
    let near = 1.0_f32;
    let far = (radii[0] * 2.0 + clearance + 128.0).max(8192.0);
    if !origin.into_iter().all(f32::is_finite) || !far.is_finite() {
        return Err("geometry exceeds the preview camera's finite range".into());
    }

    let mut camera = json!({
        "origin": origin,
        "axis": axis,
        "bounds": [minimum, maximum],
        "fovDegrees": field_of_view_degrees,
        "near": near,
        "far": far,
        "viewport": [384, 384],
        "fitNdcExtent": 0.8,
        "vertexSamples": count,
    });
    camera["matrixRows"] = matrix(&camera);
    Ok(camera)
}

/// World-to-clip rows for the square, +Y-right, +Z-up perspective preview.
pub fn matrix(camera: &Value) -> Value {
    let origin = camera["origin"]
        .as_array()
        .expect("validated camera origin");
    let x = origin[0].as_f64().expect("validated camera x") as f32;
    let y = origin[1].as_f64().expect("validated camera y") as f32;
    let z = origin[2].as_f64().expect("validated camera z") as f32;
    let fov = camera["fovDegrees"].as_f64().unwrap_or(60.0) as f32;
    let near = camera["near"].as_f64().unwrap_or(1.0) as f32;
    let far = camera["far"].as_f64().unwrap_or(8192.0) as f32;
    let focal = 1.0 / (fov.to_radians() * 0.5).tan();
    let q = far / (far - near);

    let axis = parse_axis(camera.get("axis")).expect("validated camera axis");
    if axis != DEFAULT_AXIS {
        let view = view_matrix(camera);
        let row = |index: usize| -> [f32; 4] {
            std::array::from_fn(|column| {
                view[index][column].as_f64().expect("validated view row") as f32
            })
        };
        let horizontal = row(0).map(|component| focal * component);
        let vertical = row(1).map(|component| focal * component);
        let forward = row(2);
        let mut depth = forward.map(|component| q * component);
        depth[3] -= q * near;
        return json!([horizontal, vertical, depth, forward]);
    }

    json!([
        [0.0, focal, 0.0, -focal * y],
        [0.0, 0.0, focal, -focal * z],
        [-q, 0.0, 0.0, q * x - q * near],
        [-1.0, 0.0, 0.0, x],
    ])
}

/// Keep view and projection separate for original native cloud shaders.
/// Ordinary sprite shaders continue to use the unchanged combined matrix.
pub fn view_matrix(camera: &Value) -> Value {
    let origin = camera["origin"]
        .as_array()
        .expect("validated camera origin");
    let x = origin[0].as_f64().expect("validated camera x") as f32;
    let y = origin[1].as_f64().expect("validated camera y") as f32;
    let z = origin[2].as_f64().expect("validated camera z") as f32;
    let axis = parse_axis(camera.get("axis")).expect("validated camera axis");
    if axis != DEFAULT_AXIS {
        let origin = [x, y, z];
        let eye_axes = [axis[1].map(|component| -component), axis[2], axis[0]];
        let rows = eye_axes.map(|row| [row[0], row[1], row[2], -dot(row, origin)]);
        return json!([rows[0], rows[1], rows[2], [0.0, 0.0, 0.0, 1.0]]);
    }
    json!([
        [0.0, 1.0, 0.0, -y],
        [0.0, 0.0, 1.0, -z],
        [-1.0, 0.0, 0.0, x],
        [0.0, 0.0, 0.0, 1.0],
    ])
}

pub fn projection_matrix(camera: &Value) -> Value {
    let fov = camera["fovDegrees"].as_f64().unwrap_or(60.0) as f32;
    let near = camera["near"].as_f64().unwrap_or(1.0) as f32;
    let far = camera["far"].as_f64().unwrap_or(8192.0) as f32;
    let focal = 1.0 / (fov.to_radians() * 0.5).tan();
    let q = far / (far - near);
    json!([
        [focal, 0.0, 0.0, 0.0],
        [0.0, focal, 0.0, 0.0],
        [0.0, 0.0, q, -q * near],
        [0.0, 0.0, 1.0, 0.0],
    ])
}
