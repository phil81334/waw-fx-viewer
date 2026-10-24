//! Lossless IWI wavelet reconstruction. Adapted from OpenAssetTools; see ATTRIBUTION.md.
//! The bitstream is LSB-first and predicts each mip from the next smaller one.
use super::wavelet_codes::{ALPHA, BLUE, RED_GREEN};
use std::sync::OnceLock;

// Keep lookup storage on the heap. A standalone Windows debug executable has a
// small default stack; constructing three 4096-entry arrays there can overflow.
// Lookup contents and bitstream arithmetic remain unchanged.
type Table = Vec<(i32, usize)>;

fn table(codes: &[(usize, usize, i32)]) -> Table {
    let mut result = vec![(0, 0); 4096];
    for &(code, bits, value) in codes {
        for index in (code..4096).step_by(1 << bits) {
            result[index] = (value, bits);
        }
    }
    result
}

struct Reader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl Reader<'_> {
    // Huffman lookahead may extend past the final byte. Consuming bits may not.
    fn peek(&self, count: usize) -> u32 {
        let mut value = 0;
        for index in 0..count {
            let position = self.bit + index;
            if let Some(byte) = self.data.get(position / 8) {
                value |= (((byte >> (position % 8)) & 1) as u32) << index;
            }
        }
        value
    }

    fn read(&mut self, count: usize) -> Result<i32, String> {
        if self.bit + count > self.data.len() * 8 {
            return Err("Incomplete wavelet bitstream.".into());
        }
        let value = self.peek(count);
        self.bit += count;
        Ok(value as i32)
    }

    fn value(&mut self, lookup: &Table, bits: usize, bias: i32) -> Result<i32, String> {
        let (value, count) = lookup[self.peek(12) as usize];
        self.read(count)?;
        if value == -32768 {
            return Ok(self.read(bits)? - bias);
        }
        Ok(value)
    }

    fn coefficients(&mut self, lookup: &Table, bits: usize, bias: i32) -> Result<[i32; 3], String> {
        Ok([
            self.value(lookup, bits, bias)?,
            self.value(lookup, bits, bias)?,
            self.value(lookup, bits, bias)?,
        ])
    }
}

fn reconstruct(base: u8, parity: i32, coefficients: [i32; 3]) -> [u8; 4] {
    let base = 2 * i32::from(base);
    let [horizontal, vertical, diagonal] = coefficients;
    // Casting intentionally wraps, matching the original decoder's byte stores.
    [
        (parity + ((diagonal + vertical + horizontal + base) >> 1)) as u8,
        ((horizontal + base - diagonal - vertical) >> 1) as u8,
        ((vertical - diagonal + base - horizontal) >> 1) as u8,
        ((base - horizontal - vertical + diagonal) >> 1) as u8,
    ]
}

pub fn decode(
    data: &[u8],
    width: usize,
    height: usize,
    levels: usize,
    faces: usize,
    channels: usize,
) -> Result<Vec<Vec<Vec<u8>>>, String> {
    if !width.is_power_of_two() || !height.is_power_of_two() {
        return Err("Wavelet dimensions must be powers of two.".into());
    }
    if levels == 1 && width > 1 && height > 1 {
        return Err("Wavelet image is missing its predictor mipmaps.".into());
    }
    static TABLES: OnceLock<[Table; 3]> = OnceLock::new();
    let [blue, red_green, alpha] =
        TABLES.get_or_init(|| [table(BLUE), table(RED_GREEN), table(ALPHA)]);
    let mut reader = Reader { data, bit: 0 };
    let mut output = vec![vec![Vec::new(); faces]; levels];

    for mip in (0..levels).rev() {
        let w = (width >> mip).max(1);
        let h = (height >> mip).max(1);
        for face in 0..faces {
            let mut pixels = vec![0; w * h * channels];
            if w == 1 || h == 1 {
                for value in &mut pixels {
                    *value = reader.read(8)? as u8;
                }
            } else {
                let mut predictor =
                    output.get(mip + 1).ok_or("Missing wavelet predictor.")?[face].clone();
                if reader.read(1)? != 0 {
                    for value in &mut predictor {
                        *value = (i32::from(*value) + reader.value(alpha, 9, 255)?) as u8;
                    }
                }
                for y in (0..h).step_by(2) {
                    for x in (0..w).step_by(2) {
                        let source = ((y / 2) * (w / 2) + x / 2) * channels;
                        let destinations = [
                            y * w + x,
                            y * w + x + 1,
                            (y + 1) * w + x,
                            (y + 1) * w + x + 1,
                        ];
                        let mut blue_coefficients = [0; 3];
                        for channel in 0..channels {
                            let parity = reader.read(1)?;
                            let coefficients;
                            if channels == 1 || channel == channels - 1 && channels != 3 {
                                coefficients = reader.coefficients(alpha, 9, 255)?;
                            } else if channel == 0 {
                                blue_coefficients = reader.coefficients(blue, 9, 255)?;
                                coefficients = blue_coefficients;
                            } else {
                                let differences = reader.coefficients(red_green, 10, 510)?;
                                coefficients =
                                    std::array::from_fn(|i| differences[i] + blue_coefficients[i]);
                            }
                            let values =
                                reconstruct(predictor[source + channel], parity, coefficients);
                            for (index, destination) in destinations.iter().enumerate() {
                                pixels[destination * channels + channel] = values[index];
                            }
                        }
                    }
                }
            }
            output[mip][face] = pixels;
        }
    }
    Ok(output)
}
