//! Native cloud scene records, independent of the cloud renderer.
//!
//! WaW's FX_DrawElem_Cloud submits a volume, not a sprite. The placement,
//! radii, normalized velocity endpoint and BGRA colour are retained separately
//! so the native volume renderer can consume them without a billboard stand-in.

use crate::fx::frames::Axis;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

fn axis_to_quat(axis: Axis) -> [f32; 4] {
    let m = axis.concat();
    let mut q = [
        m[5] - m[7],
        m[6] - m[2],
        m[1] - m[3],
        ((m[0] as f64 + m[4] as f64 + m[8] as f64) + 1.0) as f32,
    ];
    let squared = |q: [f32; 4]| -> f64 { q.iter().map(|v| *v as f64 * *v as f64).sum() };
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
    // Native AxisToQuat stores the squared length and square root as f32
    // before taking the reciprocal. Skipping either store shifts quaternion
    // low bits enough to change the original cloud shader's output pixels.
    let length_squared = squared(q) as f32;
    let length = (length_squared as f64).sqrt() as f32;
    let reciprocal = (1.0_f64 / length as f64) as f32;
    q.map(|v| v * reciprocal)
}

/// Native FX_AnglesToOrientedAxis uses radians and basis-vector axes.
pub fn oriented_axis(angles: [f32; 3], frame: Axis) -> Axis {
    let [pitch, yaw, roll] = angles;
    // Original 0x44EEF0 executes x87 FSINCOS, then stores each result as
    // f32. The platform's f32 sine shortcut differs by one ULP at real stock
    // angles; evaluating in f64 before the store matches the native fixtures.
    let sin_cos = |angle: f32| {
        let (sine, cosine) = (angle as f64).sin_cos();
        (sine as f32, cosine as f32)
    };
    let (sp, cp) = sin_cos(pitch);
    let (sy, cy) = sin_cos(yaw);
    let (sr, cr) = sin_cos(roll);
    let local = [
        [cp * cy, cp * sy, -sp],
        [
            ((sr as f64 * sp as f64) * cy as f64 - cr as f64 * sy as f64) as f32,
            ((sr as f64 * sp as f64) * sy as f64 + cr as f64 * cy as f64) as f32,
            sr * cp,
        ],
        [
            ((cr as f64 * sp as f64) * cy as f64 + sr as f64 * sy as f64) as f32,
            ((cr as f64 * sp as f64) * sy as f64 - sr as f64 * cy as f64) as f32,
            cr * cp,
        ],
    ];
    // The native orientation multiply (0x44E8A0) keeps all products and
    // additions in the x87 accumulator until one final float32 store.
    local.map(|v| {
        std::array::from_fn(|component| {
            ((frame[0][component] as f64 * v[0] as f64 + frame[1][component] as f64 * v[1] as f64)
                + frame[2][component] as f64 * v[2] as f64) as f32
        })
    })
}

/// Layout verified from EffectsEd3 0x452380/0x451C40: 56 bytes.
pub fn scene_record(
    origin: [f32; 3],
    velocity: [f32; 3],
    axis: Axis,
    scale: f32,
    color: [u8; 4],
    radius: [f32; 2],
) -> Value {
    let quat = axis_to_quat(axis);
    let length = ((velocity[0] as f64 * velocity[0] as f64
        + velocity[1] as f64 * velocity[1] as f64)
        + velocity[2] as f64 * velocity[2] as f64)
        .sqrt() as f32;
    let reciprocal = if length == 0.0 { 1.0 } else { 1.0 / length };
    let direction = velocity.map(|v| v * reciprocal);
    let endpos: [f32; 3] = std::array::from_fn(|i| origin[i] - direction[i]);
    let mut bytes = Vec::with_capacity(56);
    for value in quat.into_iter().chain(origin).chain([scale]).chain(endpos) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&color);
    for value in radius {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    json!({"origin":origin,"quat":quat,"scale":scale,"endpos":endpos,
        "bgra":color,"radius":radius,"sceneRecordBytes":STANDARD.encode(bytes)})
}
