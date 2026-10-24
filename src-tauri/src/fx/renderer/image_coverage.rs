//! Read-only image alpha facts for camera ranking, not material replacement.
//!
//! A transparent placeholder must not make an otherwise culled effect look
//! visible to the camera heuristic. Inspect original compressed texels without
//! extracting files, changing draws or making assumptions from image names.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use super::{image_sources, normalized_member, ArchiveStamp, ImageCache, ImageSource, Result};

const MAX_SUMMARIES: usize = 128;

#[derive(Clone)]
struct Summary {
    member: String,
    archive: PathBuf,
    stamp: ArchiveStamp,
    zero: Option<bool>,
}

#[derive(Default)]
struct Cache {
    game: PathBuf,
    archives: Vec<(PathBuf, ArchiveStamp)>,
    sources: HashMap<String, ImageSource>,
    images: ImageCache,
    summaries: VecDeque<Summary>,
}

// CPU preparation workers share one bounded cache, rather than multiplying
// retained image bytes across the blocking pool. This contains only ordinary
// file readers/bytes; the UI's renderer and D3D resources remain independent.
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

impl Cache {
    fn refresh(&mut self, game: &Path) -> Result<()> {
        let mut archives = Vec::new();
        for entry in fs::read_dir(game.join("main"))? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("iwd"))
            {
                archives.push((path.clone(), ArchiveStamp::read(&path)?));
            }
        }
        archives.sort_by(|left, right| left.0.cmp(&right.0));
        if self.game == game && self.archives == archives {
            return Ok(());
        }
        let mut paths = archives
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let sources = image_sources(&mut paths)?;
        self.game = game.to_owned();
        self.archives = archives;
        self.sources = sources;
        self.images = ImageCache::default();
        self.summaries.clear();
        Ok(())
    }

    fn alpha_zero(&mut self, image: &str) -> Result<Option<Summary>> {
        let member = normalized_member(&format!("images/{image}.iwi"));
        let Some(source) = self.sources.get(&member).cloned() else {
            // Normal rendering reports missing assets. An optional camera
            // hint must not manufacture an opaque or transparent replacement.
            return Ok(None);
        };
        let stamp = ArchiveStamp::read(&source.archive)?;
        if let Some(index) = self.summaries.iter().position(|summary| {
            summary.member == member && summary.archive == source.archive && summary.stamp == stamp
        }) {
            let summary = self
                .summaries
                .remove(index)
                .ok_or("Image summary disappeared")?;
            self.summaries.push_back(summary.clone());
            return Ok(Some(summary));
        }
        let bytes = self.images.read(&source)?;
        let summary = Summary {
            member,
            archive: source.archive,
            stamp,
            zero: alpha_zero(&bytes),
        };
        self.summaries.push_back(summary.clone());
        while self.summaries.len() > MAX_SUMMARIES {
            self.summaries.pop_front();
        }
        Ok(Some(summary))
    }
}

/// Attach facts only. The caller must still honour the material's blend and
/// shader semantics: zero texture alpha does not make every opaque pass blank.
pub(crate) fn annotate(game: &Path, materials: &mut Value) -> Result<()> {
    let Some(materials) = materials.as_object_mut() else {
        return Err("Expected a material map for image coverage".into());
    };
    {
        let mut cache = CACHE
            .get_or_init(|| Mutex::new(Cache::default()))
            .lock()
            .map_err(|_| "Image coverage cache lock was poisoned")?;
        cache.refresh(game)?;
        for material in materials.values_mut() {
            let Some(image) = material["colorMap"].as_str() else {
                continue;
            };
            let Some(summary) = cache.alpha_zero(image)? else {
                continue;
            };
            let Some(zero) = summary.zero else {
                continue;
            };
            material["previewImageAlphaZero"] = json!(zero);
            material["previewImageCoverageSource"] = json!({
                "archive": summary.archive.file_name().unwrap_or_default().to_string_lossy(),
                "member": summary.member,
                "scope": "All original 2D mip texels; alpha facts only"
            });
        }
        Ok(())
    }
}

/// Unknown or malformed input returns None, never an invented coverage fact.
/// All mips are inspected because minification can sample a different alpha
/// pattern from the largest image. Padding texels outside tiny mips do not count.
fn alpha_zero(bytes: &[u8]) -> Option<bool> {
    if bytes.len() < 28 || bytes[..4] != *b"IWi\x06" || bytes[5] & 12 != 0 {
        return None;
    }
    let width = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as usize;
    let height = u16::from_le_bytes(bytes[8..10].try_into().ok()?) as usize;
    let depth = u16::from_le_bytes(bytes[10..12].try_into().ok()?);
    if width == 0 || height == 0 || width > 8192 || height > 8192 || depth != 1 {
        return None;
    }
    match bytes[4] {
        // The native X8R8G8B8 and L8 uploads supply alpha=1. For wavelet L8
        // validate through the same decoder as rendering, rather than assuming
        // that a header alone establishes a valid payload.
        2 => {
            let levels = dimensions(width, height, bytes[5] & 2 != 0);
            let length = levels
                .iter()
                .map(|(width, height)| width * height * 3)
                .sum::<usize>();
            (length + 28 == bytes.len()).then_some(false)
        }
        9 => super::wavelet_luminance_levels(bytes).ok().map(|_| false),
        11..=13 => compressed_alpha_zero(bytes, width, height),
        _ => None,
    }
}

fn dimensions(mut width: usize, mut height: usize, single: bool) -> Vec<(usize, usize)> {
    let mut levels = Vec::new();
    loop {
        levels.push((width, height));
        if single || (width == 1 && height == 1) {
            return levels;
        }
        width = (width / 2).max(1);
        height = (height / 2).max(1);
    }
}

fn compressed_alpha_zero(bytes: &[u8], width: usize, height: usize) -> Option<bool> {
    let format = bytes[4];
    let block_size = if format == 11 { 8 } else { 16 };
    let levels = dimensions(width, height, bytes[5] & 2 != 0);
    let total = levels
        .iter()
        .map(|(width, height)| width.div_ceil(4) * height.div_ceil(4) * block_size)
        .sum::<usize>();
    if total + 28 != bytes.len() {
        return None;
    }
    let largest_size = width.div_ceil(4) * height.div_ceil(4) * block_size;
    let start = if bytes[5] & 2 != 0 {
        28
    } else {
        u32::from_le_bytes(bytes[16..20].try_into().ok()?) as usize
    };
    if start != bytes.len() - largest_size {
        return None;
    }
    let mut end = bytes.len();
    let mut all_zero = true;
    for (width, height) in levels {
        let columns = width.div_ceil(4);
        let rows = height.div_ceil(4);
        let begin = end.checked_sub(columns * rows * block_size)?;
        let mip = &bytes[begin..end];
        for block_y in 0..rows {
            for block_x in 0..columns {
                let offset = (block_y * columns + block_x) * block_size;
                let block = &mip[offset..offset + block_size];
                for y in 0..4.min(height - block_y * 4) {
                    for x in 0..4.min(width - block_x * 4) {
                        if block_alpha(format, block, y * 4 + x) != 0 {
                            all_zero = false;
                        }
                    }
                }
            }
        }
        end = begin;
    }
    (end == 28).then_some(all_zero)
}

// Only zero/nonzero is needed. Keep interpolated DXT5 numerators unrounded:
// a sub-byte positive value is conservatively nonzero even if a particular
// GPU quantizes it to zero. Never discard a potentially visible texel.
fn block_alpha(format: u8, block: &[u8], pixel: usize) -> u16 {
    match format {
        11 => {
            let first = u16::from_le_bytes([block[0], block[1]]);
            let second = u16::from_le_bytes([block[2], block[3]]);
            let selectors = u32::from_le_bytes(block[4..8].try_into().unwrap());
            if first <= second && (selectors >> (pixel * 2)) & 3 == 3 {
                0
            } else {
                255
            }
        }
        12 => u16::from((block[pixel / 2] >> ((pixel % 2) * 4)) & 15) * 17,
        13 => {
            let mut table = [0_u16; 8];
            table[0] = u16::from(block[0]);
            table[1] = u16::from(block[1]);
            if table[0] > table[1] {
                for index in 2..8 {
                    table[index] = (8 - index) as u16 * table[0] + (index - 1) as u16 * table[1];
                }
            } else {
                for index in 2..6 {
                    table[index] = (6 - index) as u16 * table[0] + (index - 1) as u16 * table[1];
                }
                table[7] = 255;
            }
            let mut encoded = [0_u8; 8];
            encoded[..6].copy_from_slice(&block[2..8]);
            table[((u64::from_le_bytes(encoded) >> (pixel * 3)) & 7) as usize]
        }
        _ => 255,
    }
}
