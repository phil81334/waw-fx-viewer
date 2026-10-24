//! Exact native rotation-clock stores; integration owns the retained at-rest byte.
pub fn quantized_rest_time(age: f32, life: f32) -> f32 {
    // Original 0x44C58B subtracts the pinned double at 0x536C20 (0.25).
    let value = (f64::from(age) * 255.0 / f64::from(life) - 0.25) as f32;
    f64::from(value).ceil() as f32
}

pub fn rotation_time(age: f32, life: f32, rest: u8) -> f32 {
    let stopped_at = (f64::from(rest) * f64::from(life) * 0.003921568859368563_f64) as f32;
    let since_rest = (f64::from(age) - f64::from(stopped_at)) as f32;
    if since_rest <= 0.0 {
        return age;
    }
    if since_rest >= 300.0 {
        return (f64::from(stopped_at) + 150.0) as f32;
    }
    (f64::from(age) - f64::from(since_rest) * f64::from(since_rest) * 0.0016666667070239782_f64)
        as f32
}
