//! Original EffectsEd loose-model texture-coordinate packing.
//!
//! This is the 0x46BAA0 surface preparation plus the custom bit packing at
//! 0x432A10. It is not generic IEEE half conversion. Preserve the surface-wide
//! offset step, including the original first-vertex scan behaviour.

/// Pack one stored f32 component exactly as original 0x432230 / 0x432A10.
/// Values are clamped in the transformed integer domain before combining bits.
pub fn pack_component(value: f32) -> u16 {
    let bits = value.to_bits();
    let transformed = (bits.wrapping_mul(2) ^ 0x8000_3fff) as i32;
    let magnitude = (transformed >> 14).clamp(-0x4000, 0x3fff);
    ((magnitude as u32 & 0x3fff) | ((bits >> 16) & 0xc000)) as u16
}

/// A little-endian dword stores V in its low word and U in its high word.
pub fn pack_pair(uv: [f32; 2]) -> u32 {
    (u32::from(pack_component(uv[0])) << 16) | u32::from(pack_component(uv[1]))
}

/// Original surface preparation before packing, not an intended centroid.
///
/// The original UV scan at 0x46BADA..0x46BC88 never advances its source pointer:
/// each accumulation uses the first vertex again. Keep each f32 store, since
/// a long surface can round that repeated sum differently from first * count.
pub fn surface_uv_offset(uvs: &[[f32; 2]]) -> [f32; 2] {
    if uvs.is_empty() {
        return [0.0; 2];
    }

    let reciprocal = (1.0_f64 / uvs.len() as f64) as f32;
    std::array::from_fn(|axis| {
        let first = uvs[0][axis];
        if (0.0..=1.0).contains(&first) {
            return 0.0;
        }

        let mut sum = 0.0_f32;
        for _ in uvs {
            sum = (f64::from(sum) + f64::from(first)) as f32;
        }
        let mean = (f64::from(sum) * f64::from(reciprocal)) as f32;
        let rounded = (f64::from(mean) + 0.5_f64) as f32;
        rounded.floor()
    })
}

/// Pack a whole source surface before any triangle expansion/reordering.
/// The caller retains the source surface boundary and finite-UV validation.
pub fn pack_surface_uvs(uvs: &[[f32; 2]]) -> Vec<u32> {
    let offset = surface_uv_offset(uvs);
    uvs.iter()
        .map(|uv| {
            pack_pair([
                (f64::from(uv[0]) - f64::from(offset[0])) as f32,
                (f64::from(uv[1]) - f64::from(offset[1])) as f32,
            ])
        })
        .collect()
}
