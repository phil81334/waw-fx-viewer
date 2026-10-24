//! Original FX_GetVelocityAtTime store order for the free-flight preview path.

pub fn evaluate(
    previous: &[f32; 24],
    next: &[f32; 24],
    intervals: f32,
    fraction: f32,
    bits: u32,
    amplitude: [f32; 3],
    axis: [[f32; 3]; 3],
    base: [f32; 3],
) -> [f32; 3] {
    let next_weight = intervals * fraction;
    let previous_weight = intervals - next_weight;
    let channel = |offset: usize| -> [f32; 3] {
        std::array::from_fn(|component| {
            // 0x44EA80 stores the first weighted endpoint before adding the
            // second. Neither randomized endpoint is prematurely rounded.
            let first = ((previous[offset + component] as f64
                + previous[offset + component + 3] as f64 * amplitude[component] as f64)
                * previous_weight as f64) as f32;
            ((next[offset + component] as f64
                + next[offset + component + 3] as f64 * amplitude[component] as f64)
                * next_weight as f64
                + first as f64) as f32
        })
    };
    let mut velocity = base;
    if bits & 0x02000000 != 0 {
        let world = channel(12);
        for component in 0..3 {
            velocity[component] =
                (velocity[component] as f64 + world[component] as f64 * 1000.0) as f32;
        }
    }
    if bits & 0x01000000 != 0 {
        let local = channel(0);
        for component in 0..3 {
            // 0x44E8A0 rotates unscaled values with one final store. Scaling
            // each local component first produces different tail/line axes.
            let rotated = (axis[0][component] as f64 * local[0] as f64
                + axis[1][component] as f64 * local[1] as f64
                + axis[2][component] as f64 * local[2] as f64) as f32;
            velocity[component] = (velocity[component] as f64 + rotated as f64 * 1000.0) as f32;
        }
    }
    velocity
}
