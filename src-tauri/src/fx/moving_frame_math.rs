//! Native FX_FrameLerp endpoint quaternion normalization and UnitQuatToAxis.
//! Only the fraction-one endpoint is claimed; no sparse frame interpolation.

pub type Axis = [[f32; 3]; 3];

pub fn played_quat(quat: [f32; 4]) -> [f32; 4] {
    let squared = (quat[0] as f64 * quat[0] as f64
        + quat[1] as f64 * quat[1] as f64
        + quat[2] as f64 * quat[2] as f64
        + quat[3] as f64 * quat[3] as f64) as f32;
    let length = (squared as f64).sqrt() as f32;
    if length == 0.0 {
        return quat;
    }
    let reciprocal = (1.0_f64 / length as f64) as f32;
    quat.map(|value| (value as f64 * reciprocal as f64) as f32)
}

pub fn unit_quat_axis(quat: [f32; 4]) -> Axis {
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
    [
        [(1.0_f64 - (yy as f64 + zz as f64)) as f32, xy + zw, xz - yw],
        [xy - zw, (1.0_f64 - (xx as f64 + zz as f64)) as f32, yz + xw],
        [xz + yw, yz - xw, (1.0_f64 - (xx as f64 + yy as f64)) as f32],
    ]
}

/// Native FX_FrameLerp normalizes even when fraction is exactly one.
pub fn played_axis(quat: [f32; 4]) -> Axis {
    unit_quat_axis(played_quat(quat))
}

/// Fraction-one translation arithmetic. The bounded native fixtures prove that
/// it reproduces the current-origin bits; retain the actual expression here.
pub fn played_origin(previous: [f32; 3], current: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|i| ((current[i] as f64 - previous[i] as f64) + previous[i] as f64) as f32)
}
