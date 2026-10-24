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

/// Original FX_FrameLerp 0x44B440. This preserves quaternion signs: native code
/// does not flip endpoints, choose a shortest arc, or perform spherical lerp.
pub fn lerp_raw(
    begin_quat: [f32; 4],
    begin_origin: [f32; 3],
    end_quat: [f32; 4],
    end_origin: [f32; 3],
    fraction: f32,
) -> ([f32; 4], [f32; 3]) {
    let mixed = std::array::from_fn(|i| {
        ((end_quat[i] as f64 - begin_quat[i] as f64) * fraction as f64 + begin_quat[i] as f64)
            as f32
    });
    let origin = std::array::from_fn(|i| {
        ((end_origin[i] as f64 - begin_origin[i] as f64) * fraction as f64 + begin_origin[i] as f64)
            as f32
    });
    (played_quat(mixed), origin)
}

/// Host-facing bounded route. A zero quaternion is the native arithmetic's
/// real result for an exactly antipodal midpoint, but it is not a usable frame.
pub fn lerp_frame(
    begin_quat: [f32; 4],
    begin_origin: [f32; 3],
    end_quat: [f32; 4],
    end_origin: [f32; 3],
    fraction: f32,
) -> Result<([f32; 4], [f32; 3]), &'static str> {
    if !fraction.is_finite()
        || !(0.0..=1.0).contains(&fraction)
        || begin_quat
            .into_iter()
            .chain(end_quat)
            .chain(begin_origin)
            .chain(end_origin)
            .any(|value| !value.is_finite())
    {
        return Err("Sparse frame inputs must be finite and fraction in [0, 1]");
    }
    let (quat, origin) = lerp_raw(begin_quat, begin_origin, end_quat, end_origin, fraction);
    if quat.iter().all(|value| *value == 0.0) {
        return Err("Sparse frame interpolation produced a zero quaternion");
    }
    Ok((quat, origin))
}
