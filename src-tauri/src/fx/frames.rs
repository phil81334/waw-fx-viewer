//! Native effect-frame operations for the visual effect player.
//!
//! Directional emission follows iw3xo-radiant's FX_ProcessEmitting, using
//! PerpendicularVector and a right-handed cross product. The companion native
//! fixtures execute WaW EffectsEd3 in isolated synthetic emulator memory.

pub type Axis = [[f32; 3]; 3];
pub const IDENTITY: Axis = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// Axes are basis vectors, as in the native orientation_t, rather than rows
/// of a conventional column-vector transformation matrix.
pub fn transform(axis: Axis, vector: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|component| {
        (axis[0][component] * vector[0] + axis[1][component] * vector[1])
            + axis[2][component] * vector[2]
    })
}

/// Child allocation stores a quaternion, then the updater reconstructs its
/// axis. Keep this real format roundtrip rather than preserving a mathematically
/// cleaner source axis that the native player no longer has.
pub fn stored_axis(axis: Axis) -> Axis {
    let m = axis.concat();
    let mut q = [
        m[5] - m[7],
        m[6] - m[2],
        m[1] - m[3],
        ((m[0] as f64 + m[4] as f64 + m[8] as f64) + 1.0) as f32,
    ];
    let squared = |q: [f32; 4]| -> f64 {
        q.iter()
            .map(|component| *component as f64 * *component as f64)
            .sum()
    };
    if (squared(q) as f32) < 1.0 {
        q = [
            m[6] + m[2],
            m[7] + m[5],
            ((m[8] as f64 - m[4] as f64 - m[0] as f64) + 1.0) as f32,
            q[2],
        ];
        if (squared(q) as f32) < 1.0 {
            q = [
                ((m[0] as f64 - m[4] as f64 - m[8] as f64) + 1.0) as f32,
                m[3] + m[1],
                q[0],
                m[5] - m[7],
            ];
            if (squared(q) as f32) < 1.0 {
                q = [
                    q[1],
                    ((m[4] as f64 - m[0] as f64 - m[8] as f64) + 1.0) as f32,
                    m[7] + m[5],
                    m[6] - m[2],
                ];
            }
        }
    }
    // AxisToQuat stores the squared norm and its square root separately as
    // float32 before taking the reciprocal. Child frame storage must keep
    // those native rounding boundaries, just like cloud scene quaternions.
    let length_squared = squared(q) as f32;
    let length = (length_squared as f64).sqrt() as f32;
    let reciprocal = (1.0_f64 / length as f64) as f32;
    q = q.map(|value| value * reciprocal);
    let [x, y, z, w] = q;
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

pub fn emission_axis(delta: [f32; 3]) -> Result<Axis, String> {
    // Original Vec3Normalize stores the sum of squares, square root and
    // reciprocal independently as float32; the x87 dot product stays wide.
    let normalize = |vector: [f32; 3]| {
        let squared = ((vector[0] as f64 * vector[0] as f64 + vector[1] as f64 * vector[1] as f64)
            + vector[2] as f64 * vector[2] as f64) as f32;
        let length = (squared as f64).sqrt() as f32;
        let reciprocal = if length == 0.0 {
            1.0
        } else {
            (1.0_f64 / length as f64) as f32
        };
        (vector.map(|value| value * reciprocal), length)
    };
    let (forward, length) = normalize(delta);
    if length == 0.0 || !length.is_finite() {
        return Err("emission direction must be finite and nonzero".into());
    }
    let squares = forward.map(|component| component * component);
    let mut smallest = 0;
    for component in 1..3 {
        if squares[component] < squares[smallest] {
            smallest = component;
        }
    }
    let mut perpendicular = forward.map(|component| -forward[smallest] * component);
    perpendicular[smallest] += 1.0;
    let (right, _) = normalize(perpendicular);
    // The native inline cross product stores after subtraction, rather than
    // rounding each product first. Near-zero components retain those bits.
    let up = [
        (forward[1] as f64 * right[2] as f64 - forward[2] as f64 * right[1] as f64) as f32,
        (forward[2] as f64 * right[0] as f64 - forward[0] as f64 * right[2] as f64) as f32,
        (forward[0] as f64 * right[1] as f64 - forward[1] as f64 * right[0] as f64) as f32,
    ];
    Ok([forward, right, up])
}
