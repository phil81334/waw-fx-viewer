//! Original EffectsEd cloud position routing, isolated from the player.
//!
//! Native free-flight velocity and gravity; no collision or external wind. Frame values already made the
//! native quaternion/axis roundtrip. Do not use this helper to claim support for
//! unverified run-relative-to-offset placement or arbitrary physics.

#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub origin: [f32; 3],
    pub axis: [[f32; 3]; 3],
}

impl Frame {
    pub const IDENTITY: Self = Self {
        origin: [0.0; 3],
        axis: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
    };
}

/// The frame used by the native updater and cloud writer. Axes are basis
/// vectors, matching orientation_t, rather than conventional matrix rows.
pub fn select_frame(bits: u32, at_spawn: Frame, current: Frame) -> Result<Frame, &'static str> {
    match bits & 0xC0 {
        0 => Ok(Frame::IDENTITY),
        0x40 => Ok(at_spawn),
        0x80 => Ok(current),
        // Caller supplies the original per-particle offset frame, rather than
        // the owning effect frame. This route is bounded at particle birth.
        _ => Ok(at_spawn),
    }
}

/// Original 0x44EC80 retains intermediate x87 precision until each output store.
pub fn to_world(frame: Frame, local: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|component| {
        (((frame.axis[0][component] as f64 * local[0] as f64 + frame.origin[component] as f64)
            + frame.axis[1][component] as f64 * local[1] as f64)
            + frame.axis[2][component] as f64 * local[2] as f64) as f32
    })
}

/// Birth uses original 0x44E5B0, rather than the update helper 0x44EC80.
/// Each axis contribution is stored before adding the next. General rotations
/// expose the difference; identity/right-angle test frames can hide it.
pub fn spawn_to_world(frame: Frame, local: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|component| {
        let first = (frame.axis[0][component] as f64 * local[0] as f64
            + frame.origin[component] as f64) as f32;
        let second = (frame.axis[1][component] as f64 * local[1] as f64 + first as f64) as f32;
        (frame.axis[2][component] as f64 * local[2] as f64 + second as f64) as f32
    })
}

/// Original 0x44ECE0 stores its three subtractions before taking the dot products.
pub fn from_world(frame: Frame, world: [f32; 3]) -> [f32; 3] {
    let delta: [f32; 3] =
        std::array::from_fn(|i| (world[i] as f64 - frame.origin[i] as f64) as f32);
    std::array::from_fn(|axis| {
        ((frame.axis[axis][0] as f64 * delta[0] as f64
            + frame.axis[axis][1] as f64 * delta[1] as f64)
            + frame.axis[axis][2] as f64 * delta[2] as f64) as f32
    })
}

/// Route native velocity samples through local storage and world space. Call
/// once for each native update interval, not once for the entire particle age.
/// The returned point is world space; `stored` remains in the chosen frame.
pub fn integrate_step(
    stored: &mut [f32; 3],
    velocity: &[[f32; 24]],
    bits: u32,
    life: f32,
    amplitude: [f32; 3],
    old_age: i64,
    new_age: i64,
    frame: Frame,
) -> Result<[f32; 3], &'static str> {
    integrate_step_with_gravity(
        stored,
        &mut [0.0; 3],
        velocity,
        bits,
        life,
        amplitude,
        [0.0; 2],
        0.0,
        old_age,
        new_age,
        frame,
    )
}

/// Original native gravity updates world velocity and position before the
/// inverse-frame store. Applying it after an inverse/forward roundtrip would
/// introduce additional rounding under rotated emitter frames.
pub fn integrate_step_with_gravity(
    stored: &mut [f32; 3],
    base_velocity: &mut [f32; 3],
    velocity: &[[f32; 24]],
    bits: u32,
    life: f32,
    amplitude: [f32; 3],
    gravity: [f32; 2],
    gravity_random: f32,
    old_age: i64,
    new_age: i64,
    frame: Frame,
) -> Result<[f32; 3], &'static str> {
    if velocity.len() < 2
        || life <= 0.0
        || old_age < 0
        || new_age < old_age
        || new_age as f32 > life
    {
        return Err("invalid cloud motion interval");
    }
    if old_age == new_age || (bits & 0x07000000 == 0 && *base_velocity == [0.0; 3]) {
        // Original zero-velocity free flight preserves the stored local point.
        // Repeating a world/inverse roundtrip would introduce artificial drift
        // under a rotating frame, even though no movement channel is active.
        return Ok(to_world(frame, *stored));
    }
    let count = velocity.len() - 1;
    let t0 = old_age as f32 / life;
    let t1 = new_age as f32 / life;
    let start_point = (count as f64 * t0 as f64) as f32;
    let end_point = (count as f64 * t1 as f64) as f32;
    let start = (start_point.floor() as usize).min(count - 1);
    let end = (end_point.ceil() as usize).saturating_sub(1).min(count - 1);
    let a = (start_point as f64 - start as f64) as f32;
    let b = (end_point as f64 - end as f64) as f32;

    let integrate = |position: &mut [f32; 3], channel: usize| {
        for axis in 0..3 {
            let sample = |index: usize, integral: bool| {
                let component = channel + axis + if integral { 6 } else { 0 };
                velocity[index][component] as f64
                    + amplitude[axis] as f64 * velocity[index][component + 3] as f64
            };
            if start == end {
                let w1 = (life as f64 * 0.5 * (b as f64 * b as f64 - a as f64 * a as f64)) as f32;
                let w0 = ((b as f64 - a as f64) * life as f64 - w1 as f64) as f32;
                position[axis] = (position[axis] as f64 + sample(start, false) * w0 as f64) as f32;
                position[axis] =
                    (position[axis] as f64 + sample(start + 1, false) * w1 as f64) as f32;
            } else {
                for (index, t, sign) in [(end, b, 1.0_f64), (start, a, -1.0)] {
                    let scale = life as f64 * sign;
                    // Original 0x44BEE0 stores time * signed lifespan before
                    // multiplying the triangular weight. Keeping that product
                    // wide introduces measurable drift across curve intervals.
                    let product = (t as f64 * scale) as f32;
                    let w1 = (0.5 * t as f64 * product as f64) as f32;
                    let w0 = (product as f64 - w1 as f64) as f32;
                    position[axis] = (position[axis] as f64 + sample(index, true) * scale) as f32;
                    position[axis] =
                        (position[axis] as f64 + sample(index, false) * w0 as f64) as f32;
                    position[axis] =
                        (position[axis] as f64 + sample(index + 1, false) * w1 as f64) as f32;
                }
            }
        }
    };

    if bits & 0x01000000 != 0 {
        integrate(stored, 0);
    }
    let mut world = to_world(frame, *stored);
    if bits & 0x07000000 == 0x01000000 && *base_velocity == [0.0; 3] {
        // The native local-only branch keeps its accumulated local position.
        // An inverse transform here would introduce rounding on every update.
        return Ok(world);
    }
    if bits & 0x02000000 != 0 {
        integrate(&mut world, 12);
    }
    if bits & 0x04000000 != 0 || *base_velocity != [0.0; 3] {
        let seconds = ((new_age - old_age) as f64 * 0.0010000000474974513) as f32;
        for component in 0..3 {
            world[component] =
                (world[component] as f64 + base_velocity[component] as f64 * seconds as f64) as f32;
        }
        let strength = (gravity[0] as f64 + gravity[1] as f64 * gravity_random as f64) as f32;
        let delta = (strength as f64 * 800.0 * seconds as f64) as f32;
        base_velocity[2] = (base_velocity[2] as f64 - delta as f64) as f32;
        world[2] = (world[2] as f64 - seconds as f64 * delta as f64 * 0.5) as f32;
    }
    *stored = from_world(frame, world);
    Ok(world)
}
