//! WaW cloud renderer geometry recovered from the pinned EffectsEd3 backend.
//!
//! The renderer expands a fixed jittered 8 x 8 x 16 volume into 1,024 quads.
//! These are original cloud primitives, not a replacement billboard approximation.

use serde_json::Value;

pub fn static_mesh(seed: u32) -> (Vec<u8>, Vec<u16>) {
    let mut state = seed;
    let mut vertices = Vec::with_capacity(4096 * 20);
    let mut indices = Vec::with_capacity(6144);
    let mut random = || {
        state = state.wrapping_mul(214013).wrapping_add(2531011);
        ((state >> 16) & 32767) as f64 / 32767.0
    };
    for x in 0..8 {
        for y in 0..8 {
            for z in 0..16 {
                // Native x87 computes each expression before the f32 store.
                let center = [
                    ((x as f64 + random()) * 0.25 - 1.0) as f32,
                    ((y as f64 + random()) * 0.25 - 1.0) as f32,
                    ((z as f64 + random()) * 0.125 - 1.0) as f32,
                ];
                let base = (vertices.len() / 20) as u16;
                for uv in [[0.0f32, 0.0], [0.0, 1.0], [1.0, 0.0], [1.0, 1.0]] {
                    for value in center.into_iter().chain(uv) {
                        vertices.extend_from_slice(&value.to_le_bytes());
                    }
                }
                indices.extend([base, base + 1, base + 2, base + 2, base + 1, base + 3]);
            }
        }
    }
    (vertices, indices)
}

/// Native cloud constant writer, 0x4B0980 and its projection helper 0x4B08A0.
/// `view_axes` maps a world direction into the three eye-space components.
pub fn matrix(
    origin: [f32; 3],
    end: [f32; 3],
    radius: [f32; 2],
    view_axes: [[f32; 3]; 3],
) -> [f32; 4] {
    let simple = [radius[0], 0.0, 0.0, radius[1]];
    let delta = std::array::from_fn::<_, 3, _>(|i| end[i] - origin[i]);
    if radius[0] == radius[1] || delta.iter().all(|value| value.abs() <= 0.001) {
        return simple;
    }
    let squared_length = delta.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() as f32;
    let length = squared_length.sqrt();
    let reciprocal = (1.0f64 / length as f64) as f32;
    let direction = delta.map(|v| (v as f64 * reciprocal as f64) as f32);
    let scaled = direction.map(|v| v * radius[1]);
    let projected: [f32; 3] = view_axes.map(|row| {
        ((row[0] as f64 * scaled[0] as f64 + row[1] as f64 * scaled[1] as f64)
            + row[2] as f64 * scaled[2] as f64) as f32
    });
    let [x, y, _] = projected;
    // This signed threshold is what the stock backend actually uses. Do not
    // replace it with an absolute-value test: negative projected directions
    // intentionally take the simple matrix branch in this binary.
    if x < 0.001 && y < 0.001 {
        return simple;
    }
    let squared_norm = ((y as f64 * y as f64) + (x as f64 * x as f64)) as f32;
    let norm = squared_norm.sqrt();
    let ratio = (radius[0] as f64 / norm as f64) as f32;
    let major = if radius[0] >= norm { ratio } else { 1.0 };
    [y * ratio, -x * ratio, x * major, y * major]
}

pub fn color(bgra: [u8; 4]) -> [f32; 4] {
    let scale = 0.003921568859368563f64;
    [bgra[2], bgra[1], bgra[0], bgra[3]].map(|v| (v as f64 * scale) as f32)
}

/// Original scaled-placement transform followed by the camera view matrix.
/// Returned rows are ready for the original shader's dp4 constant layout.
pub fn world_view_rows(
    quat: [f32; 4],
    origin: [f32; 3],
    scale: f32,
    view: [[f32; 4]; 4],
) -> [[f32; 4]; 4] {
    let [x, y, z, w] = quat;
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
    let world: [[f32; 4]; 4] = std::array::from_fn(|row| {
        std::array::from_fn(|col| {
            if row == 3 {
                if col == 3 {
                    1.0
                } else {
                    0.0
                }
            } else if col == 3 {
                origin[row]
            } else {
                axis[col][row] * scale
            }
        })
    });
    std::array::from_fn(|row| {
        std::array::from_fn(|col| {
            (0..4)
                .map(|k| view[row][k] as f64 * world[k][col] as f64)
                .sum::<f64>() as f32
        })
    })
}

/// Conservative bounds of the actual local cloud volume plus billboard extent.
/// Used only to fit the preview camera; it does not generate visible geometry.
pub fn bounds(cloud: &Value) -> Result<[[f32; 3]; 2], String> {
    let number = |name: &str, index: usize| {
        cloud[name][index]
            .as_f64()
            .filter(|v| v.is_finite())
            .map(|v| v as f32)
            .ok_or_else(|| format!("Missing finite cloud {name}[{index}]"))
    };
    let origin = [
        number("origin", 0)?,
        number("origin", 1)?,
        number("origin", 2)?,
    ];
    let scale = cloud["scale"]
        .as_f64()
        .filter(|v| v.is_finite())
        .ok_or("Missing finite cloud scale")? as f32;
    let radius = number("radius", 0)?.abs().max(number("radius", 1)?.abs());
    // A rotated cube of half-width |scale| fits this sphere. Billboard corner
    // distance is bounded by the sum of its two half-axis lengths.
    let extent = scale.abs() * 3.0f32.sqrt() + radius;
    Ok([origin.map(|v| v - extent), origin.map(|v| v + extent)])
}
