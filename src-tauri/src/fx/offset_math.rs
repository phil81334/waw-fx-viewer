//! Original EffectsEd offset helpers with explicit native float stores.
//! This file only calculates points; root owns frame storage and admission.

pub fn sample_range(range: [f32; 2], random: f32) -> f32 {
    (range[0] as f64 + range[1] as f64 * random as f64) as f32
}

/// 0x44e680: mutate an existing world point. Offset-relative frame construction
/// supplies zero here, then separately adds the authored spawn-box world point.
/// Ordinary birth supplies that spawn-box world point directly instead.
pub fn apply_offset(
    base: [f32; 3],
    axis: [[f32; 3]; 3],
    sphere: bool,
    radius_range: [f32; 2],
    height_range: [f32; 2],
    random_yaw: f32,
    random_height: f32,
    random_radius: f32,
) -> [f32; 3] {
    let radius = sample_range(radius_range, random_radius);
    // The native qword happens to hold exactly this float-valued constant.
    let yaw = (random_yaw as f64 * 6.2831854820251465_f64) as f32;
    let cosine = yaw.cos();
    let sine = yaw.sin();
    if sphere {
        let height = (random_height as f64 * 2.0 - 1.0) as f32;
        let squared = (1.0_f64 - height as f64 * height as f64) as f32;
        let radial = (squared as f64).sqrt() as f32;
        let direction = [radial * cosine, radial * sine, height];
        return std::array::from_fn(|component| {
            (base[component] as f64 + radius as f64 * direction[component] as f64) as f32
        });
    }
    let cosine_radius = radius * cosine;
    let sine_radius = radius * sine;
    let height = sample_range(height_range, random_height);
    std::array::from_fn(|component| {
        // 0x4010d0 retains both products and the base addition before storing.
        let circular = ((axis[1][component] as f64 * cosine_radius as f64 + base[component] as f64)
            + axis[2][component] as f64 * sine_radius as f64) as f32;
        // 0x44e74f adds the axial contribution in a separate store.
        (circular as f64 + axis[0][component] as f64 * height as f64) as f32
    })
}

/// 0x44e9cd followed by 0x4309d0/0x42a720: normalize the offset ONCE, then
/// construct its sphere basis. Renormalizing forward changes this frame.
pub fn sphere_basis(offset: [f32; 3]) -> [[f32; 3]; 3] {
    let squared = ((offset[0] as f64 * offset[0] as f64 + offset[1] as f64 * offset[1] as f64)
        + offset[2] as f64 * offset[2] as f64) as f32;
    let length = (squared as f64).sqrt() as f32;
    let forward = if length == 0.0 {
        [1.0, 0.0, 0.0]
    } else {
        let reciprocal = (1.0_f64 / length as f64) as f32;
        offset.map(|component| component * reciprocal)
    };
    let squares = forward.map(|component| component * component);
    let mut smallest = 0usize;
    for component in 1..3 {
        if squares[component] < squares[smallest] {
            smallest = component;
        }
    }
    let mut perpendicular = forward.map(|component| -forward[smallest] * component);
    perpendicular[smallest] += 1.0;
    let squared = ((perpendicular[0] as f64 * perpendicular[0] as f64
        + perpendicular[1] as f64 * perpendicular[1] as f64)
        + perpendicular[2] as f64 * perpendicular[2] as f64) as f32;
    let length = (squared as f64).sqrt() as f32;
    let reciprocal = if length == 0.0 {
        1.0
    } else {
        (1.0_f64 / length as f64) as f32
    };
    let up = perpendicular.map(|component| component * reciprocal);
    // Compute the reversed cross directly, preserving native signed zero.
    let left = [
        (forward[2] as f64 * up[1] as f64 - forward[1] as f64 * up[2] as f64) as f32,
        (forward[0] as f64 * up[2] as f64 - forward[2] as f64 * up[0] as f64) as f32,
        (forward[1] as f64 * up[0] as f64 - forward[0] as f64 * up[1] as f64) as f32,
    ];
    [forward, left, up]
}
