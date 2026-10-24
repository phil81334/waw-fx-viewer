//! Read-only Modtools FX catalogue and disposable, bounded preview cache.
//!
//! Preparation checks supported whole graphs. It does not certify GPU output;
//! thumbnail rendering remains the final admission step on the render thread.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::UNIX_EPOCH,
};

const CACHE_VERSION: &str = "fx-catalogue-8";
const MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_THUMBNAIL_BYTES: usize = 2 * 1024 * 1024;
const MAX_CACHE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CACHE_FILES: usize = 8192;
const MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const MAX_SOURCE_ENTRIES: usize = 32;
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Serialize, Deserialize)]
pub struct CatalogueEntry {
    pub id: String,
    pub name: String,
    pub group: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Exclusion {
    pub name: String,
    pub reason: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct CatalogueIndex {
    pub entries: Vec<CatalogueEntry>,
    pub found: usize,
    pub supported: usize,
    pub excluded: usize,
    pub errors: Vec<Exclusion>,
}

#[derive(Serialize)]
pub struct CacheInfo {
    pub path: String,
    pub bytes: u64,
    pub files: usize,
}

#[derive(Serialize, Deserialize)]
struct SavedIndex {
    fingerprint: String,
    index: CatalogueIndex,
}

#[derive(Clone, Serialize, Deserialize)]
struct PreviewFailure {
    id: String,
    reason: String,
}

#[derive(Default)]
struct SourceCache {
    entries: VecDeque<(String, Value, usize)>,
    bytes: usize,
}

pub struct Catalogue {
    game: PathBuf,
    cache: PathBuf,
    names: HashMap<String, String>,
    cache_budget: Mutex<Option<(u64, usize)>>,
    source_cache: Mutex<SourceCache>,
    preview_failures: Mutex<HashMap<String, String>>,
    cache_write: Mutex<()>,
    generation: AtomicU64,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Windows junctions are reparse points even when is_symlink is false.
fn redirected(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

fn safe_cache_folder(path: &Path) -> Result<(), String> {
    // Check all existing parents too: an unlinked child beneath a junction is
    // still a redirected destination. Do not create/delete through that path.
    for parent in path.ancestors() {
        match fs::symlink_metadata(parent) {
            Ok(metadata) => {
                if redirected(&metadata) || !metadata.is_dir() {
                    return Err("The preview cache folder is redirected or invalid.".into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn safe_file(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if redirected(&metadata) || !metadata.is_file() {
                return Err("Refusing to use a redirected cache file.".into());
            }
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn collect_files(directory: &Path, files: &mut Vec<(PathBuf, fs::Metadata)>) -> Result<(), String> {
    if !directory.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(directory).map_err(|error| error.to_string())?;
    if redirected(&metadata) {
        return Err(format!(
            "Cannot index a redirected asset folder: {}",
            directory.display()
        ));
    }
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let metadata = entry.metadata().map_err(|error| error.to_string())?;
        if redirected(&metadata)
            || entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_symlink()
        {
            return Err(format!(
                "Cannot index a redirected asset: {}",
                entry.path().display()
            ));
        }
        if metadata.is_dir() {
            collect_files(&entry.path(), files)?;
        } else if metadata.is_file() {
            files.push((entry.path(), metadata));
        }
    }
    Ok(())
}

fn png_name(name: &str) -> bool {
    let Some(id) = name.strip_suffix(".png") else {
        return false;
    };
    id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_thumbnail(bytes: &[u8]) -> bool {
    // A partial cache write must be regenerated, not returned forever as a
    // successful PNG whose browser decode fails on every retry.
    if bytes.len() < 24
        || bytes.len() > MAX_THUMBNAIL_BYTES
        || !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes[16..20] != 192_u32.to_be_bytes()
        || bytes[20..24] != 192_u32.to_be_bytes()
    {
        return false;
    }
    image::load_from_memory_with_format(bytes, image::ImageFormat::Png).is_ok()
}

impl Catalogue {
    pub fn new(game: PathBuf, cache: PathBuf) -> Self {
        Self {
            game,
            cache,
            names: HashMap::new(),
            cache_budget: Mutex::new(None),
            source_cache: Mutex::new(SourceCache::default()),
            preview_failures: Mutex::new(HashMap::new()),
            cache_write: Mutex::new(()),
            generation: AtomicU64::new(NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)),
        }
    }

    fn ensure_cache(&self) -> Result<(), String> {
        safe_cache_folder(&self.cache)?;
        safe_cache_folder(&self.cache.join("thumbnails"))?;
        fs::create_dir_all(self.cache.join("thumbnails")).map_err(|error| error.to_string())
    }

    fn install_index(&mut self, index: &CatalogueIndex) {
        self.names = index
            .entries
            .iter()
            .map(|entry| (entry.id.clone(), entry.name.clone()))
            .collect();
    }

    pub fn scan(
        &mut self,
        force: bool,
        progress: impl Fn(Value),
    ) -> Result<CatalogueIndex, String> {
        if !self.game.join("raw/fx").is_dir() {
            return Err(
                "Install the WaW Modtools first. The raw/fx folder is missing. Then click Rescan."
                    .into(),
            );
        }
        self.ensure_cache()?;
        if force {
            self.clear()?;
        }
        let scan_generation = self.generation();
        progress(json!({"phase":"files","completed":0,"total":0}));
        let mut files = Vec::new();
        // Include all files used by graph/material/model resolution. A changed
        // technique or child EFX invalidates its parents' saved previews too.
        for folder in [
            "fx",
            "materials",
            "techsets",
            "techniques",
            "statemaps",
            "shader_bin",
            "xmodel",
            "xmodelsurfs",
            "xmodelparts",
        ] {
            collect_files(&self.game.join("raw").join(folder), &mut files)?;
        }
        let main = self.game.join("main");
        if main.is_dir() {
            for entry in fs::read_dir(&main).map_err(|error| error.to_string())? {
                let entry = entry.map_err(|error| error.to_string())?;
                if entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("iwd"))
                {
                    let metadata = entry.metadata().map_err(|error| error.to_string())?;
                    if !metadata.is_file() || redirected(&metadata) {
                        return Err("An IWD image archive is redirected or invalid.".into());
                    }
                    files.push((entry.path(), metadata));
                }
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let mut fingerprint = Sha256::new();
        fingerprint.update(CACHE_VERSION.as_bytes());
        fingerprint.update(self.game.to_string_lossy().as_bytes());
        let mut names = Vec::new();
        let fx_root = self.game.join("raw/fx");
        for (path, metadata) in &files {
            let modified = metadata
                .modified()
                .map_err(|error| error.to_string())?
                .duration_since(UNIX_EPOCH)
                .map_err(|error| error.to_string())?
                .as_nanos();
            fingerprint
                .update(format!("|{}:{}:{modified}", path.display(), metadata.len()).as_bytes());
            if let Ok(relative) = path.strip_prefix(&fx_root) {
                if relative
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("efx"))
                {
                    let name = relative
                        .with_extension("")
                        .to_string_lossy()
                        .replace('\\', "/");
                    names.push(name);
                }
            }
        }
        names.sort_by_key(|name| name.to_lowercase());
        let fingerprint = format!("{:x}", fingerprint.finalize());
        let index_path = self.cache.join("index.json");
        let mut previous_candidates = HashMap::new();
        if !force {
            if let Some(metadata) = safe_file(&index_path)? {
                if metadata.len() <= MAX_INDEX_BYTES {
                    if let Ok(bytes) = fs::read(&index_path) {
                        if let Ok(saved) = serde_json::from_slice::<SavedIndex>(&bytes) {
                            let known: HashSet<_> = names.iter().map(String::as_str).collect();
                            let unique: HashSet<_> = saved
                                .index
                                .entries
                                .iter()
                                .map(|entry| entry.name.as_str())
                                .collect();
                            let valid = saved.fingerprint == fingerprint
                                && saved.index.found == names.len()
                                && saved.index.supported == saved.index.entries.len()
                                && saved.index.found
                                    == saved.index.supported + saved.index.excluded
                                && unique.len() == saved.index.entries.len()
                                && saved.index.entries.iter().all(|entry| {
                                    known.contains(entry.name.as_str())
                                        && entry.id
                                            == hash(
                                                format!("{fingerprint}|{}", entry.name).as_bytes(),
                                            )
                                });
                            if valid {
                                // Only successful preparation is reusable. A denied
                                // read or temporary sharing failure must not become
                                // a permanent exclusion in the saved catalogue.
                                previous_candidates = saved
                                    .index
                                    .entries
                                    .into_iter()
                                    .map(|entry| (entry.name.clone(), entry))
                                    .collect();
                                progress(
                                    json!({"phase":"cache","completed":previous_candidates.len(),"total":names.len()}),
                                );
                            }
                        }
                    }
                }
            }
        }
        let mut index = CatalogueIndex {
            found: names.len(),
            ..CatalogueIndex::default()
        };
        for (position, name) in names.iter().enumerate() {
            if let Some(entry) = previous_candidates.remove(name) {
                index.entries.push(entry);
            } else {
                match crate::fx::prepare::preflight(&self.game, name) {
                    Ok(_) => {
                        index.entries.push(CatalogueEntry {
                            id: hash(format!("{fingerprint}|{name}").as_bytes()),
                            name: name.clone(),
                            group: name.split('/').next().unwrap_or("Other").to_owned(),
                        });
                    }
                    Err(reason) => index.errors.push(Exclusion {
                        name: name.clone(),
                        reason,
                    }),
                }
            }
            if position % 16 == 0 || position + 1 == names.len() {
                progress(json!({"phase":"prepare","completed":position+1,"total":names.len()}));
            }
        }
        index.supported = index.entries.len();
        index.excluded = index.errors.len();
        let saved = SavedIndex {
            fingerprint,
            index: index.clone(),
        };
        let bytes = serde_json::to_vec(&saved).map_err(|error| error.to_string())?;
        let _write = self.cache_write.lock().map_err(|error| error.to_string())?;
        if scan_generation != self.generation() {
            return Err("A newer cache operation replaced this catalogue scan.".into());
        }
        self.ensure_cache()?;
        let temporary = self.cache.join("index.tmp");
        safe_file(&temporary)?;
        fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
        // Windows cannot rename over an existing destination. Remove only this
        // app's checked index; a failed publication leaves a disposable cache.
        if safe_file(&index_path)?.is_some() {
            fs::remove_file(&index_path).map_err(|error| error.to_string())?;
        }
        fs::rename(&temporary, &index_path).map_err(|error| error.to_string())?;
        drop(_write);
        self.install_index(&index);
        self.restore_preview_failures()?;
        let failures = self
            .preview_failures
            .lock()
            .map_err(|error| error.to_string())?;
        index.entries.retain(|entry| {
            if let Some(reason) = failures.get(&entry.id) {
                index.errors.push(Exclusion {
                    name: entry.name.clone(),
                    reason: reason.clone(),
                });
                false
            } else {
                true
            }
        });
        index.supported = index.entries.len();
        index.excluded = index.errors.len();
        Ok(index)
    }

    fn restore_preview_failures(&self) -> Result<(), String> {
        let _write = self.cache_write.lock().map_err(|error| error.to_string())?;
        let path = self.cache.join("preview-errors.json");
        let saved = match safe_file(&path)? {
            Some(metadata) if metadata.len() <= MAX_INDEX_BYTES => fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Vec<PreviewFailure>>(&bytes).ok())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let failures = saved
            .into_iter()
            .filter(|failure| self.names.contains_key(&failure.id))
            .map(|failure| (failure.id, failure.reason))
            .collect();
        *self
            .preview_failures
            .lock()
            .map_err(|error| error.to_string())? = failures;
        Ok(())
    }

    /// Remember proved unsupported previews for this exact asset fingerprint.
    /// Graphics-device/IO interruptions are never persisted as asset failures.
    /// Rescan clears these records and retries the installed definitions.
    pub fn record_preview_failure(
        &self,
        id: &str,
        reason: &str,
        generation: u64,
    ) -> Result<(), String> {
        let _write = self.cache_write.lock().map_err(|error| error.to_string())?;
        if generation != self.generation() || !self.names.contains_key(id) {
            return Ok(());
        }
        self.ensure_cache()?;
        let path = self.cache.join("preview-errors.json");
        safe_file(&path)?;
        let mut failures = self
            .preview_failures
            .lock()
            .map_err(|error| error.to_string())?;
        let reason = reason.strip_prefix("unsupported:").unwrap_or(reason).trim();
        failures.insert(id.to_owned(), reason.chars().take(2048).collect());
        let saved = failures
            .iter()
            .map(|(id, reason)| PreviewFailure {
                id: id.clone(),
                reason: reason.clone(),
            })
            .collect::<Vec<_>>();
        fs::write(
            path,
            serde_json::to_vec(&saved).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())
    }

    /// Root commands capture this token before scheduling CPU/GPU work. A
    /// result from before Clear cache must not write into the new generation.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn source(&self, id: &str) -> Result<Value, String> {
        if let Some(reason) = self
            .preview_failures
            .lock()
            .map_err(|error| error.to_string())?
            .get(id)
        {
            return Err(format!("unsupported: {reason}"));
        }
        let name = self
            .names
            .get(id)
            .ok_or("Effect is no longer in the catalogue. Click Rescan.")?;
        let generation = self.generation();
        let cached = {
            let mut cache = self
                .source_cache
                .lock()
                .map_err(|error| error.to_string())?;
            if let Some(position) = cache.entries.iter().position(|(key, _, _)| key == id) {
                let entry = cache
                    .entries
                    .remove(position)
                    .ok_or("Source cache entry disappeared")?;
                let value = entry.1.clone();
                cache.entries.push_back(entry);
                Some(value)
            } else {
                None
            }
        };
        if let Some(value) = cached {
            if self.source_unchanged(&value) && generation == self.generation() {
                return Ok(value);
            }
        }
        // Do not retain failed reads. Nearby thumbnail/selection requests may
        // share a resolved graph, but cache hits recheck its exact dependency stamps.
        let value = crate::fx::prepare::preflight(&self.game, name)?;
        let cost = crate::fx::memory_cost(&value);
        let mut cache = self
            .source_cache
            .lock()
            .map_err(|error| error.to_string())?;
        if generation != self.generation() {
            return Err("A newer cache operation replaced this request. Try again.".into());
        }
        if let Some(position) = cache.entries.iter().position(|(key, _, _)| key == id) {
            if let Some((_, _, old_cost)) = cache.entries.remove(position) {
                cache.bytes = cache.bytes.saturating_sub(old_cost);
            }
        }
        if cost <= MAX_SOURCE_BYTES {
            cache.bytes = cache.bytes.saturating_add(cost);
            cache
                .entries
                .push_back((id.to_owned(), value.clone(), cost));
            while cache.bytes > MAX_SOURCE_BYTES || cache.entries.len() > MAX_SOURCE_ENTRIES {
                if let Some((_, _, old_cost)) = cache.entries.pop_front() {
                    cache.bytes = cache.bytes.saturating_sub(old_cost);
                }
            }
        }
        Ok(value)
    }

    fn source_unchanged(&self, value: &Value) -> bool {
        let Some(files) = value["dependencyFiles"].as_array() else {
            return false;
        };
        if files.is_empty() {
            return false;
        }
        // Every installed IWD affects image-source precedence. Detect newly
        // added/removed archives as well as changed files already in the graph.
        let expected_archives: HashSet<_> = files
            .iter()
            .filter_map(|file| {
                let path = file["path"].as_str()?;
                if path.to_lowercase().starts_with("main/") && path.to_lowercase().ends_with(".iwd")
                {
                    Some(path.to_lowercase())
                } else {
                    None
                }
            })
            .collect();
        let Ok(directory) = fs::read_dir(self.game.join("main")) else {
            return false;
        };
        let mut current_archives = HashSet::new();
        for entry in directory {
            let Ok(entry) = entry else {
                return false;
            };
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if name.ends_with(".iwd") {
                current_archives.insert(format!("main/{name}"));
            }
        }
        if expected_archives != current_archives {
            return false;
        }
        files.iter().all(|file| {
            let Some(name) = file["path"].as_str() else {
                return false;
            };
            let relative = Path::new(name);
            if relative
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                return false;
            }
            let Ok(metadata) = fs::metadata(self.game.join(relative)) else {
                return false;
            };
            let Ok(modified) = metadata.modified() else {
                return false;
            };
            let Ok(modified) = modified.duration_since(UNIX_EPOCH) else {
                return false;
            };
            file["size"].as_u64() == Some(metadata.len())
                && file["modified"].as_str() == Some(modified.as_nanos().to_string().as_str())
        })
    }

    pub fn cached_thumbnail(&self, id: &str) -> Option<String> {
        self.cached_thumbnail_for_background(id, "dark")
    }

    pub fn cached_thumbnail_for_background(&self, id: &str, preset: &str) -> Option<String> {
        let preset = crate::fx::renderer::background::Preset::parse(preset).ok()?;
        let _write = self.cache_write.lock().ok()?;
        self.names.get(id)?;
        self.ensure_cache().ok()?;
        let key = hash(format!("background-v1|{id}|{}", preset.name()).as_bytes());
        let path = self.cache.join("thumbnails").join(format!("{key}.png"));
        let metadata = safe_file(&path).ok()??;
        if metadata.len() > MAX_THUMBNAIL_BYTES as u64 {
            return None;
        }
        let bytes = fs::read(path).ok()?;
        if !valid_thumbnail(&bytes) {
            return None;
        }
        Some(format!("data:image/png;base64,{}", STANDARD.encode(bytes)))
    }

    pub fn save_thumbnail(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
        self.save_thumbnail_for_generation(id, bytes, self.generation())
    }

    pub fn save_thumbnail_for_generation(
        &self,
        id: &str,
        bytes: &[u8],
        generation: u64,
    ) -> Result<(), String> {
        self.save_thumbnail_for_background(id, bytes, generation, "dark")
    }

    pub fn save_thumbnail_for_background(
        &self,
        id: &str,
        bytes: &[u8],
        generation: u64,
        preset: &str,
    ) -> Result<(), String> {
        let preset = crate::fx::renderer::background::Preset::parse(preset)?;
        let _write = self.cache_write.lock().map_err(|error| error.to_string())?;
        if generation != self.generation() {
            return Err("A newer cache operation replaced this thumbnail.".into());
        }
        if !self.names.contains_key(id) {
            return Err("Effect is no longer in the catalogue. Click Rescan.".into());
        }
        if !valid_thumbnail(bytes) {
            return Err("Invalid thumbnail PNG.".into());
        }
        self.ensure_cache()?;
        let key = hash(format!("background-v1|{id}|{}", preset.name()).as_bytes());
        let path = self.cache.join("thumbnails").join(format!("{key}.png"));
        let previous_size = safe_file(&path)?.map(|metadata| metadata.len());
        fs::write(path, bytes).map_err(|error| error.to_string())?;
        self.trim_cache(&key, previous_size, bytes.len() as u64)
    }

    fn thumbnail_files(&self) -> Result<Vec<(PathBuf, fs::Metadata)>, String> {
        safe_cache_folder(&self.cache)?;
        let folder = self.cache.join("thumbnails");
        safe_cache_folder(&folder)?;
        if !folder.is_dir() {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(folder).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            if png_name(&entry.file_name().to_string_lossy()) {
                if let Some(metadata) = safe_file(&entry.path())? {
                    files.push((entry.path(), metadata));
                }
            }
        }
        Ok(files)
    }

    fn trim_cache(
        &self,
        keep_id: &str,
        previous_size: Option<u64>,
        saved_size: u64,
    ) -> Result<(), String> {
        let mut budget = self
            .cache_budget
            .lock()
            .map_err(|error| error.to_string())?;
        let (mut bytes, mut count) = if let Some((bytes, count)) = *budget {
            (
                bytes.saturating_sub(previous_size.unwrap_or(0)) + saved_size,
                count + usize::from(previous_size.is_none()),
            )
        } else {
            // Count once per catalogue, not once for every generated thumbnail.
            // This first snapshot already contains the newly saved PNG.
            let files = self.thumbnail_files()?;
            (
                files.iter().map(|(_, metadata)| metadata.len()).sum(),
                files.len(),
            )
        };
        if bytes <= MAX_CACHE_BYTES && count <= MAX_CACHE_FILES {
            *budget = Some((bytes, count));
            return Ok(());
        }
        let mut files = self.thumbnail_files()?;
        bytes = files.iter().map(|(_, metadata)| metadata.len()).sum();
        count = files.len();
        files.sort_by_key(|(_, metadata)| metadata.modified().unwrap_or(UNIX_EPOCH));
        let keep = format!("{keep_id}.png");
        for (path, metadata) in files {
            if bytes <= MAX_CACHE_BYTES && count <= MAX_CACHE_FILES {
                break;
            }
            if path.file_name().is_some_and(|name| name == keep.as_str()) {
                continue;
            }
            fs::remove_file(path).map_err(|error| error.to_string())?;
            bytes = bytes.saturating_sub(metadata.len());
            count = count.saturating_sub(1);
        }
        *budget = Some((bytes, count));
        Ok(())
    }

    pub fn cache_info(&self) -> Result<CacheInfo, String> {
        let _write = self.cache_write.lock().map_err(|error| error.to_string())?;
        let files = self.thumbnail_files()?;
        let mut bytes = files.iter().map(|(_, metadata)| metadata.len()).sum();
        let mut count = files.len();
        for name in ["index.json", "index.tmp", "preview-errors.json"] {
            if let Some(metadata) = safe_file(&self.cache.join(name))? {
                bytes += metadata.len();
                count += 1;
            }
        }
        Ok(CacheInfo {
            path: self.cache.display().to_string(),
            bytes,
            files: count,
        })
    }

    pub fn clear(&self) -> Result<(), String> {
        let _write = self.cache_write.lock().map_err(|error| error.to_string())?;
        self.generation.store(
            NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            Ordering::Release,
        );
        *self
            .source_cache
            .lock()
            .map_err(|error| error.to_string())? = SourceCache::default();
        self.preview_failures
            .lock()
            .map_err(|error| error.to_string())?
            .clear();
        *self
            .cache_budget
            .lock()
            .map_err(|error| error.to_string())? = None;
        // Deliberately no recursive directory deletion: foreign files and
        // subdirectories are not ours, and junctions are never followed.
        for (path, _) in self.thumbnail_files()? {
            fs::remove_file(path).map_err(|error| error.to_string())?;
        }
        for name in ["index.json", "index.tmp", "preview-errors.json"] {
            let path = self.cache.join(name);
            if safe_file(&path)?.is_some() {
                fs::remove_file(path).map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }
}
