//! Original packed-light colour operation for an explicit, fixed preview light.
//! The preview supplies the packed value; this is not a baked-world sampler.

/// EffectsEd 0x450a20 uses signed division truncated toward zero and preserves
/// alpha. Keep integer rounding in the same places as the original routine.
pub fn apply(packed: u16, fraction: u8, mut bgra: [u8; 4]) -> [u8; 4] {
    let packed = u32::from(packed);
    let coarse = [(packed & 31) << 3, (packed >> 3) & 248, (packed >> 8) & 248];
    for (channel, index) in [2_usize, 1, 0].into_iter().enumerate() {
        let expanded = coarse[channel] | (coarse[channel] >> 5);
        let multiplier = ((2 * expanded as i32 - 255) * i32::from(fraction)) / 255 + 255;
        bgra[index] = ((multiplier * i32::from(bgra[index])) / 255).clamp(0, 255) as u8;
    }
    bgra
}
