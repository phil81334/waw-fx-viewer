//! Read-only whole-effect preparation for the catalogue and native player.
//! Thumbnail sampling and camera fitting are separate from live playback.

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{camera, cloud_geometry, loader, model_assets, model_pose, renderer};

mod camera_fit;

type Result<T> = std::result::Result<T, String>;
pub(crate) const MAX_CLIP_BYTES: usize = 128 * 1024 * 1024;
const PREVIEW_SEED: u32 = 17;

pub(crate) struct PreparedClip {
    pub frames: Vec<Value>,
    pub duration: u32,
    pub camera: Value,
    pub name: String,
    pub looping: bool,
    pub representative: usize,
    pub thumbnail_candidates: Vec<usize>,
    pub fingerprint: String,
}

/// Read-only input shared by a selected effect's worker. A restart creates a
/// fresh simulation; ordinary playback keeps the existing simulation alive.
pub(crate) struct PreparedPlayback {
    pub source: Arc<Value>,
    pub camera: Value,
    pub name: String,
    pub duration: Option<u32>,
    pub continuous: bool,
    pub material_frames: Vec<Value>,
    pub projection: FrameProjection,
}

fn load_named(game: &Path, path: &Path) -> Result<Value> {
    let mut effect = loader::load(path)?;
    let relative = path
        .strip_prefix(game.join("raw/fx"))
        .map_err(|error| error.to_string())?;
    effect["name"] = json!(canonical_effect_name(&relative.to_string_lossy())?);
    Ok(effect)
}

/// Resolve all active visual dependencies and compile without advancing time.
/// A successful preflight is not a completed render certificate.
pub(crate) fn preflight(game: &Path, name: &str) -> Result<Value> {
    let name = canonical_effect_name(name)?;
    let mut effect = load_named(game, &game.join("raw/fx").join(&name).with_extension("efx"))?;
    effect["previewPackedLighting"] = json!(0x4210_u16);
    resolve_dependencies(game, &mut effect)?;
    let compiled = super::simulate(&effect, PREVIEW_SEED, &[])?;
    let has_visual = compiled["compiled"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(
            compiled["dependencyCompiled"]
                .as_object()
                .into_iter()
                .flat_map(|definitions| definitions.values())
                .filter_map(Value::as_array)
                .flatten(),
        )
        .any(|element| {
            element["kind"]
                .as_str()
                .is_some_and(|kind| !["sound", "decal", "runner"].contains(&kind))
        });
    if !has_visual {
        return Err("This effect has no visual elements to preview".into());
    }
    resolve_materials_with_model_reader(game, &mut effect, |game, name| {
        renderer::model_material_info(game, name).map_err(|error| error.to_string())
    })?;
    super::simulate(&effect, PREVIEW_SEED, &[])?;
    renderer::validate_material_manifest(
        &material_manifest(&effect)?,
        &effect["sharedMaterialInfo"],
    )
    .map_err(|error| error.to_string())?;
    if super::memory_cost(&effect) > MAX_CLIP_BYTES / 2 {
        return Err("This effect's assets exceed the preview memory limit".into());
    }
    attach_fingerprint(game, &mut effect)?;
    Ok(effect)
}

pub(crate) fn prepare(game: &Path, name: &str, duration_ms: u32) -> Result<PreparedClip> {
    prepare_from_source(game, preflight(game, name)?, duration_ms)
}

/// Prepare bounded samples for offline rendering checks. The desktop player
/// uses prepare_playback and retains its live simulation instead.
pub(crate) fn prepare_from_source(
    game: &Path,
    mut source: Value,
    duration_ms: u32,
) -> Result<PreparedClip> {
    annotate_camera_images(game, &mut source)?;
    prepare_segment(source, duration_ms, 30, false, &|| false)
}

/// A card needs a useful still image, not ten seconds of animation. Sample
/// early activity and actual particle midpoints before trying a longer pass.
/// The simulation clock remains one millisecond in both paths.
pub(crate) fn prepare_thumbnail(game: &Path, source: Value) -> Result<PreparedClip> {
    prepare_thumbnail_with_cancel(game, source, &|| false)
}

pub(crate) fn prepare_thumbnail_with_cancel(
    game: &Path,
    source: Value,
    cancelled: &dyn Fn() -> bool,
) -> Result<PreparedClip> {
    check_cancelled(cancelled)?;
    let mut source = source;
    annotate_camera_images(game, &mut source)?;
    configure_scene(&mut source);
    let lifecycle = super::simulation::probe_lifecycle(&source, PREVIEW_SEED, 2000)?;
    check_cancelled(cancelled)?;
    let mut times = BTreeSet::from([0, 16, 50, 100, 250, 500, 750, 1000]);
    for time in lifecycle
        .sample_times
        .iter()
        .copied()
        .filter(|time| *time <= 10000)
        .take(16)
    {
        times.insert(time);
    }
    if let Some(end) = lifecycle.completion_time {
        times.retain(|time| *time <= end);
        times.extend([end / 4, end / 2, end * 3 / 4, end]);
    }
    prepare_samples(
        source,
        times.into_iter().collect(),
        lifecycle.naturally_infinite,
        cancelled,
    )
}

/// Only cards whose first pass rendered no visible pixels need the longer
/// timeline. A graphics error is not a reason to repeatedly simulate an effect.
pub(crate) fn prepare_thumbnail_fallback(game: &Path, source: Value) -> Result<PreparedClip> {
    prepare_thumbnail_fallback_with_cancel(game, source, &|| false)
}

pub(crate) fn prepare_thumbnail_fallback_with_cancel(
    game: &Path,
    mut source: Value,
    cancelled: &dyn Fn() -> bool,
) -> Result<PreparedClip> {
    check_cancelled(cancelled)?;
    annotate_camera_images(game, &mut source)?;
    prepare_segment(source, 10000, 10, true, cancelled)
}

/// Find the authored finish with the real scheduler, then fit a useful camera.
/// The returned source is played by a retained worker, never replayed in chunks.
pub(crate) fn prepare_playback(game: &Path, source: Value) -> Result<PreparedPlayback> {
    prepare_playback_with_cancel(game, source, &|| false)
}

pub(crate) fn prepare_playback_with_cancel(
    game: &Path,
    mut source: Value,
    cancelled: &dyn Fn() -> bool,
) -> Result<PreparedPlayback> {
    check_cancelled(cancelled)?;
    annotate_camera_images(game, &mut source)?;
    configure_scene(&mut source);
    let mut lifecycle = super::simulation::probe_lifecycle(&source, PREVIEW_SEED, 60000)?;
    let mut horizon = playback_camera_horizon(&lifecycle);
    let mut camera = Value::Null;
    // Camera-dependent ordering can change child random draws. Measure the
    // final lifetime under the camera the live worker will actually use.
    for _ in 0..3 {
        check_cancelled(cancelled)?;
        let times = preview_times(horizon, &lifecycle.sample_times);
        camera = fit_streaming_camera(&mut source, PREVIEW_SEED, &times, cancelled)?;
        lifecycle = super::simulation::probe_lifecycle(&source, PREVIEW_SEED, 60000)?;
        let next_horizon = playback_camera_horizon(&lifecycle);
        if next_horizon <= horizon {
            break;
        }
        horizon = next_horizon;
    }
    check_cancelled(cancelled)?;
    if let Some(first) = delayed_activity(&lifecycle, 10000) {
        // This is scheduler activity, not a promise of the first visible GPU
        // pixel. Live playback still begins at time zero and keeps the delay.
        camera["delayedActivityMsec"] = json!(first);
    }
    let duration = lifecycle.completion_time;
    let name = source["name"]
        .as_str()
        .ok_or("Missing effect name")?
        .to_owned();
    let material_frames = material_manifest(&source)?;
    let projection = FrameProjection::new(&source["modelInfo"], &camera)?;
    Ok(PreparedPlayback {
        source: Arc::new(source),
        camera,
        name,
        duration,
        continuous: lifecycle.naturally_infinite,
        material_frames,
        projection,
    })
}

fn check_cancelled(cancelled: &dyn Fn() -> bool) -> Result<()> {
    if cancelled() {
        return Err("transient: A newer selection replaced this request".into());
    }
    Ok(())
}

/// Image coverage is an optional camera hint, read only when a preview needs
/// directional or distance-aware fitting. Catalogue scanning remains metadata
/// only, and the native renderer still loads every original material normally.
fn annotate_camera_images(game: &Path, source: &mut Value) -> Result<()> {
    if camera_fit::needs_alternatives(source) {
        renderer::annotate_image_coverage(game, &mut source["sharedMaterialInfo"])
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn preview_times(horizon: u32, visual_times: &[u32]) -> Vec<u32> {
    let mut times = BTreeSet::from([0, horizon]);
    for index in 0..=100 {
        times.insert((u64::from(horizon) * index / 100) as u32);
    }
    for &time in visual_times {
        if time <= horizon {
            times.insert(time);
        }
    }
    times.into_iter().collect()
}

/// A looping runner may have no child activity in the ordinary first ten
/// seconds. Its real scheduler already reports later birth/midpoint samples;
/// include that first burst in camera/thumbnail preparation without changing
/// the effect's playback clock, authored delay or lifetime.
fn delayed_activity(lifecycle: &super::simulation::LifecycleReport, ordinary: u32) -> Option<u32> {
    lifecycle
        .sample_times
        .iter()
        .copied()
        .min()
        .filter(|first| *first > ordinary)
}

fn initial_activity_horizon(lifecycle: &super::simulation::LifecycleReport, ordinary: u32) -> u32 {
    delayed_activity(lifecycle, ordinary)
        .map(|first| first.saturating_add(2000).min(60000))
        .unwrap_or(ordinary)
}

fn playback_camera_horizon(lifecycle: &super::simulation::LifecycleReport) -> u32 {
    lifecycle
        .completion_time
        .unwrap_or_else(|| initial_activity_horizon(lifecycle, 10000))
        .clamp(1, 60000)
}

/// Load every graph material before playing, including children that have not
/// spawned yet. These are asset names, not captured or invented draw geometry.
pub(crate) fn material_manifest(source: &Value) -> Result<Vec<Value>> {
    let mut draws = Vec::new();
    for definition in std::iter::once(source).chain(
        source["dependencies"]
            .as_object()
            .into_iter()
            .flat_map(|items| items.values()),
    ) {
        for element in definition["elements"]
            .as_array()
            .ok_or("Missing elements")?
        {
            if loader::words(element, "editorFlags")
                .iter()
                .any(|flag| flag == "disabled")
            {
                continue;
            }
            for field in [
                "billboardSprite",
                "orientedSprite",
                "tail",
                "line",
                "cloud",
                "trail",
            ] {
                for material in loader::words(element, field) {
                    if material.is_empty() || ["{", "}", ";"].contains(&material.as_str()) {
                        continue;
                    }
                    let format = if field == "cloud" {
                        "cloud20"
                    } else {
                        "sprite32"
                    };
                    draws.push(json!({"texture":material,"vertexFormat":format}));
                }
            }
        }
    }
    if let Some(models) = source["modelInfo"].as_object() {
        for model in models.values() {
            for surface in model["surfaces"]
                .as_array()
                .ok_or("Missing model surfaces")?
            {
                let material = surface["material"]
                    .as_str()
                    .ok_or("Missing model material")?;
                draws.push(
                    json!({"texture":format!("__model__/{material}"),"vertexFormat":"model32"}),
                );
            }
        }
    }
    Ok(vec![json!({"draws":draws})])
}

fn prepare_segment(
    mut source: Value,
    duration_ms: u32,
    samples_per_second: u32,
    include_delayed_activity: bool,
    cancelled: &dyn Fn() -> bool,
) -> Result<PreparedClip> {
    check_cancelled(cancelled)?;
    let requested_duration = duration_ms.clamp(1, 60000);
    let looping = std::iter::once(&source)
        .chain(
            source["dependencies"]
                .as_object()
                .into_iter()
                .flat_map(|items| items.values()),
        )
        .any(|effect| {
            effect["elements"].as_array().is_some_and(|elements| {
                elements.iter().any(|element| {
                    let flags = loader::words(element, "editorFlags");
                    flags.iter().any(|flag| flag == "looping")
                        && !flags.iter().any(|flag| flag == "disabled")
                })
            })
        });

    configure_scene(&mut source);
    let lifecycle = super::simulation::probe_lifecycle(&source, PREVIEW_SEED, 60000)?;
    check_cancelled(cancelled)?;
    let duration = lifecycle
        .completion_time
        .unwrap_or_else(|| {
            if include_delayed_activity {
                initial_activity_horizon(&lifecycle, requested_duration)
            } else {
                requested_duration
            }
        })
        .max(1);
    let mut times = (0..=(duration * samples_per_second / 1000))
        .map(|index| {
            ((u64::from(index) * 1000 + u64::from(samples_per_second / 2))
                / u64::from(samples_per_second)) as u32
        })
        .filter(|time| *time <= duration)
        .collect::<Vec<_>>();
    if times.last().copied() != Some(duration) {
        times.push(duration);
    }
    times.extend(
        lifecycle
            .sample_times
            .iter()
            .copied()
            .filter(|time| *time <= duration),
    );
    times.extend([duration / 4, duration / 2, duration * 3 / 4]);
    times.sort_unstable();
    times.dedup();

    prepare_samples(source, times, looping, cancelled)
}

fn prepare_samples(
    mut source: Value,
    times: Vec<u32>,
    looping: bool,
    cancelled: &dyn Fn() -> bool,
) -> Result<PreparedClip> {
    check_cancelled(cancelled)?;
    let name = source["name"]
        .as_str()
        .ok_or("Missing effect name")?
        .to_owned();
    let duration = times.last().copied().unwrap_or(0);
    let (mut playback, camera) = simulate_fitted(&mut source, PREVIEW_SEED, &times, cancelled)?;
    let frames = std::mem::take(&mut playback["frames"])
        .as_array_mut()
        .map(std::mem::take)
        .ok_or("Missing prepared frames")?;
    let mut retained_bytes = 0_usize;
    let mut frames = frames;
    for frame in &mut frames {
        check_cancelled(cancelled)?;
        super::compact_frame(frame);
        frame["background"] = json!(0xff191e23_u32);
        retained_bytes = retained_bytes.saturating_add(super::memory_cost(frame));
        if retained_bytes > MAX_CLIP_BYTES {
            return Err("This effect exceeds the 128 MiB preview memory limit".into());
        }
    }
    let thumbnail_candidates = thumbnail_candidates(&frames, &source["sharedMaterialInfo"])?;
    let representative = *thumbnail_candidates
        .first()
        .ok_or("No visual output appeared in this preview segment")?;
    let fingerprint = source["dependencyFingerprint"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    Ok(PreparedClip {
        frames,
        duration,
        camera,
        name,
        looping,
        representative,
        thumbnail_candidates,
        fingerprint,
    })
}

fn configure_scene(source: &mut Value) {
    // No diagnostic JSON or fixture overrides are exposed by the app. The
    // resolved source is internal, produced by preflight from actual EFX files.
    source["previewPackedLighting"] = json!(0x4210_u16);
    let has_lights = std::iter::once(&*source)
        .chain(
            source["dependencies"]
                .as_object()
                .into_iter()
                .flat_map(|items| items.values()),
        )
        .any(|effect| {
            effect["elements"].as_array().is_some_and(|elements| {
                elements.iter().any(|element| {
                    let disabled = loader::words(element, "editorFlags")
                        .iter()
                        .any(|flag| flag == "disabled");
                    !disabled
                        && (element.get("light").is_some() || element.get("spotLight").is_some())
                })
            })
        });
    if has_lights {
        // This receiver exists to make FX lights visible. A large plane must
        // not dominate the fitted bounds of unrelated tiny sparks or models.
        source["previewPlane"] = json!({"axis":"x", "height":-40.0, "size":256.0,
            "baseRgb":[0.04,0.04,0.04], "albedoRgb":[0.5,0.5,0.5]});
    } else if source["modelInfo"]
        .as_object()
        .is_some_and(|models| !models.is_empty())
        && source["sharedMaterialInfo"]
            .as_object()
            .is_some_and(|materials| {
                materials
                    .values()
                    .any(|material| material["depthFeather"].as_bool() == Some(true))
            })
    {
        // Soft particles still need the model float-Z prepass. This metadata
        // requests that prepass without a visible floor or extra fit bounds.
        source["previewPlane"] = json!({"axis":"x", "height":-40.0, "size":256.0,
            "baseRgb":[0.04,0.04,0.04], "albedoRgb":[0.5,0.5,0.5], "depthOnly":true});
    } else if let Some(fields) = source.as_object_mut() {
        fields.remove("previewPlane");
    }
    source["compactPreviewFrames"] = json!(true);
    source["previewFrameByteBudget"] = json!(MAX_CLIP_BYTES / 2);
}

/// A packed colour score cannot tell whether an atlas cell contains anything.
/// Offer several times to the GPU instead of trusting one high-scoring frame.
fn thumbnail_candidates(frames: &[Value], materials: &Value) -> Result<Vec<usize>> {
    let mut ranked = frames
        .iter()
        .enumerate()
        .map(|(index, frame)| representative_score(frame, materials).map(|score| (index, score)))
        .collect::<Result<Vec<_>>>()?;
    ranked.retain(|(_, score)| *score > 0);
    ranked.sort_by_key(|(index, score)| (std::cmp::Reverse(*score), *index));
    let mut candidates = Vec::new();
    if let Some((_, strongest)) = ranked.first() {
        let tied = ranked
            .iter()
            .take_while(|(_, score)| score == strongest)
            .collect::<Vec<_>>();
        candidates.push(tied[(tied.len() - 1) / 2].0);
    }
    // Time-spread candidates cover animated textures; the remaining ranked
    // frames catch brief bursts without imposing an arbitrary eight-frame cut.
    for slot in 0..16 {
        if !frames.is_empty() {
            let index = slot * (frames.len() - 1) / 15;
            if !candidates.contains(&index) {
                candidates.push(index);
            }
        }
    }
    for (index, _) in ranked {
        if !candidates.contains(&index) {
            candidates.push(index);
        }
    }
    Ok(candidates)
}

/// Prefer strong colour/opacity instead of the final equal-count fade frame.
/// This chooses a sample only. The native renderer still checks actual pixels,
/// since packed colours cannot establish an atlas tile's visible coverage.
fn representative_score(frame: &Value, materials: &Value) -> Result<u64> {
    let mut score = 0_u64;
    for draw in frame["draws"].as_array().ok_or("Missing prepared draws")? {
        let material = draw["texture"]
            .as_str()
            .ok_or("Missing prepared material")?;
        let additive = materials[material]["sourceBlend"].as_u64() == Some(2)
            && materials[material]["destinationBlend"].as_u64() == Some(2);
        if let Some(cloud) = draw.get("cloud") {
            let strength = if additive {
                cloud["bgra"]
                    .as_array()
                    .map(|colour| {
                        colour
                            .iter()
                            .take(3)
                            .filter_map(Value::as_u64)
                            .max()
                            .unwrap_or(0)
                    })
                    .unwrap_or(0)
            } else {
                cloud["bgra"][3].as_u64().unwrap_or(0)
            };
            score = score.saturating_add(strength);
            continue;
        }

        let bytes = STANDARD
            .decode(
                draw["vertices"]
                    .as_str()
                    .ok_or("Missing prepared vertices")?,
            )
            .map_err(|error| error.to_string())?;
        let mut strength = 0_u64;
        let mut count = 0_u64;
        for vertex in bytes.chunks_exact(32) {
            let colour = &vertex[16..20];
            strength += if additive {
                u64::from(*colour[..3].iter().max().unwrap_or(&0))
            } else {
                u64::from(colour[3])
            };
            count += 1;
        }
        if count > 0 {
            score = score.saturating_add(strength / count);
        }
    }
    if let Some(lights) = frame["lights"].as_array() {
        for light in lights {
            let radius = light["radius"].as_f64().unwrap_or(0.0).clamp(0.0, 2000.0);
            let colour = light["rgb"]
                .as_array()
                .map(|rgb| rgb.iter().filter_map(Value::as_f64).fold(0.0_f64, f64::max))
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            score = score.saturating_add((radius * colour * 255.0) as u64);
        }
    }
    Ok(score)
}

fn attach_fingerprint(game: &Path, effect: &mut Value) -> Result<()> {
    let mut paths = BTreeSet::<PathBuf>::new();
    paths.insert(
        game.join("raw/fx")
            .join(effect["name"].as_str().ok_or("Missing effect name")?)
            .with_extension("efx"),
    );
    if let Some(definitions) = effect["dependencies"].as_object() {
        for name in definitions.keys() {
            paths.insert(game.join("raw/fx").join(name).with_extension("efx"));
        }
    }
    if let Some(models) = effect["modelInfo"].as_object() {
        for (name, model) in models {
            paths.insert(model_assets::asset_path(game, "xmodel", name)?);
            let lod = model["lodName"].as_str().ok_or("Missing model LOD name")?;
            paths.insert(model_assets::asset_path(game, "xmodelsurfs", lod)?);
            paths.insert(model_assets::asset_path(game, "xmodelparts", lod)?);
            for surface in model["surfaces"]
                .as_array()
                .ok_or("Missing model surfaces")?
            {
                let name = surface["material"]
                    .as_str()
                    .ok_or("Missing model material")?;
                let info =
                    renderer::model_material_info(game, name).map_err(|error| error.to_string())?;
                material_paths(game, name, &info, true, &mut paths)?;
            }
        }
    }
    if let Some(materials) = effect["sharedMaterialInfo"].as_object() {
        for (name, info) in materials {
            // Model materials use mc_ techniques; their exact files were
            // recorded with the model above, not as sprite FX materials.
            if !name.starts_with("__model__/") {
                material_paths(game, name, info, false, &mut paths)?;
            }
        }
    }
    for entry in fs::read_dir(game.join("main")).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("iwd"))
        {
            paths.insert(path);
        }
    }
    let mut records = Vec::new();
    for path in paths {
        let metadata =
            fs::metadata(&path).map_err(|error| format!("Read {}: {error}", path.display()))?;
        let modified = metadata
            .modified()
            .map_err(|error| error.to_string())?
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos()
            .to_string();
        records.push(
            json!({"path":path.strip_prefix(game).map_err(|error|error.to_string())?
            .to_string_lossy().replace('\\',"/"),"size":metadata.len(),"modified":modified}),
        );
    }
    let bytes = serde_json::to_vec(&records).map_err(|error| error.to_string())?;
    effect["dependencyFingerprint"] = json!(format!("{:x}", Sha256::digest(bytes)));
    effect["dependencyFiles"] = json!(records);
    Ok(())
}

fn material_paths(
    game: &Path,
    name: &str,
    info: &Value,
    model: bool,
    paths: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let name = name.strip_prefix("__model__/").unwrap_or(name);
    paths.insert(game.join("raw/materials").join(name));
    let family = info["family"].as_str().ok_or("Missing material family")?;
    let prefix = if model { "mc_" } else { "" };
    paths.insert(
        game.join("raw/techsets")
            .join(format!("{prefix}{family}.techset")),
    );
    for field in ["pass", "authoredPass", "effectivePass"] {
        if let Some(pass) = info[field].as_str() {
            paths.insert(game.join("raw/techniques").join(format!("{pass}.tech")));
        }
    }
    if let Some(map) = info["stateMap"].as_str() {
        paths.insert(game.join("raw/statemaps").join(format!("{map}.sm")));
    }
    for field in ["vertexProgram", "pixelProgram"] {
        if let Some(program) = info[field].as_str() {
            paths.insert(game.join("raw/shader_bin").join(program));
        }
    }
    Ok(())
}

fn canonical_effect_name(value: &str) -> Result<String> {
    let mut value = value.trim_matches('"').replace('\\', "/");
    if value.to_ascii_lowercase().ends_with(".efx") {
        value.truncate(value.len() - 4);
    }
    if value.is_empty()
        || value.starts_with('/')
        || value.contains(':')
        || value.split('/').any(|part| part == ".." || part.is_empty())
    {
        return Err(format!("Invalid child effect name: {value}").into());
    }
    Ok(value.to_ascii_lowercase())
}

fn child_names(effect: &Value) -> Result<Vec<String>> {
    let mut names = BTreeSet::new();
    for element in effect["elements"].as_array().ok_or("Missing elements")? {
        if loader::words(element, "editorFlags")
            .iter()
            .any(|word| word == "disabled")
        {
            continue;
        }
        let editor_flags = loader::words(element, "editorFlags");
        // Native preview spawn exits before consulting these types' children.
        if element.get("decal").is_some() || element.get("sound").is_some() {
            continue;
        }
        // Native conversion nulls these authored references when their editor
        // flag is disabled. A dormant name must not demand files or materials.
        let references = [
            ("runner", true),
            (
                "fxOnDeath",
                editor_flags.iter().any(|flag| flag == "playOnDeath"),
            ),
            // The fixed empty-scene preview uses EffectsEd's original no-hit
            // trace policy. Preserve authored impact metadata in the compiler,
            // but do not load assets reachable only through a collision.
            ("fxOnImpact", false),
            (
                "emission",
                editor_flags.iter().any(|flag| flag == "playOnRun"),
            ),
        ];
        for (field, enabled) in references {
            if !enabled {
                continue;
            }
            for word in loader::words(element, field) {
                if !word.is_empty() && !["{", "}", ";"].contains(&word.as_str()) {
                    names.insert(canonical_effect_name(&word)?);
                }
            }
        }
    }
    Ok(names.into_iter().collect())
}

/// Load a finite definition registry without recursively embedding JSON trees.
/// A visited set makes cyclic references finite to read; runtime expansion must
/// still enforce its separate recursion/particle/effect limits.
fn resolve_dependencies(game: &Path, effect: &mut Value) -> Result<()> {
    let root = canonical_effect_name(effect["name"].as_str().ok_or("Missing effect name")?)?;
    effect["name"] = json!(root);
    let mut visited = BTreeSet::from([root.clone()]);
    let mut queue: VecDeque<String> = child_names(effect)?.into();
    let mut definitions = serde_json::Map::new();
    while let Some(child) = queue.pop_front() {
        if !visited.insert(child.clone()) {
            continue;
        }
        if visited.len() > 512 {
            return Err("Effect dependency graph exceeds 512 definitions".into());
        }
        let path = game.join("raw/fx").join(&child).with_extension("efx");
        let mut definition = load_named(game, &path)
            .map_err(|error| format!("Dependency {child} of {root}: {error}"))?;
        definition["name"] = json!(child);
        queue.extend(child_names(&definition)?);
        definitions.insert(child, definition);
    }
    effect["dependencies"] = Value::Object(definitions);
    resolve_copy_sort_metadata(game, effect)?;
    Ok(())
}

/// Native conversion copies each distinct authored emission child's one-shot
/// class even when playOnRun is off. Those copies affect sort averages, but do
/// not start extra particles. Read their metadata without requiring render
/// assets for dormant children or recursively expanding inactive dependencies.
fn copied_sort_orders(game: &Path, effect: &Value) -> Result<Value> {
    let mut seen = BTreeSet::new();
    let mut orders = Vec::new();
    for element in effect["elements"].as_array().ok_or("Missing elements")? {
        if loader::words(element, "editorFlags")
            .iter()
            .any(|flag| flag == "disabled")
        {
            continue;
        }
        for reference in loader::words(element, "emission") {
            if reference.is_empty() {
                continue;
            }
            let name = canonical_effect_name(&reference)?;
            if !seen.insert(name.clone()) {
                continue;
            }
            let path = game.join("raw/fx").join(&name).with_extension("efx");
            let child = load_named(game, &path)
                .map_err(|error| format!("Emitted-class sort metadata {name}: {error}"))?;
            for copied in child["elements"]
                .as_array()
                .ok_or("Missing child elements")?
            {
                let flags = loader::words(copied, "editorFlags");
                if flags
                    .iter()
                    .any(|flag| flag == "disabled" || flag == "looping")
                    || copied.get("runner").is_some()
                {
                    continue;
                }
                let value = loader::words(copied, "sortOrder")
                    .first()
                    .map(|word| word.parse::<i64>())
                    .transpose()
                    .map_err(|error| error.to_string())?
                    .unwrap_or(5);
                orders.push(value.clamp(0, 255) as u8);
            }
        }
    }
    Ok(json!(orders))
}

fn resolve_copy_sort_metadata(game: &Path, effect: &mut Value) -> Result<()> {
    effect["copySortOrders"] = copied_sort_orders(game, effect)?;
    if let Some(definitions) = effect["dependencies"].as_object_mut() {
        for child in definitions.values_mut() {
            child["copySortOrders"] = copied_sort_orders(game, child)?;
        }
    }
    Ok(())
}

/// Resolve every required visual dependency before accepting whole-effect play.
/// Sound names are not material references and never request an audio service.

fn resolve_materials_with_model_reader(
    game: &Path,
    effect: &mut Value,
    model_reader: fn(&Path, &str) -> Result<Value>,
) -> Result<()> {
    let mut model_info = serde_json::Map::new();
    let mut model_materials = serde_json::Map::new();
    for source in std::iter::once(&*effect).chain(
        effect["dependencies"]
            .as_object()
            .into_iter()
            .flat_map(|definitions| definitions.values()),
    ) {
        for element in source["elements"]
            .as_array()
            .ok_or("Missing model elements")?
        {
            if loader::words(element, "editorFlags")
                .iter()
                .any(|flag| flag == "disabled")
            {
                continue;
            }
            for name in loader::words(element, "model") {
                if name.is_empty()
                    || ["{", "}", ";"].contains(&name.as_str())
                    || model_info.contains_key(&name)
                {
                    continue;
                }
                let model = model_assets::read(game, &name)
                    .map_err(|error| format!("Model {name}: {error}"))?;
                for surface in &model.surfaces {
                    let key = format!("__model__/{}", surface.material);
                    if !model_materials.contains_key(&key) {
                        let metadata = model_reader(game, &surface.material).map_err(|error| {
                            format!("Model {name}, material {}: {error}", surface.material)
                        })?;
                        model_materials.insert(key, metadata);
                    }
                }
                model_info.insert(
                    name,
                    serde_json::to_value(model).map_err(|error| error.to_string())?,
                );
            }
        }
    }
    if !model_info.is_empty() {
        effect["modelInfo"] = Value::Object(model_info);
    }
    let own = renderer::material_info(game, effect).map_err(|error| error.to_string())?;
    let mut shared = own.as_object().ok_or("Invalid material map")?.clone();
    shared.extend(model_materials);
    effect["materialInfo"] = own;
    if let Some(definitions) = effect["dependencies"].as_object_mut() {
        for (name, definition) in definitions {
            let information = renderer::material_info(game, definition)
                .map_err(|error| format!("Material dependency in {name}: {error}"))?;
            for (name, value) in information
                .as_object()
                .ok_or("Invalid child material map")?
            {
                shared.insert(name.clone(), value.clone());
            }
            definition["materialInfo"] = information;
        }
    }
    effect["sharedMaterialInfo"] = Value::Object(shared);
    Ok(())
}

/// Camera constants and static mesh packets belong to a selection, not a
/// simulation frame. Packing them once avoids cloning/parsing every model's
/// original vertex JSON for each displayed particle.
pub(crate) struct FrameProjection {
    settings: Value,
    rows: Value,
    view: Value,
    projection: Value,
    view_rows: [[f32; 4]; 4],
    view_axes: [[f32; 3]; 3],
    models: serde_json::Map<String, Value>,
    cloud_vertices: String,
    cloud_indices: Value,
}

impl FrameProjection {
    pub(crate) fn new(model_info: &Value, settings: &Value) -> Result<Self> {
        let view = camera::view_matrix(settings);
        let view_rows = [
            super::array::<4>(&view[0], "view row")?,
            super::array::<4>(&view[1], "view row")?,
            super::array::<4>(&view[2], "view row")?,
            super::array::<4>(&view[3], "view row")?,
        ];
        let view_axes = std::array::from_fn(|axis| {
            [view_rows[axis][0], view_rows[axis][1], view_rows[axis][2]]
        });
        let mut models = serde_json::Map::new();
        if let Some(assets) = model_info.as_object() {
            for (name, asset) in assets {
                let mut surfaces = Vec::new();
                for surface in asset["surfaces"]
                    .as_array()
                    .ok_or("Missing model surface data")?
                {
                    let bytes = model_pose::pack_surface_json(&surface["geometry"])?;
                    surfaces.push(json!({
                        "texture":format!("__model__/{}", surface["material"].as_str().ok_or("Missing model material")?),
                        "vertexFormat":"model32", "vertices":STANDARD.encode(bytes),
                        "indices":surface["geometry"]["indices"]
                    }));
                }
                models.insert(name.clone(), json!(surfaces));
            }
        }
        let (cloud_vertices, cloud_indices) = cloud_geometry::static_mesh(1);
        Ok(Self {
            settings: settings.clone(),
            rows: camera::matrix(settings),
            projection: camera::projection_matrix(settings),
            view,
            view_rows,
            view_axes,
            models,
            cloud_vertices: STANDARD.encode(cloud_vertices),
            cloud_indices: json!(cloud_indices),
        })
    }

    pub(crate) fn project(&self, frame: &mut Value) -> Result<()> {
        if frame.get("previewPlane").is_some() && frame.get("lights").is_none() {
            frame["lights"] = json!([]);
        }
        frame["matrixRows"] = self.rows.clone();
        frame["viewMatrixRows"] = self.view.clone();
        frame["projectionMatrixRows"] = self.projection.clone();
        frame["cameraOrigin"] = self.settings["origin"].clone();
        if let Some(models) = frame["models"].as_array() {
            let mut meshes = Vec::new();
            for model in models {
                let name = model["model"].as_str().ok_or("Missing scene model name")?;
                let surfaces = self
                    .models
                    .get(name)
                    .and_then(Value::as_array)
                    .ok_or("Missing prepared model surface data")?;
                for surface in surfaces {
                    let mut mesh = surface.clone();
                    mesh["modelPlacement"] = model.clone();
                    meshes.push(mesh);
                }
            }
            meshes.extend(
                frame["draws"]
                    .as_array()
                    .ok_or("Missing FX draws")?
                    .iter()
                    .cloned(),
            );
            frame["draws"] = json!(meshes);
        }
        for draw in frame["draws"].as_array_mut().ok_or("Missing frame draws")? {
            if let Some(cloud) = draw.get("cloud").cloned() {
                let origin = super::array(&cloud["origin"], "cloud origin")?;
                let end = super::array(&cloud["endpos"], "cloud endpoint")?;
                let radius = super::array(&cloud["radius"], "cloud radii")?;
                let quat = super::array(&cloud["quat"], "cloud quaternion")?;
                let scale = cloud["scale"]
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .ok_or("Missing finite cloud scale")? as f32;
                let bytes = cloud["bgra"].as_array().ok_or("Missing cloud colour")?;
                if bytes.len() != 4 {
                    return Err("Cloud colour must contain four bytes".into());
                }
                let mut color = [0_u8; 4];
                for (index, value) in bytes.iter().enumerate() {
                    color[index] = u8::try_from(value.as_u64().ok_or("Invalid cloud colour byte")?)
                        .map_err(|error| error.to_string())?;
                }
                draw["vertexFormat"] = json!("cloud20");
                draw["vertices"] = json!(self.cloud_vertices);
                draw["indices"] = self.cloud_indices.clone();
                draw["cloudMatrix"] =
                    json!(cloud_geometry::matrix(origin, end, radius, self.view_axes));
                draw["cloudColor"] = json!(cloud_geometry::color(color));
                draw["worldViewMatrixRows"] = json!(cloud_geometry::world_view_rows(
                    quat,
                    origin,
                    scale,
                    self.view_rows
                ));
            }
        }
        if super::memory_cost(frame) > MAX_CLIP_BYTES {
            return Err("This effect exceeds the 128 MiB preview memory limit".into());
        }
        Ok(())
    }
}

pub(crate) fn project(playback: &mut Value, settings: &Value) -> Result<()> {
    let projection = FrameProjection::new(&playback["modelInfo"], settings)?;
    let mut retained_bytes = 0_usize;
    for frame in playback["frames"].as_array_mut().ok_or("Missing frames")? {
        projection.project(frame)?;
        retained_bytes = retained_bytes.saturating_add(super::memory_cost(frame));
        if retained_bytes > MAX_CLIP_BYTES {
            return Err("This effect exceeds the 128 MiB preview memory limit".into());
        }
    }
    playback["camera"] = settings.clone();
    Ok(())
}

fn camera_contains(settings: &Value, bounds: &Value) -> bool {
    let rows = camera::matrix(settings);
    for corner in 0..8 {
        let point: [f64; 3] =
            std::array::from_fn(|axis| bounds[(corner >> axis) & 1][axis].as_f64().unwrap());
        let clip: [f64; 4] = std::array::from_fn(|row| {
            rows[row][3].as_f64().unwrap()
                + (0..3)
                    .map(|axis| rows[row][axis].as_f64().unwrap() * point[axis])
                    .sum::<f64>()
        });
        if clip[3] <= 0.0
            || clip[2] < 0.0
            || clip[2] > clip[3]
            || (clip[0] / clip[3]).abs() > 0.801
            || (clip[1] / clip[3]).abs() > 0.801
        {
            return false;
        }
    }
    true
}

/// Actual depth-feather shaders use physical clip W for camera-distance fades.
/// Fit once, regenerate camera-dependent tails/sorting, then verify the final
/// bounds rather than retaining the old orthographic W=1 graphics fixture.
fn simulate_fitted(
    effect: &mut Value,
    seed: u32,
    times: &[u32],
    cancelled: &dyn Fn() -> bool,
) -> Result<(Value, Value)> {
    let mut playback = sample_playback(effect, seed, times, cancelled)?;
    fit_light_receiver(effect, &mut playback)?;
    let has_geometry = playback["frames"]
        .as_array()
        .ok_or("Missing frames")?
        .iter()
        .any(|frame| {
            frame["draws"]
                .as_array()
                .is_some_and(|draws| !draws.is_empty())
                || frame["models"]
                    .as_array()
                    .is_some_and(|models| !models.is_empty())
                || frame["lights"]
                    .as_array()
                    .is_some_and(|lights| !lights.is_empty())
                || frame.get("previewPlane").is_some()
        });
    if !has_geometry {
        let settings = json!({"origin":[200.0,0.0,0.0], "fovDegrees":60.0,
            "near":1.0,"far":8192.0,"emptyGeometry":true});
        camera_fit::apply(effect, &settings)?;
        let selected = camera_fit::choose(effect, seed, times, &settings, 0.0, cancelled)?;
        if selected != settings {
            drop(playback);
            playback = sample_playback(effect, seed, times, cancelled)?;
        }
        project(&mut playback, &selected)?;
        return Ok((playback, selected));
    }
    let mut history = camera::fit(playback["frames"].as_array().unwrap())?;
    for attempt in 0..8 {
        check_cancelled(cancelled)?;
        let current = camera::fit(playback["frames"].as_array().unwrap())?;
        let settings = if attempt < 4 {
            current
        } else {
            history.clone()
        };
        camera_fit::apply(effect, &settings)?;
        // Release the prior sample buffers before preparing the fitted pass.
        drop(playback);
        playback = sample_playback(effect, seed, times, cancelled)?;
        let final_bounds = camera::fit(playback["frames"].as_array().unwrap())?;
        if camera_contains(&settings, &final_bounds["bounds"]) {
            project(&mut playback, &settings)?;
            let score = camera_fit::playback_score(effect, &playback, &settings)?;
            let selected = camera_fit::choose(effect, seed, times, &settings, score, cancelled)?;
            if selected != settings {
                drop(playback);
                playback = sample_playback(effect, seed, times, cancelled)?;
                project(&mut playback, &selected)?;
            }
            return Ok((playback, selected));
        }
        history = union_camera_bounds(&history, &final_bounds)?;
    }
    Err("Camera-dependent geometry did not converge to a complete preview fit".into())
}

fn sample_playback(
    effect: &Value,
    seed: u32,
    times: &[u32],
    cancelled: &dyn Fn() -> bool,
) -> Result<Value> {
    // Use the same retained scheduler as live playback. Camera/thumbnail
    // preparation must not hit an obsolete cumulative spawn/instance limit.
    // Visit the same scheduler samples with the same retention budget, but
    // release background preparation promptly when a selection supersedes it.
    check_cancelled(cancelled)?;
    let budget = effect["previewFrameByteBudget"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(MAX_CLIP_BYTES);
    let mut retained = 0_usize;
    let mut frames = Vec::with_capacity(times.len());
    super::simulation::visit_samples(effect, seed, times, &mut |frame| {
        check_cancelled(cancelled)?;
        retained = retained.saturating_add(super::memory_cost(&frame));
        if retained > budget {
            return Err("This effect exceeds the preview memory limit".into());
        }
        frames.push(frame);
        Ok(())
    })?;
    check_cancelled(cancelled)?;
    Ok(json!({"frames":frames,"modelInfo":effect["modelInfo"]}))
}

/// Selected playback needs bounds, not a recording of every frame. Visiting
/// the exact same samples keeps the camera maths while bounding RAM to one
/// frame, even for dense rain and smoke.
fn fit_streaming_camera(
    effect: &mut Value,
    seed: u32,
    times: &[u32],
    cancelled: &dyn Fn() -> bool,
) -> Result<Value> {
    let (mut bounds, mut receivers) = visit_camera_bounds(effect, seed, times, cancelled)?;
    let previous_plane = effect.get("previewPlane").cloned();
    fit_light_receiver(effect, &mut receivers)?;
    if effect.get("previewPlane") != previous_plane.as_ref() {
        (bounds, _) = visit_camera_bounds(effect, seed, times, cancelled)?;
    }
    if bounds["emptyGeometry"] == true {
        camera_fit::apply(effect, &bounds)?;
        return camera_fit::choose(effect, seed, times, &bounds, 0.0, cancelled);
    }
    let mut history = bounds.clone();
    for attempt in 0..8 {
        let settings = if attempt < 4 { bounds } else { history.clone() };
        camera_fit::apply(effect, &settings)?;
        let (next_bounds, _, score) =
            visit_camera_bounds_scored(effect, seed, times, cancelled, Some(&settings))?;
        if next_bounds["emptyGeometry"] == true
            || camera_contains(&settings, &next_bounds["bounds"])
        {
            return camera_fit::choose(effect, seed, times, &settings, score, cancelled);
        }
        history = union_camera_bounds(&history, &next_bounds)?;
        bounds = next_bounds;
    }
    Err("Camera-dependent geometry did not converge to a complete preview fit".into())
}

/// View-dependent tails or child ordering can oscillate between two fitted
/// bounds. After the normal passes, fit their accumulated union and still
/// verify every fresh sample against it. No geometry or UVs are changed.
fn union_camera_bounds(left: &Value, right: &Value) -> Result<Value> {
    let mut minimum = [0.0_f32; 3];
    let mut maximum = [0.0_f32; 3];
    for axis in 0..3 {
        minimum[axis] = (left["bounds"][0][axis]
            .as_f64()
            .ok_or("Missing camera minimum")? as f32)
            .min(
                right["bounds"][0][axis]
                    .as_f64()
                    .ok_or("Missing camera minimum")? as f32,
            );
        maximum[axis] = (left["bounds"][1][axis]
            .as_f64()
            .ok_or("Missing camera maximum")? as f32)
            .max(
                right["bounds"][1][axis]
                    .as_f64()
                    .ok_or("Missing camera maximum")? as f32,
            );
    }
    let count = left["vertexSamples"]
        .as_u64()
        .unwrap_or(0)
        .saturating_add(right["vertexSamples"].as_u64().unwrap_or(0)) as usize;
    camera::fit_bounds(minimum, maximum, count)
}

fn visit_camera_bounds(
    effect: &Value,
    seed: u32,
    times: &[u32],
    cancelled: &dyn Fn() -> bool,
) -> Result<(Value, Value)> {
    let (bounds, receivers, _) = visit_camera_bounds_scored(effect, seed, times, cancelled, None)?;
    Ok((bounds, receivers))
}

/// Score during an existing bounds pass instead of retaining or replaying an
/// additional full animation for every ordinary thumbnail and selection.
fn visit_camera_bounds_scored(
    effect: &Value,
    seed: u32,
    times: &[u32],
    cancelled: &dyn Fn() -> bool,
    settings: Option<&Value>,
) -> Result<(Value, Value, f64)> {
    let mut minimum = [f32::INFINITY; 3];
    let mut maximum = [f32::NEG_INFINITY; 3];
    let mut count = 0_usize;
    let mut receivers = Vec::new();
    let projection = settings
        .filter(|_| camera_fit::needs_alternatives(effect))
        .map(|settings| FrameProjection::new(&effect["modelInfo"], settings))
        .transpose()?;
    let mut score = if projection.is_none() {
        1.0_f64
    } else {
        0.0_f64
    };
    let mut positive_samples = 0_usize;
    let mut cloud_area = 0.0_f64;
    super::simulation::visit_samples(effect, seed, times, &mut |mut frame| {
        check_cancelled(cancelled)?;
        if let Some(lights) = frame["lights"].as_array() {
            receivers.push(json!({"lights":lights}));
        }
        match camera::fit(std::slice::from_ref(&frame)) {
            Ok(settings) => {
                for axis in 0..3 {
                    minimum[axis] = minimum[axis].min(
                        settings["bounds"][0][axis]
                            .as_f64()
                            .ok_or("Missing minimum bound")? as f32,
                    );
                    maximum[axis] = maximum[axis].max(
                        settings["bounds"][1][axis]
                            .as_f64()
                            .ok_or("Missing maximum bound")? as f32,
                    );
                }
                count =
                    count.saturating_add(settings["vertexSamples"].as_u64().unwrap_or(0) as usize);
            }
            Err(error) if error == "cannot fit a camera without rendered geometry" => {}
            Err(error) => return Err(error),
        }
        if let Some(projection) = &projection {
            projection.project(&mut frame)?;
            let current_score = super::camera_visibility::score(
                &frame,
                settings.unwrap(),
                &effect["sharedMaterialInfo"],
            )?;
            score = score.max(current_score);
            if current_score >= 0.00000001 {
                positive_samples += 1;
                cloud_area = cloud_area.max(super::camera_visibility::cloud_screen_area(
                    &frame,
                    &effect["sharedMaterialInfo"],
                )?);
            }
        }
        Ok(())
    })?;
    let mut settings = if count == 0 {
        json!({"origin":[200.0,0.0,0.0],"fovDegrees":60.0,"near":1.0,"far":8192.0,"emptyGeometry":true})
    } else {
        camera::fit_bounds(minimum, maximum, count)?
    };
    if projection.is_some() {
        settings["cameraPositiveSamples"] = json!(positive_samples);
        settings["cameraCloudArea"] = json!(cloud_area);
    }
    Ok((settings, json!({"frames":receivers}), score))
}

/// A small light can finish without ever reaching the original large receiver.
/// Move only that optional preview plane; the effect's light data stays intact.
fn fit_light_receiver(effect: &mut Value, playback: &mut Value) -> Result<()> {
    let Some(plane) = effect.get("previewPlane") else {
        return Ok(());
    };
    if plane["depthOnly"] == true {
        return Ok(());
    }
    let height = plane["height"].as_f64().unwrap_or(-40.0);
    let half_size = plane["size"].as_f64().unwrap_or(256.0) / 2.0;
    let frames = playback["frames"]
        .as_array()
        .ok_or("Missing sampled frames")?;
    let mut strongest = None;
    let mut best_score = 0.0;
    for light in frames
        .iter()
        .filter_map(|frame| frame["lights"].as_array())
        .flatten()
    {
        let radius = light["radius"].as_f64().unwrap_or(0.0).clamp(0.0, 2000.0);
        let rgb = light["rgb"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_f64)
                    .fold(0.0_f64, f64::max)
            })
            .unwrap_or(0.0);
        let Some(origin) = light["origin"]
            .as_array()
            .filter(|values| values.len() == 3)
        else {
            continue;
        };
        let origin = [
            origin[0].as_f64().unwrap_or(0.0),
            origin[1].as_f64().unwrap_or(0.0),
            origin[2].as_f64().unwrap_or(0.0),
        ];
        if rgb <= 0.0 || radius <= 0.0 {
            continue;
        }
        if origin[0] > height
            && (origin[0] - height).abs() < radius * 0.8
            && origin[1].abs() < half_size + radius * 0.5
            && origin[2].abs() < half_size + radius * 0.5
        {
            return Ok(());
        }
        let score = radius * rgb;
        if score > best_score {
            best_score = score;
            strongest = Some((origin, radius));
        }
    }
    if let Some((origin, radius)) = strongest {
        effect["previewPlane"]["height"] = json!(origin[0] - radius * 0.3);
        effect["previewPlane"]["size"] =
            json!((2.0 * (origin[1].abs().max(origin[2].abs()) + radius)).clamp(16.0, 4096.0));
        for frame in playback["frames"]
            .as_array_mut()
            .ok_or("Missing sampled frames")?
        {
            frame["previewPlane"] = effect["previewPlane"].clone();
        }
    }
    Ok(())
}
