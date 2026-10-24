//! Native mc_ unlit model shader selection for the standalone FX renderer.
//! UV input packing is independently checked against the original loose-model
//! converter; unsupported state/shader families still reject the whole effect.
use super::renderer::*;
use serde_json::json;
use std::{fs, path::Path};

pub(super) fn read(game: &Path, name: &str) -> Result<Material> {
    read_with_policy(game, name, false)
}

fn read_with_policy(game: &Path, name: &str, allow_translucent: bool) -> Result<Material> {
    relative_name(name)?;
    let raw = game.join("raw");
    let material_path = raw.join("materials").join(name);
    let bytes = fs::read(&material_path)
        .map_err(|error| format!("Read {}: {error}", material_path.display()))?;
    let word = u32_at(&bytes, 40)?;
    let second_word = u32_at(&bytes, 44)?;
    if ![0x0812_0000, 0x0d25_0000].contains(&(word & !0x1000_ffff))
        || ![2, 12, 0x4006c, 0x4004c, 0x4004d, 0x2c, 13].contains(&second_word)
    {
        return Err(format!("Unproved model material state: {name}").into());
    }
    let family = string_at(&bytes, u32_at(&bytes, 52)? as usize)?;
    relative_name(&family)?;
    let techset_path = raw.join("techsets").join(format!("mc_{family}.techset"));
    let techset = fs::read(&techset_path)
        .map_err(|error| format!("Read {}: {error}", techset_path.display()))?;
    let text = std::str::from_utf8(&techset)?;
    let pass =
        super::techset::technique(text, "unlit")?.ok_or("Model material has no unlit pass")?;
    // Native XModel registration requests mc/<surface>, selecting mc_ without
    // probing alternative files. The vcsh aliases have identical native
    // contracts; each remaining admitted pass uses its exact program pair.
    let (technique, vertex_program, pixel_program) = match pass.as_str() {
        "vcsh_dtex" | "vcsh_d0_dtex" | "vcsh0_dtex_ct" | "vcsh_dtex_dfoliage" => (
            "cd2cdfe5900772498434dc39a5cf9132e921e0cd2f09b840fbaddc6dc8723d62",
            "vs_3_0_fb69530b",
            "ps_3_0_0b9c522e",
        ),
        "vcsh_treecanopy_dtex" => (
            "8b441a31e29b499695691460e3f9dc51c3e19e8dbfbb33f99ca29214fa621a21",
            "vs_3_0_a74fc43a",
            "ps_3_0_f458b77f",
        ),
        "vertcol_simple_fog_dtex"
        | "vertcol_simple_fog_dtex_lit"
        | "vertcol_simple_fog_dtex_unlit" => (
            "4a90ef9e3eb788281e923548948d4ff4e1b8230fd38c93c9c867e0a66ce858bc",
            "vs_3_0_fb69530b",
            "ps_3_0_e2f1a87f",
        ),
        "vertcol_simple_dtex" => (
            "0851261f7c2d1ca02d01e8e7f32a8af2e2ed1fecfa3ffd93121ceac9bf80618f",
            "vs_3_0_a74fc43a",
            "ps_3_0_9532ed6e",
        ),
        _ => return Err(format!("Unproved native model unlit pass: {name} -> {pass}").into()),
    };
    pinned_file(
        &raw.join("techniques").join(format!("{pass}.tech")),
        technique,
    )?;
    pinned_file(
        &raw.join("statemaps/default.sm"),
        "1e0de3a39c5c755b23c741a187138172485d03b08adef8070c3eb7a14c3a6bfc",
    )?;
    let count = u16::from_le_bytes(bytes[48..50].try_into()?) as usize;
    let table = u32_at(&bytes, 56)? as usize;
    let mut binding = None;
    for index in 0..count {
        let offset = table + 12 * index;
        if string_at(&bytes, u32_at(&bytes, offset)? as usize)? == "colorMap" {
            let image = string_at(&bytes, u32_at(&bytes, offset + 8)? as usize)?;
            relative_name(&image)?;
            let sampler = u32_at(&bytes, offset + 4)?;
            super::material_states::sampler(sampler)?;
            binding = Some((image, sampler));
        }
    }
    let (image, sampler) = binding.ok_or("Model material has no colour map")?;
    let state = super::material_states::operation(word, "default")?;
    // Ordinary playback admits the exact glass state pair whose complete
    // mixed effect passed the independent 41-frame native-input GPU gate.
    // Other alpha routes remain private controls, not general admission.
    if state.blend && !allow_translucent && !proved_translucent_state(word, second_word) {
        return Err("Unproved translucent model state or queue contract".into());
    }
    let alpha_test = if word & 0x800 != 0 {
        // The original default state map substitutes GT0 for this ordinary
        // alpha-blend route. This also applies to an unlit model technique.
        if state.blend && word & 15 == 5 && [2, 6].contains(&((word >> 4) & 15)) {
            Some((5, 0))
        } else {
            None
        }
    } else {
        match (word >> 12) & 3 {
            1 => Some((5, 0)),
            2 => Some((2, 128)),
            3 => Some((7, 128)),
            _ => return Err("Unproved model alpha test".into()),
        }
    };
    let cull_mode = match (word >> 14) & 3 {
        1 => 1,
        2 => 3,
        3 => 2,
        _ => return Err("Invalid model cull mode".into()),
    };
    let metadata = json!({"modelSurface":true,"name":name,"family":family,
        "materialSha256":digest(&bytes),"techsetSha256":digest(&techset),
        "pass":pass,"materialCategory":"mc/","stateMap":"default","word":word,"secondWord":second_word,
        "techniqueSha256":technique,"colorMap":image,"sampler":sampler,
        "vertexProgram":vertex_program,"pixelProgram":pixel_program,
        "primarySortKey":bytes[9],"atlasRows":1,"atlasColumns":1,
        "previewPolicy":"Original material unlit technique; stored vertex colours, materialColor overlay0, fog off; no advanced lighting/shadows/specular"});
    Ok(Material {
        metadata,
        image,
        additive_shader: false,
        cloud: false,
        vertex_program: vertex_program.into(),
        pixel_program: pixel_program.into(),
        feather: None,
        source: word & 15,
        destination: (word >> 4) & 15,
        operation: state.operation,
        alpha_test,
        alpha_source: 8,
        alpha_destination: 2,
        cull_mode,
        falloff: None,
        eye_offset: None,
        distortion_scale: None,
        second_word,
        sampler,
    })
}

fn proved_translucent_state(word: u32, second_word: u32) -> bool {
    word == 0x0812_8965 && second_word == 0x0000_000d
}
