//! Sprite geometry for the visual effect player.
//!
//! Adapted from the MIT-licensed iw3xo-radiant `fx_draw.cpp`, pinned at
//! 665347c2e90671068088ddc4cd58291194cf9423. WaW's additional `line` anchoring
//! is verified against the stock EffectsEd3 handler at 0x451fd0.
//! The app's ATTRIBUTION.md retains the upstream attribution.

pub type Vector = [f32; 3];
pub type Axis = [Vector; 3];

#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub centre: Vector,
    pub tangent: Vector,
    pub binormal: Vector,
    pub normal: Vector,
}

fn cross(left: Vector, right: Vector) -> Vector {
    // EffectsEd keeps each multiply/subtract in its x87 register until the
    // resulting component is stored. Rounding either product first changes
    // the facing basis of some tail and line particles.
    [
        (left[1] as f64 * right[2] as f64 - left[2] as f64 * right[1] as f64) as f32,
        (left[2] as f64 * right[0] as f64 - left[0] as f64 * right[2] as f64) as f32,
        (left[0] as f64 * right[1] as f64 - left[1] as f64 * right[0] as f64) as f32,
    ]
}

fn normalize(vector: Vector) -> Vector {
    // Native Vec3Normalize (0x41A8E0) stores the squared length, square root
    // and reciprocal separately. Its final operation is multiplication by
    // that stored reciprocal, rather than division by the length.
    let squared_length = (vector[0] as f64 * vector[0] as f64
        + vector[1] as f64 * vector[1] as f64
        + vector[2] as f64 * vector[2] as f64) as f32;
    let length = squared_length.sqrt();

    // Native Vec3Normalize leaves an exactly zero vector unchanged. Inventing
    // an arbitrary camera-facing fallback would change the observed geometry.
    if length == 0.0 {
        return vector;
    }

    let inverse = (1.0_f64 / length as f64) as f32;
    vector.map(|component| (component as f64 * inverse as f64) as f32)
}

/// Construct the unrotated basis used by the native sprite writer.
///
/// `orientation` is the camera axis for billboards, and the resolved element
/// frame from FX_GetOrientation for oriented sprites. It is not the model's
/// separately sampled spawn-angle/angle-velocity placement axis.
/// `velocity` must be the instantaneous world velocity, not displacement.
/// `half_length` is sampled visual size[1], before visual rotation.
pub fn basis(
    kind: &str,
    position: Vector,
    velocity: Vector,
    half_length: f32,
    camera: Vector,
    orientation: Axis,
) -> Result<Geometry, String> {
    match kind {
        "billboardSprite" => Ok(Geometry {
            centre: position,
            normal: orientation[0].map(|component| -component),
            tangent: orientation[1].map(|component| -component),
            binormal: orientation[2],
        }),
        "orientedSprite" => Ok(Geometry {
            centre: position,
            normal: orientation[0],
            tangent: orientation[1],
            binormal: orientation[2],
        }),
        "tail" | "line" => {
            let direction = normalize(velocity);
            let sign = if kind == "tail" { -1.0_f64 } else { 1.0_f64 };
            // Native tail/line anchoring retains the product until the final
            // position store, before constructing the camera-facing basis.
            let centre = std::array::from_fn(|axis| {
                (position[axis] as f64 + sign * half_length as f64 * direction[axis] as f64) as f32
            });
            let camera_delta = std::array::from_fn(|axis| camera[axis] - centre[axis]);
            let tangent = normalize(cross(direction, camera_delta));

            Ok(Geometry {
                centre,
                tangent,
                binormal: direction,
                normal: cross(tangent, direction),
            })
        }
        _ => Err(format!("unsupported quad geometry kind {kind}")),
    }
}

/// Native FX_GenSpriteVerts rotation basis, shared by positions and tangents.
pub fn rotated_axes(geometry: &Geometry, rotation: f32) -> (Vector, Vector) {
    // Original 450ED3 uses x87 FSINCOS followed by f32 stores. The f32
    // platform shortcuts differ by one ULP for real sprite rotation angles.
    let cosine = (rotation as f64).cos() as f32;
    let sine = (rotation as f64).sin() as f32;
    // Original 0x450EE3..0x450F5A stores each rotated axis component only
    // after both products have been combined. Keep those products unrounded
    // until that store, just as in the native basis construction above.
    let left = std::array::from_fn(|axis| {
        (cosine as f64 * geometry.tangent[axis] as f64
            + sine as f64 * geometry.binormal[axis] as f64) as f32
    });
    let up = std::array::from_fn(|axis| {
        (sine as f64 * geometry.tangent[axis] as f64
            - cosine as f64 * geometry.binormal[axis] as f64) as f32
    });

    (left, up)
}

/// Native vertex order: left+up, left-up, right-up, right+up.
/// UV order is (s,t+dt), (s,t), (s+ds,t), (s+ds,t+dt).
pub fn corners(
    geometry: &Geometry,
    rotation: f32,
    half_width: f32,
    half_height: f32,
) -> [Vector; 4] {
    let (rotated_left, rotated_up) = rotated_axes(geometry, rotation);
    let left = rotated_left.map(|component| half_width * component);
    let up = rotated_up.map(|component| half_height * component);
    let signs = [(-1.0, 1.0), (-1.0, -1.0), (1.0, -1.0), (1.0, 1.0)];

    signs.map(|(left_sign, up_sign)| {
        std::array::from_fn(|axis| {
            (geometry.centre[axis] + left_sign * left[axis]) + up_sign * up[axis]
        })
    })
}

/// GfxPackedVertex's UNORM byte representation, matching the native truncating
/// normal/tangent writer. This is not an XYZ signed-byte normal encoding.
pub fn pack_unit_vector(vector: Vector) -> [u8; 4] {
    // WaW's x87 writer multiplies/adds double constants before truncating.
    // Rounding the intermediate to f32 can push a value across a byte boundary.
    let encode = |component: f32| (component as f64 * 127.0 + 127.5) as u8;
    [encode(vector[0]), encode(vector[1]), encode(vector[2]), 63]
}
