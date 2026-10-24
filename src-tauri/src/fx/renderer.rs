//! Native D3D9 playback using the user's installed Modtools shader programs.
//! Numerical rendering paths preserve the checked native declarations, state
//! and shader constants. Unsupported assets reject the complete preview.
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::ptr::{copy_nonoverlapping, null, null_mut};
use std::sync::Arc;
use std::time::SystemTime;

use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D9::*;

use super::{model_material, model_order, wavelet};
pub(crate) mod background;
mod image_coverage;
mod plane;
use plane as plane_render;

pub(crate) use image_coverage::annotate as annotate_image_coverage;

pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// Fresh, attributed copies of the published Image Viewer's wavelet decoder.
// No sibling app or archived research is a build/runtime dependency.

pub(crate) fn model_material_info(game: &Path, name: &str) -> Result<Value> {
    Ok(model_material::read(game, name)?.metadata)
}

/// The installed FX images use one wavelet-luminance image. Keep its original
/// luminance semantics (RGB=L, A=1) through D3D9 L8 rather than treating L as alpha.
/// Other wavelet channel layouts remain outside this bounded renderer path.
fn wavelet_luminance_levels(bytes: &[u8]) -> Result<Vec<(u32, u32, Vec<u8>)>> {
    if bytes.len() < 28 || bytes[..4] != *b"IWi\x06" || bytes[4] != 9 {
        return Err("Expected a WaW v6 wavelet-luminance image".into());
    }
    let width = u16::from_le_bytes(bytes[6..8].try_into()?) as u32;
    let height = u16::from_le_bytes(bytes[8..10].try_into()?) as u32;
    let depth = u16::from_le_bytes(bytes[10..12].try_into()?);
    if width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || u64::from(width) * u64::from(height) > 16_777_216
        || depth != 1
        || bytes[5] & 12 != 0
    {
        return Err("Unsupported wavelet dimensions or image shape".into());
    }
    let levels = if bytes[5] & 2 != 0 {
        1
    } else {
        (32 - width.max(height).leading_zeros()) as usize
    };
    let decoded = wavelet::decode(&bytes[28..], width as usize, height as usize, levels, 1, 1)?;
    Ok(decoded
        .into_iter()
        .enumerate()
        .map(|(mip, mut faces)| {
            (
                (width >> mip).max(1),
                (height >> mip).max(1),
                faces.remove(0),
            )
        })
        .collect())
}

/// Raw RGB IWI pixels are stored B,G,R. D3D9's X8R8G8B8 accepts
/// B,G,R,X in memory and supplies alpha=1 to the original shader.
fn raw_rgb_row(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() % 3 != 0 {
        return Err("Incomplete raw RGB row".into());
    }
    let mut expanded = Vec::with_capacity(bytes.len() / 3 * 4);
    for pixel in bytes.chunks_exact(3) {
        expanded.extend_from_slice(pixel);
        expanded.push(255);
    }
    Ok(expanded)
}

/// EffectsEd keeps one sprite material batch open while clouds are submitted.
/// Only a different sprite material (or the final dispatcher flush) emits that
/// batch. This preserves the shared primary-48 unsorted queue's actual order.
fn frontend_enqueue_indices(draws: &[Value]) -> Result<Vec<usize>> {
    let mut enqueued = Vec::with_capacity(draws.len());
    let mut pending = Vec::new();
    let mut pending_material: Option<&str> = None;

    for (index, draw) in draws.iter().enumerate() {
        // Scene models were already submitted in the opaque prepass. They
        // never enter the pending sprite/cloud frontend batch.
        if draw["vertexFormat"] == "model32" {
            continue;
        }
        let material = draw["texture"].as_str().ok_or("Missing material")?;
        if draw["vertexFormat"] == "cloud20" {
            enqueued.push(index);
            continue;
        }
        // Native trail traversal happens after every effect's ordinary sprites
        // and clouds. Its interaction with this mixed queue is not yet proved.
        if draw.get("indices").is_some() {
            return Err(
                "Mixed primary-48 cloud/indexed-sprite frontend order remains unverified".into(),
            );
        }
        if pending_material.is_some_and(|previous| previous != material) {
            enqueued.append(&mut pending);
        }
        pending_material = Some(material);
        pending.push(index);
    }
    enqueued.append(&mut pending);
    Ok(enqueued)
}

pub(super) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or("Truncated material")?
            .try_into()?,
    ))
}

pub(super) fn string_at(bytes: &[u8], offset: usize) -> Result<String> {
    let remainder = bytes
        .get(offset..)
        .ok_or("Invalid material string offset")?;
    let length = remainder
        .iter()
        .position(|value| *value == 0)
        .ok_or("Unterminated material string")?;
    Ok(std::str::from_utf8(&remainder[..length])?.to_owned())
}

pub(super) fn relative_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with(['/', '\\'])
        || name.contains(':')
        || name.split(['/', '\\']).any(|part| part == "..")
    {
        return Err("Asset name must be relative and remain inside its asset folder".into());
    }
    Ok(())
}

pub(super) fn pinned_file(path: &Path, expected: &str) -> Result<Vec<u8>> {
    let bytes = fs::read(path)?;
    if digest(&bytes) != expected {
        return Err(format!("Installed verified pass/shader changed: {}", path.display()).into());
    }
    Ok(bytes)
}

/// Select the installed emissive pass, retaining grouped technique labels.
fn emissive_pass(text: &str) -> Result<String> {
    let mut selected = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('"') && line.ends_with(':') {
            if line == "\"emissive\":" {
                selected = true;
            }
        } else if line.ends_with(';') {
            if selected {
                let name = line.trim_end_matches(';').trim();
                relative_name(name)?;
                return Ok(name.to_owned());
            }
            selected = false;
        }
    }
    Err("No installed emissive technique".into())
}

/// Trails use indexed native streams; old quads retain their implicit triangles.
fn validated_indices(draw: &Value, vertex_bytes: usize) -> Result<Vec<u16>> {
    let stride = match draw["vertexFormat"].as_str() {
        Some("cloud20") => 20,
        Some("model32") => 32,
        _ => 32,
    };
    if vertex_bytes == 0 || vertex_bytes % stride != 0 || vertex_bytes / stride > 65535 {
        return Err("Expected 1..65535 complete vertices in the declared native format".into());
    }
    let vertex_count = vertex_bytes / stride;
    let indices = if let Some(values) = draw.get("indices") {
        values
            .as_array()
            .ok_or("Indices must be an array")?
            .iter()
            .map(|value| {
                let index = value.as_u64().ok_or("Index must be an unsigned integer")?;
                if index >= vertex_count as u64 || index > u16::MAX as u64 {
                    return Err("Mesh index outside native vertex stream".into());
                }
                Ok(index as u16)
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        if vertex_count != 4 {
            return Err("Non-quad draw requires explicit indices".into());
        }
        vec![0, 1, 2, 2, 3, 0]
    };
    if indices.is_empty() || indices.len() % 3 != 0 || indices.len() > u32::MAX as usize {
        return Err("Expected a nonempty complete triangle index list".into());
    }
    Ok(indices)
}

pub(super) struct Material {
    pub(super) metadata: Value,
    pub(super) image: String,
    pub(super) additive_shader: bool,
    pub(super) cloud: bool,
    pub(super) vertex_program: String,
    pub(super) pixel_program: String,
    pub(super) feather: Option<[f32; 4]>,
    pub(super) source: u32,
    pub(super) destination: u32,
    pub(super) operation: u32,
    pub(super) alpha_test: Option<(u32, u32)>,
    pub(super) alpha_source: u32,
    pub(super) alpha_destination: u32,
    pub(super) cull_mode: u32,
    pub(super) falloff: Option<[[f32; 4]; 3]>,
    pub(super) eye_offset: Option<[f32; 4]>,
    pub(super) distortion_scale: Option<[f32; 4]>,
    pub(super) second_word: u32,
    pub(super) sampler: u32,
}

/// Bounded material-index order recovered from the original comparator.
/// Only exact audited material bytes receive a rank; this is not a general
/// alphabetical replacement for EffectsEd's multi-field material sorter.
fn cloud_sort_identity(sha256: &str) -> Option<(String, u64)> {
    // Exact hash identities are compatibility data recovered from native sort
    // checks. Unknown equal-primary groups remain unsupported.
    match sha256 {
        "357fb9f500b968cb3d16802fdf2e780a417a504fca4b11854340211483016faf" => {
            Some(("camonet-clouds".to_owned(), 0))
        }
        "5bec4bf00cba41e0564abeb9cc52375bd3e10964386159802bc5c6b0d1f3aa53" => {
            Some(("camonet-clouds".to_owned(), 1))
        }
        "56f926a30cd016e761fdb60eb8b20d760010f066ba50c7004c78537e416ed614" => {
            Some(("camonet-clouds".to_owned(), 2))
        }
        "931bbc64a5c77d05221fb6376cefcdba6c1365cb69705b530ef950e72b28f3e9" => {
            Some(("rain-blend".to_owned(), 0))
        }
        "b50f8eb0728a0d861a0e9b0f6b330741c5815a1c190d3a7da5aed2d6431cb056" => {
            Some(("rain-blend".to_owned(), 1))
        }
        "2b1e29def8cb0821adc6378ca4e48d4357690d4eee2d867ab270cc892967a615" => {
            Some(("rain-blend".to_owned(), 2))
        }
        "4a3381b7e1576ae3142410f40fbc1c278ebeb7946669938e7c1acbc71da7dd74" => {
            Some(("rain-add".to_owned(), 0))
        }
        "bfc51ba3a980fb70f059321633fba2993bcc07e1a009e395e6a6f3b91618e9e8" => {
            Some(("rain-add".to_owned(), 1))
        }
        "82232587a35f01c88631c102831b6775c411204b124d310076693e329e8590c5" => {
            Some(("camonet-clouds".to_owned(), 4))
        }
        "d626ced0b978ebc84c82755c70df82ee86c2b1b5de09eaf7997a66ba0c1b439c" => {
            Some(("camonet-clouds".to_owned(), 5))
        }
        "2e7c2a87f34c3ba6f161e2f745fd15b2d86d647601aeaa721411146c061c62c9" => {
            Some(("camonet-clouds".to_owned(), 6))
        }
        "2f7b6db8de34386090e0eae3ab57d07c024699f9b8f827c662bfdcc4df4a89f0" => {
            Some(("camonet-clouds".to_owned(), 3))
        }
        "9de31a0b9b922b6512a2b6152f693ba42b32dfa365d587ba048e0cfcf1efa7b0" => {
            Some(("pollen-clouds".to_owned(), 0))
        }
        "2846342bb930965f17babb017af00baf1cefdaf503df04f09d806428b3e7a347" => {
            Some(("pollen-clouds".to_owned(), 1))
        }
        "70c3d042020479de880affa199080b4870b35b01b13f8641dc648a751590d6af" => {
            Some(("pollen-clouds".to_owned(), 2))
        }
        "7fa1f8fa7c38a6a229f71c9068eeccfe81e43154604b3c7958f3132aa3fc0d83" => {
            Some(("pollen-clouds".to_owned(), 3))
        }
        _ => None,
    }
}

/// Fixed empty-preview environment: native EffectsEd r_outdoor=0.
///
/// Original routine 0x484880 adds disable flag 2, and 0x484dc0 selects the
/// corresponding non-outdoor technique. Keep this an explicit, bounded table
/// backed by the offline native captures. Other outdoor names remain blocked;
/// a guessed white outdoor lookup image would not reproduce this policy.
fn empty_preview_pass(authored: &str) -> (&str, Option<&'static str>) {
    match authored {
        "zfeather_outdoor_add_fog_dtex" => (
            "zfeather_add_fog_dtex",
            Some("a203211321b94b7bf7bbb9459006f607fa7bed382d87ac9c32df8d5e76e32b34"),
        ),
        "particle_cloud_outdoor" => (
            "particle_cloud",
            Some("1ebf628f7c747a77de763850b8b5c7227af5b625eb7a751d6a349681738d0eeb"),
        ),
        "particle_cloud_outdoor_add" => (
            "particle_cloud_add",
            Some("acc466a371235a104568f4b9d4819e5292f2d846759d5b6ea6785d9be49962e5"),
        ),
        "zfeather_outdoor_fog_dtex" => (
            "zfeather_fog_dtex",
            Some("af305ef3025059faec3ccbfda8e4a21c7d25ae54fde8445cce2fc7b2249386df"),
        ),
        _ => (authored, None),
    }
}

/// Resolve the original installed pass. Depth feathering uses the original
/// shaders and float-Z resource. Distortion is restricted to the byte-pinned
/// scale/zfeather pass and the original pre-emissive scene-copy contract.
fn material(game: &Path, name: &str) -> Result<Material> {
    if let Some(actual) = name.strip_prefix("__model__/") {
        return model_material::read(game, actual);
    }
    relative_name(name)?;
    let raw = game.join("raw");
    let bytes = fs::read(raw.join("materials").join(name))?;
    if bytes.len() < 60 {
        return Err("Truncated material header".into());
    }
    let word = u32_at(&bytes, 40)?;
    let second_word = u32_at(&bytes, 44)?;
    // This milestone only handles the particle-preview state configuration
    // already exercised by the native D3D9 comparison. Other cull/stencil/etc
    // configurations must be decoded rather than silently flattened.
    // The pass state map replaces authored separate-alpha blending below.
    // Dust-mote clouds carry a different authored alpha triplet (0x0d25),
    // but the pinned default map resolves both to Add/InvDestAlpha/One.
    if ![0x0812_0000, 0x0d25_0000].contains(&(word & !0x1000_ffff))
        || ![2, 12, 0x4006c, 0x4004c, 0x4004d, 0x2c, 13].contains(&second_word)
    {
        return Err(format!(
            "Unverified render-state configuration for {name}: {word:#x}/{second_word:#x}"
        )
        .into());
    }
    // Native EffectsEd setter 0x492740 indexes table 0x54c89c with
    // (word >> 14) & 3. Offline capture proves [invalid, None, CCW, CW].
    let cull_mode = match (word >> 14) & 3 {
        1 => 1,
        2 => 3,
        3 => 2,
        _ => return Err("Invalid native material cull mode".into()),
    };
    let family = string_at(&bytes, u32_at(&bytes, 52)? as usize)?;
    relative_name(&family)?;
    let techset_path = raw.join("techsets").join(format!("{family}.techset"));
    let techset = fs::read(&techset_path)?;
    let authored_pass = emissive_pass(std::str::from_utf8(&techset)?)?;
    let (effective_pass, authored_outdoor_hash) = empty_preview_pass(&authored_pass);
    let pass = effective_pass.to_owned();
    if let Some(hash) = authored_outdoor_hash {
        // Pin the authored technique as well as the effective one below. The
        // authored material, blend state and colour image are never replaced.
        pinned_file(
            &raw.join("techniques").join(format!("{authored_pass}.tech")),
            hash,
        )?;
    }
    let cloud = matches!(pass.as_str(), "particle_cloud" | "particle_cloud_add");
    let (technique_hash, state_name, state_hash, additive_shader) = match pass.as_str() {
        "distortion_scale_zfeather_dtex" => (
            "ea1420c16892ef9f11eea63cc9af0a30ce254217ed117c016176b84e8436e24c",
            "default",
            "1e0de3a39c5c755b23c741a187138172485d03b08adef8070c3eb7a14c3a6bfc",
            false,
        ),
        "particle_cloud" => (
            "5a5b0532c7ff5438bfe6b21a8a339c5cbb64417be564b3d2caea0666c6160d15",
            "default",
            "1e0de3a39c5c755b23c741a187138172485d03b08adef8070c3eb7a14c3a6bfc",
            false,
        ),
        "particle_cloud_add" => (
            "4c70056b54db76ae2e6b979901882e68ddfe638ac304428f5365a729901b458f",
            "default",
            "1e0de3a39c5c755b23c741a187138172485d03b08adef8070c3eb7a14c3a6bfc",
            true,
        ),
        "vertcol_simple_dtex" => (
            "0851261f7c2d1ca02d01e8e7f32a8af2e2ed1fecfa3ffd93121ceac9bf80618f",
            "default",
            "1e0de3a39c5c755b23c741a187138172485d03b08adef8070c3eb7a14c3a6bfc",
            false,
        ),
        "effect_add_dtex" => (
            "3e4dd231f84a05f0d95fc07a30323f8b16e13662caace522b629ac7fe9f5dac7",
            "effect_add",
            "ed57f22bb788b19c1efb4e4c7705bea531c41163a544db87bef69ca46ca1fca1",
            true,
        ),
        "zfeather_dtex" => (
            "d08567902bc68f59e54f3961d61707262eba94fc32f7ca1ce53aa3f3a4025118",
            "default",
            "1e0de3a39c5c755b23c741a187138172485d03b08adef8070c3eb7a14c3a6bfc",
            false,
        ),
        "zfeather_add_dtex" => (
            "255511d519eaa73647fd3ce6b025306c01fc4f50b0561ed7cb7f9985896e631c",
            "zfeather_add",
            "0a6564c4c287e9748aad78e410ad63d791a247124d610043e2db844f50bfccc6",
            true,
        ),
        "zfeather_fog_dtex" => (
            "84b376f66e9c3e17026c99c0d1cabba562e46e0d8bca3387ae87af259b54d088",
            "default",
            "1e0de3a39c5c755b23c741a187138172485d03b08adef8070c3eb7a14c3a6bfc",
            false,
        ),
        "zfeather_add_fog_dtex" => (
            "515cb8fd10b10fc7e4c0f4799036fbdbcd5962aa698f59f50b0de6974e7cadd4",
            "zfeather_add",
            "0a6564c4c287e9748aad78e410ad63d791a247124d610043e2db844f50bfccc6",
            true,
        ),
        "vertcol_simple_fog_dtex" => (
            "4a90ef9e3eb788281e923548948d4ff4e1b8230fd38c93c9c867e0a66ce858bc",
            "default",
            "1e0de3a39c5c755b23c741a187138172485d03b08adef8070c3eb7a14c3a6bfc",
            false,
        ),
        "effect_add_fog_dtex" => (
            "e329234b5382b99b2ca96fc1544a4f7e1a8f5feb98dbf5a456652a1534ef6dac",
            "effect_add",
            "ed57f22bb788b19c1efb4e4c7705bea531c41163a544db87bef69ca46ca1fca1",
            true,
        ),
        "effect_add_eyeofs_dtex" => (
            "935ad7d945818e18dddfdf42d21afeb685149e97506b233605bc0ec1dc1737d6",
            "effect_add",
            "ed57f22bb788b19c1efb4e4c7705bea531c41163a544db87bef69ca46ca1fca1",
            true,
        ),
        "effect_add_eyeofs_fog_dtex" => (
            "f93aaf37c70ea67f8c7244645b8f0bd46f291b6af12609674991280232d5f0e1",
            "effect_add",
            "ed57f22bb788b19c1efb4e4c7705bea531c41163a544db87bef69ca46ca1fca1",
            true,
        ),
        "zfeather_add_eyeofs_dtex" => (
            "17d4d1db3e4fc74ea5cf89d799bcf6c57960b9d41e37cbff4935e667636f93ee",
            "zfeather_add",
            "0a6564c4c287e9748aad78e410ad63d791a247124d610043e2db844f50bfccc6",
            true,
        ),
        "effect_add_eyeofs_falloff_dtex" => (
            "54b55065c76d56fa5d916ec5e7501e64256743ab01ec6e2a461622289d2e5173",
            "effect_add",
            "ed57f22bb788b19c1efb4e4c7705bea531c41163a544db87bef69ca46ca1fca1",
            true,
        ),
        "effect_add_eyeofs_falloff_fog_dtex" => (
            "23ff5a8d3230354d07c0abbc0ca2c701f40e0b5ed3ee295173f8f4a802a36e40",
            "effect_add",
            "ed57f22bb788b19c1efb4e4c7705bea531c41163a544db87bef69ca46ca1fca1",
            true,
        ),
        "effect_add_falloff_fog_dtex" => (
            "f56c7055fc4bfd4ba04100da348ce7f9675c0ccb09b9d0c9a0ac01c1f8bf3dc9",
            "effect_add",
            "ed57f22bb788b19c1efb4e4c7705bea531c41163a544db87bef69ca46ca1fca1",
            true,
        ),
        "zfeather_add_falloff_dtex" => (
            "fdf0818e1ea79d02a9a470b76bacf41031cc00a74a3792df88af82aa7448cabb",
            "zfeather_add",
            "0a6564c4c287e9748aad78e410ad63d791a247124d610043e2db844f50bfccc6",
            true,
        ),
        "zfeather_add_falloff_fog_dtex" => (
            "7294ab031393960ad7bebe2500689f846951bbdb8bb9e40f771b076da6b79986",
            "zfeather_add",
            "0a6564c4c287e9748aad78e410ad63d791a247124d610043e2db844f50bfccc6",
            true,
        ),
        "zfeather_falloff_fog_dtex" => (
            "87f18d5d2876caf00164092d51345040859e09df9b97a365a7044b4439d1eb2d",
            "default",
            "1e0de3a39c5c755b23c741a187138172485d03b08adef8070c3eb7a14c3a6bfc",
            false,
        ),
        _ => return Err(format!("Shader binding not implemented: {name} -> {pass}").into()),
    };
    pinned_file(
        &raw.join("techniques").join(format!("{pass}.tech")),
        technique_hash,
    )?;
    pinned_file(
        &raw.join("statemaps").join(format!("{state_name}.sm")),
        state_hash,
    )?;
    let count = u16::from_le_bytes(bytes[48..50].try_into()?) as usize;
    let table = u32_at(&bytes, 56)? as usize;
    let mut image = None;
    let mut material_sampler = None;
    for index in 0..count {
        let offset = table + 12 * index;
        let slot = string_at(&bytes, u32_at(&bytes, offset)? as usize)?;
        if slot == "colorMap" {
            let sampler = u32_at(&bytes, offset + 4)?;
            if ![746, 0x242, 0x24a, 0x20a, 0x202, 0x2eb, 0x2e1].contains(&sampler) {
                return Err(format!("Unverified sampler state for {name}: {sampler:#x}").into());
            }
            material_sampler = Some(sampler);
            image = Some(string_at(&bytes, u32_at(&bytes, offset + 8)? as usize)?);
        }
    }
    let sampler = material_sampler.ok_or("Missing color sampler")?;
    let image = image.ok_or("Material has no colorMap")?;
    relative_name(&image)?;
    let source = word & 15;
    let destination = (word >> 4) & 15;
    let operation = (word >> 8) & 7;
    if operation > 1 || !(1..=10).contains(&source) || !(1..=10).contains(&destination) {
        return Err(format!("Unverified blend/depth-write combination for {name}").into());
    }
    let mut alpha = if word & 0x800 != 0 {
        0
    } else {
        (word >> 12) & 3
    };
    // Both pinned state maps substitute GT0 for ordinary alpha blending.
    if alpha == 0 && source == 5 && [2, 6].contains(&destination) {
        alpha = 1;
    }
    let alpha_test = match alpha {
        0 if word & 0x800 != 0 => None,
        1 => Some((5, 0)),
        2 => Some((2, 128)),
        3 => Some((7, 128)),
        _ => return Err("Unverified alpha-test state".into()),
    };
    // Binary material v1: u16 constant count at +50; +60 points to
    // records containing a name offset followed by four native float values.
    let constant_count = u16::from_le_bytes(bytes[50..52].try_into()?) as usize;
    let constant_table = u32_at(&bytes, 60)? as usize;
    let mut material_constants = serde_json::Map::new();
    for index in 0..constant_count {
        let offset = constant_table + index * 20;
        let key = string_at(&bytes, u32_at(&bytes, offset)? as usize)?;
        let mut values = [0.0f32; 4];
        for component in 0..4 {
            values[component] = f32::from_bits(u32_at(&bytes, offset + 4 + component * 4)?);
            if !values[component].is_finite() {
                return Err("Nonfinite material constant".into());
            }
        }
        material_constants.insert(key, json!(values));
    }
    let feather = if pass.starts_with("zfeather") {
        let values = material_constants
            .get("featherParms")
            .and_then(Value::as_array)
            .ok_or("Missing material featherParms")?;
        Some(std::array::from_fn(|index| {
            values[index].as_f64().unwrap() as f32
        }))
    } else {
        None
    };
    let falloff = if pass.contains("falloff") {
        let mut constants = [[0.0f32; 4]; 3];
        for (index, key) in ["falloffParms", "falloffBeginColor", "falloffEndColor"]
            .iter()
            .enumerate()
        {
            let values = material_constants
                .get(*key)
                .and_then(Value::as_array)
                .ok_or_else(|| format!("Missing material {key}"))?;
            constants[index] =
                std::array::from_fn(|component| values[component].as_f64().unwrap() as f32);
        }
        Some(constants)
    } else {
        None
    };
    let eye_offset = if pass.contains("eyeofs") {
        let values = material_constants
            .get("eyeOffsetParms")
            .and_then(Value::as_array)
            .ok_or("Missing material eyeOffsetParms")?;
        Some(std::array::from_fn(|index| {
            values[index].as_f64().unwrap() as f32
        }))
    } else {
        None
    };
    let distortion_scale = if pass == "distortion_scale_zfeather_dtex" {
        let values = material_constants
            .get("distortionScale")
            .and_then(Value::as_array)
            .ok_or("Missing material distortionScale")?;
        Some(std::array::from_fn(|index| {
            values[index].as_f64().unwrap() as f32
        }))
    } else {
        None
    };
    let (vertex_program, pixel_program) = match pass.as_str() {
        "distortion_scale_zfeather_dtex" => ("vs_3_0_39fff4de", "ps_3_0_25f62fed"),
        "particle_cloud" => ("vs_3_0_319704f6", "ps_3_0_1d8b95b0"),
        "particle_cloud_add" => ("vs_3_0_f0f1e7e8", "ps_3_0_b9f1c6ae"),
        "vertcol_simple_dtex" => ("vs_3_0_a74fc43a", "ps_3_0_9532ed6e"),
        "effect_add_dtex" => ("vs_3_0_a74fc43a", "ps_3_0_df524070"),
        "zfeather_dtex" => ("vs_3_0_fcd09bfa", "ps_3_0_63c6a06e"),
        "zfeather_add_dtex" => ("vs_3_0_fcd09bfa", "ps_3_0_efa11d30"),
        "zfeather_fog_dtex" => ("vs_3_0_9299b36d", "ps_3_0_61596419"),
        "zfeather_add_fog_dtex" => ("vs_3_0_9299b36d", "ps_3_0_b49c9507"),
        "vertcol_simple_fog_dtex" => ("vs_3_0_fb69530b", "ps_3_0_e2f1a87f"),
        "effect_add_fog_dtex" => ("vs_3_0_fb69530b", "ps_3_0_2e21fe61"),
        "effect_add_eyeofs_dtex" => ("vs_3_0_b99ffff7", "ps_3_0_df524070"),
        "effect_add_eyeofs_fog_dtex" => ("vs_3_0_a3068f86", "ps_3_0_2e21fe61"),
        "zfeather_add_eyeofs_dtex" => ("vs_3_0_69ba296f", "ps_3_0_efa11d30"),
        "effect_add_eyeofs_falloff_dtex" => ("vs_3_0_40ba1880", "ps_3_0_df524070"),
        "effect_add_eyeofs_falloff_fog_dtex" => ("vs_3_0_97b797b1", "ps_3_0_2e21fe61"),
        "effect_add_falloff_fog_dtex" => ("vs_3_0_45d7ea7c", "ps_3_0_2e21fe61"),
        "zfeather_add_falloff_dtex" => ("vs_3_0_fda1502d", "ps_3_0_efa11d30"),
        "zfeather_add_falloff_fog_dtex" => ("vs_3_0_be1f061a", "ps_3_0_b49c9507"),
        "zfeather_falloff_fog_dtex" => ("vs_3_0_0dd75ebb", "ps_3_0_61596419"),
        _ => unreachable!(),
    };
    let additive_blend = destination == 2 && [2, 10].contains(&source);
    let (alpha_source, alpha_destination) = if additive_blend {
        (1, 2)
    } else if state_name == "default" {
        (8, 2)
    } else {
        (source, destination)
    };
    let cloud_sort = cloud_sort_identity(&digest(&bytes));
    let metadata = json!({
        "verifiedCloudSortGroup": cloud_sort.as_ref().map(|value| &value.0),
        "verifiedCloudMaterialRank": cloud_sort.as_ref().map(|value| value.1),
        "atlasColumns": bytes[11], "atlasRows": bytes[10], "primarySortKey": bytes[9],
        "materialSha256": digest(&bytes), "word": word, "secondWord": second_word,
        "family": family, "techsetSha256": digest(&techset), "pass": pass,
        "techniqueSha256": technique_hash, "stateMap": state_name,
        "authoredPass": authored_pass,
        "authoredTechniqueSha256": authored_outdoor_hash.unwrap_or(technique_hash),
        "effectivePass": pass,
        "effectiveTechniqueSha256": technique_hash,
        "outdoorPolicy": {
            "rOutdoor": 0,
            "nativeDisableFlag": 2,
            "nativeTechniqueRemapper": "0x484dc0",
            "remapped": authored_outdoor_hash.is_some(),
            "environment": "native r_outdoor=0, no world outdoor occlusion",
            "limit": "Explicit empty-view configuration, not the editor's default r_outdoor=1 and not map occlusion"
        },
        "stateMapSha256": state_hash, "colorMap": image,
        "sourceBlend": source, "destinationBlend": destination, "blendOperation": operation,
        "alphaTest": alpha_test, "alphaSource": alpha_source, "alphaDestination": alpha_destination,
        "cullModeD3D9": cull_mode, "sampler": sampler,
        "distortionScale": distortion_scale,
        "verifiedDistortionRank": if distortion_scale.is_some() {
            distortion_rank(name, &digest(&bytes))?
        } else {
            Value::Null
        },
        "depthStencil": "Owned D24S8 cleared depth1/stencil0; native EQUAL/KEEP; zero polygon bias preview",
        "vertexProgram": vertex_program, "pixelProgram": pixel_program,
        "materialConstants": material_constants, "depthFeather": feather.is_some(),
        "previewEnvironment": "Empty scene; original float-Z clear 2000000; fog transmittance explicitly 1; native r_outdoor=0, no world outdoor occlusion",
        "limit": "Material/pass preflight. IWD image availability and GPU upload are checked during render."
    });
    Ok(Material {
        metadata,
        image,
        additive_shader,
        cloud,
        vertex_program: vertex_program.to_owned(),
        pixel_program: pixel_program.to_owned(),
        feather,
        source,
        destination,
        operation,
        alpha_test,
        alpha_source,
        alpha_destination,
        cull_mode,
        falloff,
        eye_offset,
        distortion_scale,
        second_word,
        sampler,
    })
}

pub(crate) fn material_info(game: &Path, effect: &Value) -> Result<Value> {
    let mut names = BTreeSet::new();
    for element in effect["elements"]
        .as_array()
        .ok_or("Missing effect elements")?
    {
        let disabled = element["editorFlags"].as_array().is_some_and(|values| {
            values
                .iter()
                .any(|value| value.as_str() == Some("disabled"))
        });
        if disabled {
            continue;
        }
        // Point lights have a scene-light record, not a material/image list.
        if element.get("light").is_some() {
            continue;
        }
        for kind in [
            "billboardSprite",
            "orientedSprite",
            "tail",
            "line",
            "trail",
            "cloud",
        ] {
            if let Some(values) = element[kind].as_array() {
                for value in values {
                    if let Some(text) = value.as_str() {
                        if !["{", "}", ";"].contains(&text) {
                            names.insert(text.trim_matches('"').to_owned());
                        }
                    }
                }
            }
        }
    }
    let mut info = serde_json::Map::new();
    for name in names {
        let resolved =
            material(game, &name).map_err(|error| format!("Material {name}: {error}"))?;
        info.insert(name.clone(), resolved.metadata);
    }
    Ok(Value::Object(info))
}

struct LoadedMaterial {
    material: Material,
    texture: IDirect3DTexture9,
    image_evidence: Value,
}

#[derive(Clone)]
struct ImageSource {
    archive: PathBuf,
    entry: String,
}

#[derive(Clone, PartialEq, Eq)]
struct ArchiveStamp {
    length: u64,
    modified: SystemTime,
}

impl ArchiveStamp {
    fn read(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path)?;
        Ok(Self {
            length: metadata.len(),
            modified: metadata.modified()?,
        })
    }
}

struct OpenArchive {
    path: PathBuf,
    stamp: ArchiveStamp,
    reader: zip::ZipArchive<fs::File>,
}

struct CachedImage {
    archive: PathBuf,
    entry: String,
    stamp: ArchiveStamp,
    bytes: Arc<[u8]>,
}

/// Browsing commonly reuses a few IWDs and texture sheets. Keep their readers
/// and original bytes bounded in memory, not extracted copies on disk. Source
/// stamps are rechecked before a hit so a changed archive is never reused.
#[derive(Default)]
struct ImageCache {
    archives: VecDeque<OpenArchive>,
    images: VecDeque<CachedImage>,
    retained_bytes: usize,
}

impl ImageCache {
    fn read(&mut self, source: &ImageSource) -> Result<Arc<[u8]>> {
        const MAX_OPEN_ARCHIVES: usize = 4;
        const MAX_RETAINED_IMAGES: usize = 64;
        const MAX_RETAINED_BYTES: usize = 32 * 1024 * 1024;
        const MAX_IMAGE_BYTES: u64 = 128 * 1024 * 1024;

        let stamp = ArchiveStamp::read(&source.archive)?;
        if let Some(index) = self
            .images
            .iter()
            .position(|image| image.archive == source.archive && image.entry == source.entry)
        {
            let image = self
                .images
                .remove(index)
                .ok_or("Image cache entry disappeared")?;
            self.retained_bytes = self.retained_bytes.saturating_sub(image.bytes.len());
            if image.stamp == stamp {
                let bytes = image.bytes.clone();
                self.retained_bytes += image.bytes.len();
                self.images.push_back(image);
                return Ok(bytes);
            }
        }

        let cached_archive = self
            .archives
            .iter()
            .position(|archive| archive.path == source.archive)
            .and_then(|index| self.archives.remove(index));
        let mut archive = match cached_archive {
            Some(archive) if archive.stamp == stamp => archive,
            _ => OpenArchive {
                path: source.archive.clone(),
                stamp: stamp.clone(),
                reader: zip::ZipArchive::new(fs::File::open(&source.archive)?)?,
            },
        };
        let bytes = {
            let mut member = archive.reader.by_name(&source.entry)?;
            if member.size() > MAX_IMAGE_BYTES {
                return Err("IWI image exceeds the preview read limit".into());
            }
            let mut bytes = Vec::with_capacity(member.size() as usize);
            member
                .by_ref()
                .take(MAX_IMAGE_BYTES + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_IMAGE_BYTES {
                return Err("IWI image exceeds the preview read limit".into());
            }
            Arc::<[u8]>::from(bytes)
        };
        self.archives.push_back(archive);
        while self.archives.len() > MAX_OPEN_ARCHIVES {
            self.archives.pop_front();
        }

        if bytes.len() <= MAX_RETAINED_BYTES {
            self.retained_bytes += bytes.len();
            self.images.push_back(CachedImage {
                archive: source.archive.clone(),
                entry: source.entry.clone(),
                stamp,
                bytes: bytes.clone(),
            });
            while self.retained_bytes > MAX_RETAINED_BYTES
                || self.images.len() > MAX_RETAINED_IMAGES
            {
                if let Some(image) = self.images.pop_front() {
                    self.retained_bytes = self.retained_bytes.saturating_sub(image.bytes.len());
                }
            }
        }
        Ok(bytes)
    }
}

fn normalized_member(name: &str) -> String {
    name.replace('\\', "/").to_lowercase()
}

/// Match the native-priority lookup already verified by the Model Viewer:
/// ordinary archives first, descending case-insensitive names within each group.
/// The first normalized image entry wins. This is an independent research
/// implementation; the sibling app is not a build/runtime dependency.
fn image_sources(archives: &mut [PathBuf]) -> Result<HashMap<String, ImageSource>> {
    archives.sort_by(|first, second| {
        let first = first
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        let second = second
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        first
            .starts_with("localized_")
            .cmp(&second.starts_with("localized_"))
            .then_with(|| second.cmp(&first))
    });
    let mut sources = HashMap::new();
    for path in archives {
        let mut archive = zip::ZipArchive::new(fs::File::open(&path)?)?;
        for index in 0..archive.len() {
            let member = archive.by_index(index)?;
            let key = normalized_member(member.name());
            if member.is_dir() || !key.starts_with("images/") || !key.ends_with(".iwi") {
                continue;
            }
            sources.entry(key).or_insert_with(|| ImageSource {
                archive: path.clone(),
                entry: member.name().to_owned(),
            });
        }
    }
    Ok(sources)
}

pub(crate) struct Renderer {
    device: IDirect3DDevice9,
    declaration: IDirect3DVertexDeclaration9,
    cloud_declaration: IDirect3DVertexDeclaration9,
    model_declaration: IDirect3DVertexDeclaration9,
    vertex_shaders: HashMap<String, IDirect3DVertexShader9>,
    pixel_shaders: HashMap<String, IDirect3DPixelShader9>,
    float_z: IDirect3DTexture9,
    resolved_post_sun: IDirect3DTexture9,
    resolved_post_sun_surface: IDirect3DSurface9,
    target: IDirect3DSurface9,
    _depth_stencil: IDirect3DSurface9,
    output: IDirect3DSurface9,
    materials: HashMap<String, LoadedMaterial>,
    size: u32,
    shaders: Value,
    game: PathBuf,
    sources: HashMap<String, ImageSource>,
    images: ImageCache,
    preview_plane: RefCell<Option<plane_render::Resources>>,
    presentation_background: Cell<u32>,
}

fn shader(game: &Path, name: &str, expected: &str) -> Result<Vec<u32>> {
    let bytes = pinned_file(&game.join("raw/shader_bin").join(name), expected)?;
    if bytes.len() < 8 || (bytes.len() - 4) % 4 != 0 {
        return Err("Invalid shader cache wrapper".into());
    }
    Ok(bytes[4..]
        .chunks_exact(4)
        .map(|value| u32::from_le_bytes(value.try_into().unwrap()))
        .collect())
}

/// Apply the same whole-effect ordering admission before catalogue display and
/// before GPU loading. Parsed metadata is sufficient; no device is required.
pub(crate) fn validate_material_manifest(frames: &[Value], metadata: &Value) -> Result<()> {
    let names: BTreeSet<&str> = frames
        .iter()
        .filter_map(|frame| frame["draws"].as_array())
        .flatten()
        .filter_map(|draw| draw["texture"].as_str())
        .collect();
    let mut models = Vec::new();
    let mut blended = false;
    for name in names {
        if !name.starts_with("__model__/") {
            continue;
        }
        let material = &metadata[name];
        let word = material["word"].as_u64().ok_or("Missing model state")? as u32;
        blended |= crate::fx::material_states::operation(word, "default")?.blend;
        models.push(model_order::ModelOrderInput {
            index: models.len(),
            material: name.to_owned(),
            primary: material["primarySortKey"]
                .as_u64()
                .ok_or("Missing model primary key")? as u32,
            verified_material_rank: None,
        });
    }
    if blended {
        model_order::ordered_indices(&models)?;
    }

    let mut cloud_peers: HashMap<u64, BTreeSet<&str>> = HashMap::new();
    for draw in frames
        .iter()
        .filter_map(|frame| frame["draws"].as_array())
        .flatten()
    {
        if draw["vertexFormat"] != "cloud20" {
            continue;
        }
        let name = draw["texture"].as_str().ok_or("Missing cloud material")?;
        let primary = metadata[name]["primarySortKey"]
            .as_u64()
            .ok_or("Missing cloud primary key")?;
        cloud_peers.entry(primary).or_default().insert(name);
    }
    for names in cloud_peers.values().filter(|names| names.len() > 1) {
        let groups: BTreeSet<Option<&str>> = names
            .iter()
            .map(|name| metadata[*name]["verifiedCloudSortGroup"].as_str())
            .collect();
        if groups.len() != 1 || groups.contains(&None) {
            return Err("Cloud material peers are outside the verified ordering groups".into());
        }
    }
    Ok(())
}

impl Renderer {
    /// Validate and load a complete clip before playback. The installed files
    /// are only read; renderer resources stay on the owner UI thread.
    pub(crate) unsafe fn load_frames(&mut self, frames: &[Value]) -> Result<()> {
        let names: BTreeSet<String> = frames
            .iter()
            .filter_map(|frame| frame["draws"].as_array())
            .flatten()
            .filter_map(|draw| draw["texture"].as_str().map(str::to_owned))
            .collect();
        // A selection owns its material set. Clearing bounds GPU memory across
        // browsing; thumbnails never accumulate the entire library on the GPU.
        self.materials.clear();
        let game = self.game.clone();
        for name in names {
            self.load_material(&game, &name)?;
        }
        let metadata = Value::Object(
            self.materials
                .iter()
                .map(|(name, loaded)| (name.clone(), loaded.material.metadata.clone()))
                .collect(),
        );
        validate_material_manifest(frames, &metadata)
    }

    pub(crate) unsafe fn device_state(&self) -> u32 {
        match self.device.TestCooperativeLevel() {
            Ok(()) => 0,
            Err(error) => error.code().0 as u32,
        }
    }

    pub(crate) unsafe fn present(&self, width: u32, height: u32) -> Result<()> {
        let backbuffer = self.device.GetBackBuffer(0, 0, D3DBACKBUFFER_TYPE_MONO)?;
        self.device.SetDepthStencilSurface(None)?;
        self.device.SetRenderTarget(0, &backbuffer)?;
        self.device.Clear(
            0,
            null(),
            D3DCLEAR_TARGET as u32,
            self.presentation_background.get(),
            1.0,
            0,
        )?;
        // Preserve the verified square camera. A rectangular pane letterboxes
        // the render instead of stretching geometry or changing its projection.
        let side = width.min(height) as i32;
        let left = (width as i32 - side) / 2;
        let top = (height as i32 - side) / 2;
        let destination = windows::Win32::Foundation::RECT {
            left,
            top,
            right: left + side,
            bottom: top + side,
        };
        self.device.StretchRect(
            &self.target,
            null(),
            &backbuffer,
            &destination,
            D3DTEXF_LINEAR,
        )?;
        self.device
            .Present(null(), null(), HWND::default(), null())?;
        self.device.SetRenderTarget(0, &self.target)?;
        self.device.SetDepthStencilSurface(&self._depth_stencil)?;
        Ok(())
    }

    pub(crate) fn materials_loaded(&self) -> usize {
        self.materials.len()
    }

    pub(crate) unsafe fn new(
        window: HWND,
        width: u32,
        height: u32,
        size: u32,
        game: &Path,
    ) -> Result<Self> {
        let factory = Direct3DCreate9(D3D_SDK_VERSION).ok_or("Direct3DCreate9 failed")?;
        let mut parameters = D3DPRESENT_PARAMETERS {
            BackBufferWidth: width,
            BackBufferHeight: height,
            BackBufferFormat: D3DFMT_A8R8G8B8,
            BackBufferCount: 1,
            SwapEffect: D3DSWAPEFFECT_DISCARD,
            hDeviceWindow: window,
            Windowed: true.into(),
            PresentationInterval: D3DPRESENT_INTERVAL_IMMEDIATE as u32,
            ..Default::default()
        };
        let mut device = None;
        factory.CreateDevice(
            0,
            D3DDEVTYPE_HAL,
            window,
            (D3DCREATE_HARDWARE_VERTEXPROCESSING | D3DCREATE_FPU_PRESERVE) as u32,
            &mut parameters,
            &mut device,
        )?;
        let device = device.ok_or("No D3D9 device returned")?;
        let elements = [
            D3DVERTEXELEMENT9 {
                Stream: 0,
                Offset: 0,
                Type: 2,
                Method: 0,
                Usage: 0,
                UsageIndex: 0,
            },
            D3DVERTEXELEMENT9 {
                Stream: 0,
                Offset: 16,
                Type: 4,
                Method: 0,
                Usage: 10,
                UsageIndex: 0,
            },
            // UBYTE4, not FLOAT16_2: native shader reconstructs packed half UVs.
            D3DVERTEXELEMENT9 {
                Stream: 0,
                Offset: 20,
                Type: 5,
                Method: 0,
                Usage: 5,
                UsageIndex: 0,
            },
            // Original falloff shaders explicitly declare UBYTE4 normal.
            // The native sprite/trail writer packs it at byte24, not byte12.
            D3DVERTEXELEMENT9 {
                Stream: 0,
                Offset: 24,
                Type: 5,
                Method: 0,
                Usage: 3,
                UsageIndex: 0,
            },
            // Native distortion uses the packed tangent as UBYTE4 TEXCOORD1.
            D3DVERTEXELEMENT9 {
                Stream: 0,
                Offset: 28,
                Type: 5,
                Method: 0,
                Usage: 5,
                UsageIndex: 1,
            },
            D3DVERTEXELEMENT9 {
                Stream: 255,
                Offset: 0,
                Type: 17,
                Method: 0,
                Usage: 0,
                UsageIndex: 0,
            },
        ];
        let declaration = device.CreateVertexDeclaration(elements.as_ptr())?;
        // Native cloud buffer is POSITION FLOAT3 followed by TEXCOORD FLOAT2.
        // Unlike sprite/trail vertices, its UV coordinates are not packed halfs.
        let cloud_elements = [
            D3DVERTEXELEMENT9 {
                Stream: 0,
                Offset: 0,
                Type: 2,
                Method: 0,
                Usage: 0,
                UsageIndex: 0,
            },
            D3DVERTEXELEMENT9 {
                Stream: 0,
                Offset: 12,
                Type: 1,
                Method: 0,
                Usage: 5,
                UsageIndex: 0,
            },
            D3DVERTEXELEMENT9 {
                Stream: 255,
                Offset: 0,
                Type: 17,
                Method: 0,
                Usage: 0,
                UsageIndex: 0,
            },
        ];
        let cloud_declaration = device.CreateVertexDeclaration(cloud_elements.as_ptr())?;
        // Original model unlit binding, created by native 4535B0 layout 1.
        // The shader consumes position.xyz, BGRA and packed UV bytes.
        let model_elements = [
            D3DVERTEXELEMENT9 {
                Stream: 0,
                Offset: 0,
                Type: 3,
                Method: 0,
                Usage: 0,
                UsageIndex: 0,
            },
            D3DVERTEXELEMENT9 {
                Stream: 0,
                Offset: 16,
                Type: 4,
                Method: 0,
                Usage: 10,
                UsageIndex: 0,
            },
            D3DVERTEXELEMENT9 {
                Stream: 0,
                Offset: 20,
                Type: 5,
                Method: 0,
                Usage: 5,
                UsageIndex: 0,
            },
            D3DVERTEXELEMENT9 {
                Stream: 255,
                Offset: 0,
                Type: 17,
                Method: 0,
                Usage: 0,
                UsageIndex: 0,
            },
        ];
        let model_declaration = device.CreateVertexDeclaration(model_elements.as_ptr())?;
        let specifications = [
            (
                "vs_3_0_e7b86539",
                "a56e8c2ce72e4f970587b8465cf48a030ae72d49d3308c15dce4ea4b1f1f20e8",
            ),
            (
                "vs_3_0_a74fc43a",
                "01c5974d666e1db7997a25125bf1a9801e97879177e0e5108f84d931141e3acc",
            ),
            (
                "ps_3_0_f458b77f",
                "1d22c3a83fdaef33e8cd3f1325e01d4090bae1ea65f3b736e599fae8d0486363",
            ),
            (
                "ps_3_0_0b9c522e",
                "b915f87c1594f449346d3d0b0ce3002b1680da1e4892848e5c69a8af967a59b1",
            ),
            (
                "vs_3_0_39fff4de",
                "c61372c43a6f7dfe5058c86adb1a3ffe1b69a4d6a0b29876cf572147fd18fd36",
            ),
            (
                "ps_3_0_25f62fed",
                "ec35f082275c5308ffdc337f4cab513a9bcfbb92a831c53b3c5de52e5bcc6446",
            ),
            (
                "vs_3_0_319704f6",
                "027da4c94e6c078220cf45f58c87072f467e54fc0322c5812100d5c2d5bfb1e4",
            ),
            (
                "vs_3_0_f0f1e7e8",
                "027da4c94e6c078220cf45f58c87072f467e54fc0322c5812100d5c2d5bfb1e4",
            ),
            (
                "ps_3_0_1d8b95b0",
                "b039d763a26a34ada465b421178a3a0dc02981e09817dfea983e87d73becc5cd",
            ),
            (
                "ps_3_0_b9f1c6ae",
                "53bdf5cf4c6b221f9143f99760894ec60258d0495a20bfbc38d3d6112db78513",
            ),
            (
                "vs_3_0_40ba1880",
                "998018ada9f6cbd4807911f70aca806b6b6f2d3968af81b6904b85584967975a",
            ),
            (
                "vs_3_0_97b797b1",
                "f546a3c71a347e2c0e4fa85fd52fbdd248f6b9bbee95f497930480ed10ff82ee",
            ),
            (
                "vs_3_0_45d7ea7c",
                "aba59dde21f745d39194fb66b14ffb83a1198d70eddf7342c39ff7eedccc5f82",
            ),
            (
                "vs_3_0_fda1502d",
                "479633cbc220dbde630614f592f87c33a75577945576a4c727f30fc7745e6404",
            ),
            (
                "vs_3_0_be1f061a",
                "2ffa95832e7b603501f292545b51866694d28f5a8c5d0c3512f1ce541d29a9c4",
            ),
            (
                "vs_3_0_0dd75ebb",
                "155726876fb5d986ddc37b661d81aadc3ebbcac7ef92aab8d943a53043d97da3",
            ),
            (
                "vs_3_0_b99ffff7",
                "1a069e0b903cc590ef8dafb7eabca100c10d5869c8a278a2df0626fd3f7ccf98",
            ),
            (
                "vs_3_0_a3068f86",
                "4c0f117785c7111a1441d4f95787ccaf424b257d153233cf61c6f8bb2f8040d5",
            ),
            (
                "vs_3_0_69ba296f",
                "db1826a45ae8727c2fa2c9c00d32cfe8eefc80d47eb63cd2ac27346ec3df73f7",
            ),
            (
                "ps_3_0_2e21fe61",
                "df6d92472056b75cbb6e34e30b70a56ca676f6bd3d065df3889e18361e4c83fa",
            ),
            (
                "ps_3_0_61596419",
                "ded036f4968d7caa9a18573b77889b1dac4f86ec05c63fdee1d550eb04e82d6f",
            ),
            (
                "ps_3_0_b49c9507",
                "a9c8a6b0d2b69292b20808c67e36c0385e8bcab252441e886f6d4c15c7fb22f8",
            ),
            (
                "ps_3_0_e2f1a87f",
                "6e60442d7c069309d7fd4d73be6d072844a5a34123bacc61e66efd371a7b4355",
            ),
            (
                "vs_3_0_9299b36d",
                "4c752305b82c128748d97036e905f6c03446b9c19ab398a5e3293262e57515fc",
            ),
            (
                "vs_3_0_fb69530b",
                "98c7b1ac1b7347a75e6c239cdecbc24369845e18c2f3e6212563f76f06f7fd22",
            ),
            (
                "vs_3_0_fcd09bfa",
                "e52fd3626636c0f5177d60a6e1424cc176779138675668736cdf9a9048eabf12",
            ),
            (
                "ps_3_0_efa11d30",
                "e1a09a4d1f6359051b83d674d0aa40efabc5f86fbfadb70e5d1261e775ea6cba",
            ),
            (
                "ps_3_0_63c6a06e",
                "96d7764d2192988d8d91147311544fe861293b7a1be71cfefc365f440c13d902",
            ),
            (
                "vs_3_0_a74fc43a",
                "01c5974d666e1db7997a25125bf1a9801e97879177e0e5108f84d931141e3acc",
            ),
            (
                "ps_3_0_9532ed6e",
                "dcd24143834dff0a53672f5a42b336f6d97f1152a0938621562d4b178c1894ca",
            ),
            (
                "ps_3_0_df524070",
                "c9dc3940c0c7ebc58aa31f0debf425a806f7ca3cbdc5c9d8ada441b697470d71",
            ),
        ];
        let words = specifications
            .iter()
            .map(|(name, hash)| shader(game, name, hash))
            .collect::<Result<Vec<_>>>()?;
        let mut vertex_shaders = HashMap::new();
        let mut pixel_shaders = HashMap::new();
        for ((name, _), words) in specifications.iter().zip(&words) {
            if name.starts_with("vs_") {
                vertex_shaders.insert(name.to_string(), device.CreateVertexShader(words.as_ptr())?);
            } else {
                pixel_shaders.insert(name.to_string(), device.CreatePixelShader(words.as_ptr())?);
            }
        }
        // The installed floatz_clear.hlsl shader writes exactly 2,000,000.
        // Its disassembly/hash are recorded in shader-checks, rather than
        // substituting white for an unknown depth encoding. The empty preview
        // contains no receiving geometry: this constant texture is its cleared
        // float-Z surface. A scene with occluders needs a real depth prepass.
        let mut float_z = None;
        device.CreateTexture(
            1,
            1,
            1,
            0,
            D3DFMT_R32F,
            D3DPOOL_MANAGED,
            &mut float_z,
            null_mut(),
        )?;
        let float_z = float_z.ok_or("No float-Z texture returned")?;
        let mut depth_lock = D3DLOCKED_RECT::default();
        float_z.LockRect(0, &mut depth_lock, null(), 0)?;
        copy_nonoverlapping(
            2_000_000.0f32.to_le_bytes().as_ptr(),
            depth_lock.pBits.cast(),
            4,
        );
        float_z.UnlockRect(0)?;
        for (state, value) in [(1, 3), (2, 3), (5, 1), (6, 1), (7, 0), (11, 0)] {
            device.SetSamplerState(4, D3DSAMPLERSTATETYPE(state), value)?;
        }
        for (state, value) in [(7, 0), (14, 0), (22, 1), (137, 0), (194, 0)] {
            device.SetRenderState(D3DRENDERSTATETYPE(state), value)?;
        }
        for (state, value) in [(1, 3), (2, 3), (5, 2), (6, 2), (7, 0), (11, 0)] {
            device.SetSamplerState(0, D3DSAMPLERSTATETYPE(state), value)?;
        }
        let mut depth_stencil = None;
        device.CreateDepthStencilSurface(
            size,
            size,
            D3DFMT_D24S8,
            D3DMULTISAMPLE_NONE,
            0,
            true,
            &mut depth_stencil,
            null_mut(),
        )?;
        let depth_stencil = depth_stencil.ok_or("No depth/stencil surface returned")?;
        device.SetDepthStencilSurface(&depth_stencil)?;
        // Explicit empty-scene environment. Captured native setters use EQUAL
        // and KEEP; reference0/full masks match original D3D defaults. The
        // original preview clear paths request depth1/stencil0. Bias is zero.
        for (state, value) in [
            (57, 0),
            (58, u32::MAX),
            (59, u32::MAX),
            (185, 0),
            (53, 1),
            (54, 1),
            (55, 1),
            (186, 1),
            (187, 1),
            (188, 1),
            (175, 0),
            (195, 0),
        ] {
            device.SetRenderState(D3DRENDERSTATETYPE(state), value)?;
        }
        let mut target = None;
        device.CreateRenderTarget(
            size,
            size,
            D3DFMT_A8R8G8B8,
            D3DMULTISAMPLE_NONE,
            0,
            false,
            &mut target,
            null_mut(),
        )?;
        let target = target.ok_or("No FX render target returned")?;
        device.SetRenderTarget(0, &target)?;
        let mut resolved_post_sun = None;
        device.CreateTexture(
            size,
            size,
            1,
            D3DUSAGE_RENDERTARGET as u32,
            D3DFMT_A8R8G8B8,
            D3DPOOL_DEFAULT,
            &mut resolved_post_sun,
            null_mut(),
        )?;
        let resolved_post_sun = resolved_post_sun.ok_or("No scene-copy texture returned")?;
        let resolved_post_sun_surface = resolved_post_sun.GetSurfaceLevel(0)?;
        let mut output = None;
        device.CreateOffscreenPlainSurface(
            size,
            size,
            D3DFMT_A8R8G8B8,
            D3DPOOL_SYSTEMMEM,
            &mut output,
            null_mut(),
        )?;
        let mut archives = fs::read_dir(game.join("main"))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        archives.retain(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("iwd"))
        });
        let sources = image_sources(&mut archives)?;
        Ok(Self {
            device,
            declaration,
            cloud_declaration,
            model_declaration,
            vertex_shaders,
            pixel_shaders,
            float_z,
            resolved_post_sun,
            resolved_post_sun_surface,
            target,
            _depth_stencil: depth_stencil,
            output: output.ok_or("No output surface returned")?,
            materials: HashMap::new(),
            size,
            shaders: Value::Object(
                specifications
                    .iter()
                    .map(|(name, hash)| (name.to_string(), json!(hash)))
                    .collect(),
            ),
            game: game.to_owned(),
            sources,
            images: ImageCache::default(),
            preview_plane: RefCell::new(None),
            presentation_background: Cell::new(background::DARK),
        })
    }

    unsafe fn load_material(&mut self, game: &Path, name: &str) -> Result<()> {
        if self.materials.contains_key(name) {
            return Ok(());
        }
        let material = material(game, name)?;
        self.load_resolved_material(name, material)
    }

    unsafe fn load_resolved_material(&mut self, name: &str, material: Material) -> Result<()> {
        if self.materials.contains_key(name) {
            return Ok(());
        }
        let member = normalized_member(&format!("images/{}.iwi", material.image));
        // Use the indexed original spelling when opening the ZIP entry. Native
        // lookup is case/slash insensitive, whereas ZipArchive::by_name is exact.
        let source = self
            .sources
            .get(&member)
            .ok_or_else(|| format!("Image missing from main/*.iwd: {member}"))?
            .clone();
        let bytes = self.images.read(&source)?;
        if bytes.len() < 28 || bytes[..4] != *b"IWi\x06" {
            return Err(format!("Unsupported IWI version: {member}").into());
        }
        let width = u16::from_le_bytes(bytes[6..8].try_into()?) as u32;
        let height = u16::from_le_bytes(bytes[8..10].try_into()?) as u32;
        let depth = u16::from_le_bytes(bytes[10..12].try_into()?);
        if width == 0 || height == 0 || depth != 1 || bytes[5] & 4 != 0 {
            return Err("Only ordinary 2D IWI images are supported in this research path".into());
        }
        if bytes[4] == 9 {
            let levels = wavelet_luminance_levels(&bytes)?;
            let mut texture = None;
            self.device.CreateTexture(
                width,
                height,
                levels.len() as u32,
                0,
                D3DFMT_L8,
                D3DPOOL_MANAGED,
                &mut texture,
                null_mut(),
            )?;
            let texture = texture.ok_or("No luminance texture returned")?;
            let mut mip_evidence = Vec::new();
            for (level, (mip_width, mip_height, pixels)) in levels.iter().enumerate() {
                let mut locked = D3DLOCKED_RECT::default();
                texture.LockRect(level as u32, &mut locked, null(), 0)?;
                if locked.Pitch < *mip_width as i32 {
                    texture.UnlockRect(level as u32)?;
                    return Err("Native luminance texture pitch is too small".into());
                }
                for row in 0..*mip_height as usize {
                    copy_nonoverlapping(
                        pixels.as_ptr().add(row * *mip_width as usize),
                        locked.pBits.cast::<u8>().add(row * locked.Pitch as usize),
                        *mip_width as usize,
                    );
                }
                texture.UnlockRect(level as u32)?;
                mip_evidence.push(json!({
                    "level": level, "width": mip_width, "height": mip_height,
                    "decodedLuminanceSha256": digest(pixels)
                }));
            }
            let image_evidence = json!({
                "archive": source.archive.file_name().unwrap().to_string_lossy(),
                "member": source.entry, "normalizedMember": member,
                "iwiSha256": digest(&bytes), "width": width, "height": height,
                "format": 9, "d3dFormat": "L8", "topMipOnly": false,
                "mipLevels": levels.len(), "decodedMips": mip_evidence,
                "samplingChannels": "RGB=luminance; alpha=1"
            });
            self.materials.insert(
                name.to_owned(),
                LoadedMaterial {
                    material,
                    texture,
                    image_evidence,
                },
            );
            return Ok(());
        }
        let (format, block) = match bytes[4] {
            2 => (D3DFMT_X8R8G8B8, 0usize),
            11 => (D3DFMT_DXT1, 8usize),
            12 => (D3DFMT_DXT3, 16usize),
            13 => (D3DFMT_DXT5, 16usize),
            _ => return Err(format!("Unsupported IWI encoding for {member}").into()),
        };
        // IWI stores original mips smallest first. Validate the complete chain
        // independently against the header before uploading original levels.
        let mut levels = Vec::new();
        let mut mip_width = width;
        let mut mip_height = height;
        loop {
            let row_bytes = if block == 0 {
                mip_width as usize * 3
            } else {
                mip_width.div_ceil(4) as usize * block
            };
            let rows = if block == 0 {
                mip_height as usize
            } else {
                mip_height.div_ceil(4) as usize
            };
            levels.push((mip_width, mip_height, row_bytes, rows));
            if (mip_width == 1 && mip_height == 1) || bytes[5] & 2 != 0 {
                break;
            }
            mip_width = (mip_width / 2).max(1);
            mip_height = (mip_height / 2).max(1);
        }
        let total_bytes: usize = levels.iter().map(|level| level.2 * level.3).sum();
        if total_bytes + 28 != bytes.len() {
            return Err("IWI mip chain length does not match its 2D dimensions".into());
        }
        // With NOMIPMAPS the picmip filesize fields repeat the full length;
        // there is no smaller mip prefix, so payload begins immediately at28.
        let start = if bytes[5] & 2 != 0 {
            28
        } else {
            u32_at(&bytes, 16)? as usize
        };
        let largest_size = levels[0].2 * levels[0].3;
        if start != bytes.len() - largest_size {
            return Err("IWI largest-mip offset disagrees with complete mip chain".into());
        }
        let top = &bytes[start..];
        let mut texture = None;
        self.device.CreateTexture(
            width,
            height,
            levels.len() as u32,
            0,
            format,
            D3DPOOL_MANAGED,
            &mut texture,
            null_mut(),
        )?;
        let texture = texture.ok_or("No texture returned")?;
        let mut end = bytes.len();
        for (level, (_, _, row_bytes, rows)) in levels.iter().enumerate() {
            let begin = end - row_bytes * rows;
            let mut locked = D3DLOCKED_RECT::default();
            texture.LockRect(level as u32, &mut locked, null(), 0)?;
            let upload_row_bytes = if block == 0 {
                row_bytes / 3 * 4
            } else {
                *row_bytes
            };
            if locked.Pitch < upload_row_bytes as i32 {
                texture.UnlockRect(level as u32)?;
                return Err("Native compressed texture pitch is too small".into());
            }
            for row in 0..*rows {
                let expanded;
                let source_row = if block == 0 {
                    expanded = raw_rgb_row(
                        &bytes[begin + row * row_bytes..begin + (row + 1) * row_bytes],
                    )?;
                    expanded.as_ptr()
                } else {
                    bytes.as_ptr().add(begin + row * row_bytes)
                };
                copy_nonoverlapping(
                    source_row,
                    locked.pBits.cast::<u8>().add(row * locked.Pitch as usize),
                    upload_row_bytes,
                );
            }
            texture.UnlockRect(level as u32)?;
            end = begin;
        }
        let image_evidence = json!({
            "archive": source.archive.file_name().unwrap().to_string_lossy(),
            "member": source.entry, "normalizedMember": member,
            "iwiSha256": digest(&bytes), "largestMipSha256": digest(top),
            "width": width, "height": height, "format": bytes[4], "mipOffset": start,
            "topMipOnly": false, "mipLevels": levels.len()
        });
        self.materials.insert(
            name.to_owned(),
            LoadedMaterial {
                material,
                texture,
                image_evidence,
            },
        );
        Ok(())
    }

    pub(crate) unsafe fn frame(&self, frame: &Value) -> Result<()> {
        let mut constants = Vec::new();
        for row in frame["matrixRows"]
            .as_array()
            .ok_or("Missing view projection rows")?
        {
            let row = row.as_array().ok_or("Invalid matrix row")?;
            if row.len() != 4 {
                return Err("Each matrix row must contain four numbers".into());
            }
            for value in row {
                let value = value.as_f64().ok_or("Invalid matrix value")? as f32;
                if !value.is_finite() {
                    return Err("Nonfinite matrix value".into());
                }
                constants.push(value);
            }
        }
        if constants.len() != 16 {
            return Err("Expected four view projection rows".into());
        }
        // Native world matrices are camera-relative. Split translation out of
        // the supplied world-to-clip rows so position projection is unchanged,
        // while the falloff shader receives the true eye-to-vertex vector.
        let mut eye = [0.0f32; 3];
        if let Some(values) = frame.get("cameraOrigin") {
            let values = values.as_array().ok_or("Invalid camera origin")?;
            if values.len() != 3 {
                return Err("Camera origin requires three coordinates".into());
            }
            for axis in 0..3 {
                eye[axis] = values[axis].as_f64().ok_or("Invalid camera coordinate")? as f32;
                if !eye[axis].is_finite() {
                    return Err("Nonfinite camera origin".into());
                }
            }
            for row in 0..4 {
                for axis in 0..3 {
                    constants[row * 4 + 3] += constants[row * 4 + axis] * eye[axis];
                }
            }
        } else if frame["draws"].as_array().is_some_and(|draws| {
            draws.iter().any(|draw| {
                draw["texture"]
                    .as_str()
                    .and_then(|name| self.materials.get(name))
                    .is_some_and(|loaded| loaded.material.falloff.is_some())
            })
        }) {
            return Err("Falloff shader requires explicit cameraOrigin".into());
        }
        constants.extend([
            1., 0., 0., -eye[0], 0., 1., 0., -eye[1], 0., 0., 1., -eye[2], 0., 0., 0., 1.,
        ]);
        self.device.SetVertexDeclaration(&self.declaration)?;
        self.device
            .SetVertexShaderConstantF(0, constants.as_ptr(), 8)?;
        let preview_background = background::Background::from_frame(frame)?;
        self.presentation_background
            .set(preview_background.base_colour());
        self.device.Clear(
            0,
            null(),
            (D3DCLEAR_TARGET | D3DCLEAR_ZBUFFER | D3DCLEAR_STENCIL) as u32,
            preview_background.base_colour(),
            1.,
            0,
        )?;
        preview_background.paint(&self.device, self.size)?;
        if frame["previewPlane"].is_object() {
            let mut resources = self.preview_plane.borrow_mut();
            if resources.is_none() {
                *resources = Some(plane_render::Resources::new(
                    &self.device,
                    &self.game,
                    self.size,
                )?);
            }
            // Opaque floor and its actual point lights precede the single
            // pre-emissive scene copy used by distortion particles.
            self.device.BeginScene()?;
            let result = resources.as_ref().unwrap().draw(&self.device, frame);
            self.device.EndScene()?;
            result?;
        }
        // Opaque model surfaces contribute real hardware depth and scene colour
        // before the emissive/distortion copy. Never append them as particles.
        let mut models: Vec<&Value> = frame["draws"]
            .as_array()
            .ok_or("Missing draws")?
            .iter()
            .filter(|draw| draw["vertexFormat"] == "model32")
            .collect();
        let mut has_blended_model = false;
        for draw in &models {
            let name = draw["texture"].as_str().ok_or("Missing model material")?;
            let loaded = self
                .materials
                .get(name)
                .ok_or("Model material not loaded")?;
            let word = loaded.material.metadata["word"]
                .as_u64()
                .ok_or("Missing model state word")? as u32;
            has_blended_model |= crate::fx::material_states::operation(word, "default")?.blend;
        }
        if has_blended_model {
            let rows = models
                .iter()
                .enumerate()
                .map(|(index, draw)| {
                    let name = draw["texture"].as_str().ok_or("Missing model material")?;
                    let metadata = &self
                        .materials
                        .get(name)
                        .ok_or("Model material not loaded")?
                        .material
                        .metadata;
                    Ok(model_order::ModelOrderInput {
                        index,
                        material: name.to_owned(),
                        primary: metadata["primarySortKey"]
                            .as_u64()
                            .ok_or("Missing model primary key")?
                            as u32,
                        verified_material_rank: metadata["verifiedModelMaterialRank"]
                            .as_u64()
                            .map(|rank| rank as u32),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let order = model_order::ordered_indices(&rows)?;
            models = order.into_iter().map(|index| models[index]).collect();
        }
        if !models.is_empty()
            && !frame["previewPlane"].is_object()
            && frame["draws"].as_array().unwrap().iter().any(|draw| {
                draw["texture"]
                    .as_str()
                    .and_then(|name| self.materials.get(name))
                    .is_some_and(|loaded| loaded.material.feather.is_some())
            })
        {
            return Err("Model/soft-particle overlap requires the verified full-resolution minimal-scene depth path".into());
        }
        if !models.is_empty() {
            self.device.BeginScene()?;
            let result = (|| -> Result<()> {
                for model in &models {
                    self.device
                        .SetVertexShaderConstantF(0, constants.as_ptr(), 8)?;
                    self.draw_frame(model, frame)?;
                }
                Ok(())
            })();
            self.device.EndScene()?;
            result?;
            if frame["previewPlane"].is_object() {
                self.preview_plane
                    .borrow()
                    .as_ref()
                    .ok_or("Missing receiver depth")?
                    .models_depth(
                        &self.device,
                        frame,
                        &self.model_declaration,
                        &self.materials,
                    )?;
            }
        }
        if frame["draws"].as_array().is_some_and(|draws| {
            draws.iter().any(|draw| {
                draw["texture"]
                    .as_str()
                    .and_then(|name| self.materials.get(name))
                    .is_some_and(|loaded| loaded.material.distortion_scale.is_some())
            })
        }) {
            // Native 0x4b6859..0x4b687b copies the scene before all emissive
            // particles. Do not recopy after each ordinary sprite. This empty
            // research scene contains its clear background only.
            self.copy_pre_emissive_scene()?;
        }
        self.device.BeginScene()?;
        let result = (|| -> Result<()> {
            let mut ordered_draws: Vec<&Value> = frame["draws"]
                .as_array()
                .ok_or("Missing draws")?
                .iter()
                .filter(|draw| draw["vertexFormat"] != "model32")
                .collect();
            let primary_48 = |draw: &&Value| {
                draw["texture"]
                    .as_str()
                    .and_then(|name| self.materials.get(name))
                    .is_some_and(|loaded| loaded.material.metadata["primarySortKey"] == 48)
            };
            if frame["nativeBackendOrder"].as_bool() != Some(true)
                && ordered_draws
                    .iter()
                    .any(|draw| draw["vertexFormat"] == "cloud20" && primary_48(draw))
                && ordered_draws
                    .iter()
                    .any(|draw| draw["vertexFormat"] != "cloud20" && primary_48(draw))
            {
                // Both share unsorted bucket13. Reproduce the original
                // frontend's material-change/final sprite-batch flush before
                // the independently proved backend primary-key merge.
                let draws = frame["draws"].as_array().ok_or("Missing draws")?;
                ordered_draws = frontend_enqueue_indices(draws)?
                    .into_iter()
                    .map(|index| &draws[index])
                    .collect();
            }
            if frame["nativeBackendOrder"].as_bool() != Some(true)
                && ordered_draws.iter().any(|draw| {
                    draw["vertexFormat"] == "cloud20"
                        || draw["texture"]
                            .as_str()
                            .and_then(|name| self.materials.get(name))
                            .is_some_and(|loaded| loaded.material.metadata["primarySortKey"] != 48)
                })
            {
                // Native bucket merging uses the primary material key. Equal
                // primary cloud surfaces also use runtime material indices.
                // Rank only byte-pinned groups proven by the native comparator;
                // unknown/cross-family groups retain a stated research limit.
                // Original bucket14 (primary24) merges before bucket13
                // (primary48); stable sorting preserves each bucket's order.
                // The independent sort-blockers proof exercises 120 permutations.
                let mut groups: HashMap<u64, BTreeSet<Option<String>>> = HashMap::new();
                for draw in &ordered_draws {
                    let name = draw["texture"].as_str().ok_or("Missing material")?;
                    let material = &self
                        .materials
                        .get(name)
                        .ok_or("Material not loaded")?
                        .material
                        .metadata;
                    let primary = material["primarySortKey"]
                        .as_u64()
                        .ok_or("Missing primary key")?;
                    let group = if draw["vertexFormat"] == "cloud20" {
                        material["verifiedCloudSortGroup"]
                            .as_str()
                            .map(str::to_owned)
                    } else {
                        None
                    };
                    groups.entry(primary).or_default().insert(group);
                }
                ordered_draws.sort_by(|left, right| {
                    let metadata = |draw: &Value| -> &Value {
                        &self.materials[draw["texture"].as_str().unwrap()]
                            .material
                            .metadata
                    };
                    let a = metadata(left);
                    let b = metadata(right);
                    let primary = a["primarySortKey"].as_u64().unwrap();
                    let first = primary.cmp(&b["primarySortKey"].as_u64().unwrap());
                    if first != std::cmp::Ordering::Equal {
                        return first;
                    }
                    if let (Some(left_rank), Some(right_rank)) = (
                        a["verifiedDistortionRank"].as_u64(),
                        b["verifiedDistortionRank"].as_u64(),
                    ) {
                        return left_rank.cmp(&right_rank);
                    }
                    let peers = &groups[&primary];
                    if peers.len() == 1 && !peers.contains(&None) {
                        return a["verifiedCloudMaterialRank"]
                            .as_u64()
                            .cmp(&b["verifiedCloudMaterialRank"].as_u64());
                    }
                    std::cmp::Ordering::Equal
                });
            }
            for draw in ordered_draws {
                // Cloud c4 holds object world-view; ordinary particles use c4
                // for camera-relative world. Restore the shared baseline before
                // every draw so mixed streams cannot inherit another layout.
                self.device
                    .SetVertexShaderConstantF(0, constants.as_ptr(), 8)?;
                self.draw_frame(draw, frame)?;
            }
            Ok(())
        })();
        self.device.EndScene()?;
        result
    }

    unsafe fn draw(&self, draw: &Value) -> Result<()> {
        self.draw_frame(draw, &Value::Null)
    }

    unsafe fn copy_pre_emissive_scene(&self) -> Result<()> {
        self.device.SetTexture(0, None::<&IDirect3DBaseTexture9>)?;
        self.device.StretchRect(
            &self.target,
            null(),
            &self.resolved_post_sun_surface,
            null(),
            D3DTEXF_LINEAR,
        )?;
        Ok(())
    }

    unsafe fn draw_frame(&self, draw: &Value, frame: &Value) -> Result<()> {
        let float_z = if frame["previewPlane"].is_object() {
            self.preview_plane
                .borrow()
                .as_ref()
                .ok_or("Preview plane depth was not rendered")?
                .float_z
                .clone()
        } else {
            self.float_z.clone()
        };
        let name = draw["texture"].as_str().ok_or("Missing material name")?;
        let loaded = self.materials.get(name).ok_or("Material not loaded")?;
        let material = &loaded.material;
        let cloud_stream = draw["vertexFormat"].as_str() == Some("cloud20");
        let model_stream = draw["vertexFormat"].as_str() == Some("model32");
        if cloud_stream != material.cloud {
            return Err("Native cloud stream and cloud material must be selected together".into());
        }
        self.device.SetVertexDeclaration(if cloud_stream {
            &self.cloud_declaration
        } else if model_stream {
            &self.model_declaration
        } else {
            &self.declaration
        })?;
        if model_stream {
            let placement: crate::fx::model_pose::Placement =
                serde_json::from_value(draw["modelPlacement"].clone())?;
            let eye: [f32; 3] = serde_json::from_value(frame["cameraOrigin"].clone())?;
            let world = crate::fx::model_pose::world_rows(&placement, eye);
            let constants: Vec<f32> = world.into_iter().flatten().collect();
            self.device
                .SetVertexShaderConstantF(4, constants.as_ptr(), 4)?;
            self.device
                .SetVertexShaderConstantF(21, [0.0f32, 0.0, 1.0, 0.0].as_ptr(), 1)?;
            self.device
                .SetPixelShaderConstantF(0, [0.0f32; 4].as_ptr(), 1)?;
            self.device
                .SetPixelShaderConstantF(1, [0.0f32; 4].as_ptr(), 1)?;
        }
        if cloud_stream {
            // The backend supplies the exact per-cloud constants recovered from
            // native 0x4B0980. Do not replace this path with sprite quads.
            let read_values = |value: &Value, count: usize| -> Result<Vec<f32>> {
                let values = value.as_array().ok_or("Missing native cloud constants")?;
                let flat: Vec<&Value> = values
                    .iter()
                    .flat_map(|v| {
                        if let Some(row) = v.as_array() {
                            row.iter().collect()
                        } else {
                            vec![v]
                        }
                    })
                    .collect();
                if flat.len() != count {
                    return Err("Wrong native cloud constant count".into());
                }
                flat.into_iter()
                    .map(|v| {
                        let number = v.as_f64().ok_or("Invalid native cloud constant")? as f32;
                        if !number.is_finite() {
                            return Err("Nonfinite native cloud constant".into());
                        }
                        Ok(number)
                    })
                    .collect()
            };
            let world_view = read_values(&draw["worldViewMatrixRows"], 16)?;
            let projection = read_values(&frame["projectionMatrixRows"], 16)?;
            let cloud_matrix = read_values(&draw["cloudMatrix"], 4)?;
            let cloud_colour = read_values(&draw["cloudColor"], 4)?;
            self.device
                .SetVertexShaderConstantF(4, world_view.as_ptr(), 4)?;
            self.device
                .SetVertexShaderConstantF(8, projection.as_ptr(), 4)?;
            self.device
                .SetVertexShaderConstantF(16, cloud_matrix.as_ptr(), 1)?;
            self.device
                .SetPixelShaderConstantF(3, cloud_colour.as_ptr(), 1)?;
        }
        let (alpha_compare, alpha_reference) = material.alpha_test.unwrap_or((8, 0));
        let operation = crate::fx::material_states::operation(
            material.metadata["word"]
                .as_u64()
                .ok_or("Missing material state word")? as u32,
            material.metadata["stateMap"]
                .as_str()
                .ok_or("Missing material state map")?,
        )?;
        for (state, value) in [
            (22, material.cull_mode),
            (7, u32::from(material.second_word & 2 == 0)),
            (14, u32::from(operation.depth_write)),
            (23, if material.second_word & 2 != 0 { 8 } else { 4 }),
            (52, u32::from(material.second_word & 0x40 != 0)),
            (
                56,
                if material.second_word & 0x40000 != 0 {
                    3
                } else {
                    1
                },
            ),
            (
                189,
                if material.second_word & 0x40000 != 0 {
                    3
                } else {
                    1
                },
            ),
            (27, u32::from(operation.blend)),
            (171, operation.operation),
            (19, material.source),
            (20, material.destination),
            (206, u32::from(operation.separate_alpha)),
            (207, material.alpha_source),
            (208, material.alpha_destination),
            (209, 1),
            (15, u32::from(material.alpha_test.is_some())),
            (25, alpha_compare),
            (24, alpha_reference),
        ] {
            self.device
                .SetRenderState(D3DRENDERSTATETYPE(state), value)?;
        }
        self.device.SetVertexShader(
            self.vertex_shaders
                .get(&material.vertex_program)
                .ok_or("Missing vertex shader")?,
        )?;
        self.device.SetPixelShader(
            self.pixel_shaders
                .get(&material.pixel_program)
                .ok_or("Missing pixel shader")?,
        )?;
        if let Some(falloff) = material.falloff {
            for (index, constant) in falloff.iter().enumerate() {
                self.device
                    .SetVertexShaderConstantF(13 + index as u32, constant.as_ptr(), 1)?;
            }
        }
        if let Some(eye_offset) = material.eye_offset {
            self.device
                .SetVertexShaderConstantF(16, eye_offset.as_ptr(), 1)?;
        }
        if let Some(feather) = material.feather {
            self.device
                .SetVertexShaderConstantF(12, feather.as_ptr(), 1)?;
            self.device
                .SetPixelShaderConstantF(5, feather.as_ptr(), 1)?;
            // Clip-space [-w,+w] -> UV [0,1], with D3D9 texel-centre offset.
            // The 1x1 cleared-depth input is constant, so lookup precision does
            // not affect this empty-view proof. Nonconstant depth is unverified.
            let scale = [0.5f32, -0.5, 1.0, 1.0];
            let offset = [
                0.5 + 0.5 / self.size as f32,
                0.5 + 0.5 / self.size as f32,
                0.0,
                0.0,
            ];
            self.device
                .SetVertexShaderConstantF(17, scale.as_ptr(), 1)?;
            self.device
                .SetVertexShaderConstantF(18, offset.as_ptr(), 1)?;
            self.device.SetTexture(4, &float_z)?;
            // Distortion uses slot4 for its normal map; restore the independent
            // float-Z sampler before an ordinary feathered particle.
            for (state, value) in [(1, 3), (2, 3), (5, 1), (6, 1), (7, 0), (11, 0)] {
                self.device
                    .SetSamplerState(4, D3DSAMPLERSTATETYPE(state), value)?;
            }
        }
        if let Some(distortion) = material.distortion_scale {
            if frame.get("matrixRows").is_some() {
                // The original distortion VS consumes world-space positions
                // directly through c0. It never applies ordinary sprites' c4
                // camera-relative transform, so retain the full view projection.
                let rows = frame["matrixRows"]
                    .as_array()
                    .ok_or("Missing distortion projection")?;
                let mut projection = Vec::with_capacity(16);
                for row in rows {
                    for component in row.as_array().ok_or("Invalid distortion projection row")? {
                        let value = component
                            .as_f64()
                            .ok_or("Invalid distortion projection value")?
                            as f32;
                        if !value.is_finite() {
                            return Err("Nonfinite distortion projection".into());
                        }
                        projection.push(value);
                    }
                }
                if projection.len() != 16 {
                    return Err("Distortion projection must contain 16 values".into());
                }
                self.device
                    .SetVertexShaderConstantF(0, projection.as_ptr(), 4)?;
            }
            self.device
                .SetVertexShaderConstantF(12, distortion.as_ptr(), 1)?;
            self.device
                .SetVertexShaderConstantF(17, [0.5f32, -0.5, 0.0, 1.0].as_ptr(), 1)?;
            self.device.SetVertexShaderConstantF(
                18,
                [
                    0.5f32 + 0.5 / self.size as f32,
                    0.5f32 + 0.5 / self.size as f32,
                    0.0,
                    0.0,
                ]
                .as_ptr(),
                1,
            )?;
            // Native emissive callback 0x4b5ea0 sets code constant 77 through
            // setter 0x47ae30. These are original full-frame mapping values.
            self.device
                .SetVertexShaderConstantF(20, [0.0f32, 0.0, 1.0, 1.0].as_ptr(), 1)?;
            self.device.SetTexture(0, &self.resolved_post_sun)?;
            self.device.SetTexture(5, &float_z)?;
            for (slot, filter) in [(0, 2), (5, 1)] {
                for (state, value) in [(1, 3), (2, 3), (5, filter), (6, filter), (7, 0), (11, 0)] {
                    self.device
                        .SetSamplerState(slot, D3DSAMPLERSTATETYPE(state), value)?;
                }
            }
        }
        // Fog is an explicit environment parameter of this empty research
        // viewport. This binding requests no fog, without switching the chosen
        // material to its non-fog shader variant. With c21.y=c21.w=0 the native
        // shader computes exp2(0)=1 transmittance. Native default values remain
        // a separate EffectsEd environment check.
        if material.metadata["pass"]
            .as_str()
            .is_some_and(|pass| pass.contains("_fog_"))
        {
            self.device
                .SetVertexShaderConstantF(21, [0.0f32, 0.0, 1.0, 0.0].as_ptr(), 1)?;
            self.device
                .SetPixelShaderConstantF(0, [0.0f32; 4].as_ptr(), 1)?;
        }
        // Native r_texFilter0/anisotropy1: low5 value2 disables mip selection;
        // values10/11 use nearest original mip (native code11 verified this round).
        // High bits independently clamp U/V/W.
        let sampler = crate::fx::material_states::sampler(material.sampler)?;
        let texture_slot = if material.distortion_scale.is_some() {
            4
        } else {
            0
        };
        for (state, value) in [
            (1, sampler.address_u),
            (2, sampler.address_v),
            (3, sampler.address_w),
            (5, sampler.mag_filter),
            (6, sampler.min_filter),
            (7, sampler.mip_filter),
            (10, 1),
            (11, 0),
        ] {
            self.device
                .SetSamplerState(texture_slot, D3DSAMPLERSTATETYPE(state), value)?;
        }
        self.device.SetTexture(texture_slot, &loaded.texture)?;
        let vertices = base64::engine::general_purpose::STANDARD
            .decode(draw["vertices"].as_str().ok_or("Missing vertices")?)?;
        let indices = validated_indices(draw, vertices.len())?;
        let stride = if cloud_stream {
            20
        } else if model_stream {
            32
        } else {
            32
        };
        self.device.DrawIndexedPrimitiveUP(
            D3DPT_TRIANGLELIST,
            0,
            (vertices.len() / stride) as u32,
            (indices.len() / 3) as u32,
            indices.as_ptr().cast(),
            D3DFMT_INDEX16,
            vertices.as_ptr().cast(),
            stride as u32,
        )?;
        Ok(())
    }

    pub(crate) unsafe fn pixels(&self) -> Result<Vec<u8>> {
        self.device
            .GetRenderTargetData(&self.target, &self.output)?;
        let mut locked = D3DLOCKED_RECT::default();
        self.output
            .LockRect(&mut locked, null(), D3DLOCK_READONLY as u32)?;
        let row_bytes = self.size as usize * 4;
        let mut pixels = vec![0; row_bytes * self.size as usize];
        for row in 0..self.size as usize {
            copy_nonoverlapping(
                locked.pBits.cast::<u8>().add(row * locked.Pitch as usize),
                pixels.as_mut_ptr().add(row * row_bytes),
                row_bytes,
            );
        }
        self.output.UnlockRect()?;
        Ok(pixels)
    }
}

fn distortion_rank(name: &str, sha256: &str) -> Result<Value> {
    // Never substitute filename sorting for the audited native comparator.
    match (name, sha256) {
        (
            "gfx_fxt_distortion_ring_thin",
            "bd15c50e387bab8a11f341a00ef3b1cc1faeec2dec6ac0bf6c125e1700cf20e7",
        ) => Ok(json!(0)),
        (
            "gfx_fxt_fx_distortion_heat",
            "bc1a79b005d438305e17e1521c93e687e93ed0a7169559d0f0375b4b2497bcbc",
        ) => Ok(json!(1)),
        (
            "gfx_fxt_fx_distortion_ring_heavy",
            "be795144e51673cb371f54344e1202e347712dcba81759882de27ebda0140872",
        ) => Ok(json!(2)),
        (
            "gfx_fxt_fx_distortion_ring_light",
            "18d7cad6b481e3327c349522cca4b6bd72dd8f3eed69535354b4c57b46ac0063",
        ) => Ok(json!(3)),
        (
            "gfx_fxt_fx_distortion_shc_wave",
            "19c0292aee16641adb5204c3b47095bda842580ec74408b789f1feb68f2c1c81",
        ) => Ok(json!(4)),
        (
            "gfx_fxt_fx_distortion_splash",
            "143af14f407fda81ae2cbcb57ec914b33ff676eab0d08042e4634fe6489c72ca",
        ) => Ok(json!(5)),
        (
            "gfx_fxt_fx_distortion_water_noise",
            "50cf6d8c7755e01baa0d89ae52b6fa4cfbf223b1a24ad6defd151bdd1477ffaa",
        ) => Ok(json!(6)),
        (
            "gfx_fxt_fx_shimmer",
            "4933b7f9359e174abce1b8fb073bb5b3bf53edeb4b40f5ef5d34c3198ce2c773",
        ) => Ok(json!(7)),
        _ => Err("Distortion material is outside the verified comparator".into()),
    }
}
