//! Original EffectsEd scaled model placement and the native 28-byte mesh layout.
//! Matrices use shader dp4 rows; the source binary stores their transpose.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct Placement {
    pub quat: [f32; 4],
    pub origin: [f32; 3],
    pub scale: f32,
}

/// Original 0x42c270 quaternion conversion. Do not normalize captured placement.
pub fn absolute_rows(placement: &Placement) -> [[f32; 4]; 4] {
    let [x, y, z, w] = placement.quat;
    let xx = (x + x) * x;
    let xy = (x + x) * y;
    let xz = (x + x) * z;
    let xw = (x + x) * w;
    let yy = (y + y) * y;
    let yz = (y + y) * z;
    let yw = (y + y) * w;
    let zz = (z + z) * z;
    let zw = (z + z) * w;
    let axis = [
        [(1.0f64 - (yy as f64 + zz as f64)) as f32, xy + zw, xz - yw],
        [xy - zw, (1.0f64 - (xx as f64 + zz as f64)) as f32, yz + xw],
        [xz + yw, yz - xw, (1.0f64 - (xx as f64 + yy as f64)) as f32],
    ];
    // 0x42af10 scales each basis component before storing the matrix.
    std::array::from_fn(|row| {
        std::array::from_fn(|column| {
            if row == 3 {
                if column == 3 {
                    1.0
                } else {
                    0.0
                }
            } else if column == 3 {
                placement.origin[row]
            } else {
                axis[column][row] * placement.scale
            }
        })
    })
}

/// Original 0x493940 removes the camera origin before storing the world matrix.
pub fn world_rows(placement: &Placement, camera_origin: [f32; 3]) -> [[f32; 4]; 4] {
    let mut rows = absolute_rows(placement);
    for axis in 0..3 {
        rows[axis][3] = placement.origin[axis] - camera_origin[axis];
    }
    rows
}

/// Original 0x42bf80 keeps the dot product in x87 precision until its f32 store.
pub fn world_point(placement: &Placement, point: [f32; 3]) -> [f32; 3] {
    let rows = absolute_rows(placement);
    std::array::from_fn(|row| {
        (((rows[row][0] as f64 * point[0] as f64 + rows[row][1] as f64 * point[1] as f64)
            + rows[row][2] as f64 * point[2] as f64)
            + rows[row][3] as f64) as f32
    })
}

/// Transform all eight corners so rotations and negative scales remain valid.
pub fn bounds(placement: &Placement, local: [[f32; 3]; 2]) -> [[f32; 3]; 2] {
    let mut result = [[f32::INFINITY; 3], [f32::NEG_INFINITY; 3]];
    for corner in 0..8 {
        let point = world_point(
            placement,
            std::array::from_fn(|axis| local[(corner >> axis) & 1][axis]),
        );
        for axis in 0..3 {
            result[0][axis] = result[0][axis].min(point[axis]);
            result[1][axis] = result[1][axis].max(point[axis]);
        }
    }
    result
}

/// Object-local positions, BGRA and whole-surface native packed UVs.
/// The unlit shader reads only position.xyz, BGRA and UV. The sign/tangent
/// placeholders and retained normal word are not a full native stream claim.
pub fn pack_surface(surface: &crate::fx::model_assets::Surface) -> Result<Vec<u8>, String> {
    let count = surface.positions.len() / 3;
    if count == 0
        || surface.positions.len() != count * 3
        || surface.normals.len() != count * 3
        || surface.uvs.len() != count * 2
        || surface.colours.len() != count * 4
    {
        return Err("Model surface has inconsistent vertex attributes".into());
    }
    let uv_pairs: Vec<[f32; 2]> = surface
        .uvs
        .chunks_exact(2)
        .map(|pair| [pair[0], pair[1]])
        .collect();
    let packed_uvs = crate::fx::model_uv::pack_surface_uvs(&uv_pairs);
    let mut output = Vec::with_capacity(count * 32);
    for vertex in 0..count {
        let position = &surface.positions[vertex * 3..vertex * 3 + 3];
        let uv = &surface.uvs[vertex * 2..vertex * 2 + 2];
        let normal = std::array::from_fn(|axis| surface.normals[vertex * 3 + axis]);
        if position
            .iter()
            .chain(uv)
            .chain(normal.iter())
            .any(|value| !value.is_finite())
        {
            return Err("Model surface contains a non-finite vertex attribute".into());
        }
        for value in position {
            output.extend_from_slice(&value.to_le_bytes());
        }
        // Native unlit VS reconstructs w=1 and does not consume binormal sign.
        output.extend_from_slice(&1.0_f32.to_le_bytes());
        output.extend_from_slice(&surface.colours[vertex * 4..vertex * 4 + 4]);
        output.extend_from_slice(&packed_uvs[vertex].to_le_bytes());
        output.extend(crate::fx::geometry::pack_unit_vector(normal));
        output.extend_from_slice(&0_u32.to_le_bytes());
    }
    Ok(output)
}

/// JSON adapter for the already-validated model asset stored on an effect.
pub fn pack_surface_json(surface: &Value) -> Result<Vec<u8>, String> {
    let floats = |name: &str| -> Result<Vec<f32>, String> {
        surface[name]
            .as_array()
            .ok_or_else(|| format!("Missing model {name}"))?
            .iter()
            .map(|value| {
                value
                    .as_f64()
                    .filter(|v| v.is_finite())
                    .map(|v| v as f32)
                    .ok_or_else(|| format!("Invalid model {name}"))
            })
            .collect()
    };
    let colours = surface["colours"]
        .as_array()
        .ok_or("Missing model colours")?
        .iter()
        .map(|value| {
            value
                .as_u64()
                .filter(|v| *v <= 255)
                .map(|v| v as u8)
                .ok_or("Invalid model colour".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    pack_surface(&crate::fx::model_assets::Surface {
        positions: floats("positions")?,
        normals: floats("normals")?,
        uvs: floats("uvs")?,
        colours,
        indices: Vec::new(),
        max_bone: 0,
        skin_indices: Vec::new(),
        skin_weights: Vec::new(),
    })
}
