//! Bounded original EffectsEd material-state contracts. These helpers do not
//! choose techniques or resolve pass-specific alpha factors/cull/stencil state.
//! The admitted routes preserve the independently checked native device states.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operation {
    pub blend: bool,
    pub depth_write: bool,
    /// Separate-alpha routing is unnecessary while RGB blending is disabled.
    /// Enabled routes retain the renderer's existing pass-resolved factors.
    pub separate_alpha: bool,
    /// D3D has no Disable blend-op enumerant. Keep a legal inert ADD value when
    /// ALPHABLENDENABLE is false, rather than submitting invalid enum zero.
    pub operation: u32,
}

pub fn operation(word: u32, state_map: &str) -> Result<Operation, &'static str> {
    let authored_operation = (word >> 8) & 7;
    if !matches!(authored_operation, 0 | 1) {
        return Err("Material blend operation is outside the native proof");
    }
    let source = word & 15;
    let destination = (word >> 4) & 15;
    let blend = authored_operation != 0;
    if blend && (!(1..=10).contains(&source) || !(1..=10).contains(&destination)) {
        return Err("Material blend factor is outside the native proof");
    }
    // These are the exact installed state-map rules. The depth-write choice
    // must follow the selected pass rather than an unconditional FX setting.
    let depth_write = match state_map {
        "default" | "effect_add" => !blend,
        "zfeather_add" => source == 2 && destination == 1,
        _ => return Err("Material depth-write state map is outside the native proof"),
    };
    Ok(Operation {
        blend,
        depth_write,
        separate_alpha: blend,
        operation: 1,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sampler {
    pub min_filter: u32,
    pub mag_filter: u32,
    pub mip_filter: u32,
    pub address_u: u32,
    pub address_v: u32,
    pub address_w: u32,
}

/// Original r_texFilter=0 / anisotropy=1 profile, captured through the actual
/// 0x493630 lookup-table construction and 0x494550 D3D sampler setter.
pub fn sampler(value: u32) -> Result<Sampler, &'static str> {
    if ![
        0x61, 0x62, 0x2e1, 0x2e2, 0x2ea, 0x2eb, 0x242, 0x24a, 0x20a, 0x202, 0x20b,
    ]
    .contains(&value)
    {
        return Err("Sampler is outside the original native capture");
    }
    let byte = value as u8;
    let filter = match byte & 31 {
        1 => 1,
        2 | 10 | 11 => 2,
        _ => return Err("Sampler filter profile is outside the native proof"),
    };
    Ok(Sampler {
        min_filter: filter,
        mag_filter: filter,
        mip_filter: u32::from(matches!(byte & 31, 10 | 11)),
        address_u: if byte & 0x20 != 0 { 3 } else { 1 },
        address_v: if byte & 0x40 != 0 { 3 } else { 1 },
        address_w: if byte & 0x80 != 0 { 3 } else { 1 },
    })
}
