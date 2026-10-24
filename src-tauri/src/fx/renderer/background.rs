//! Preview backgrounds are colour only, not scene geometry. Clear rectangles
//! leave shaders, blend state, vertex declarations and depth untouched.

use serde_json::Value;
use windows::Win32::Graphics::Direct3D9::{IDirect3DDevice9, D3DCLEAR_TARGET, D3DRECT};

use super::Result;

pub(crate) const DARK: u32 = 0xff191e23;
const GREY: u32 = 0xff777777;
const LIGHT: u32 = 0xffd8d8d8;
const CHECKER_LIGHT: u32 = 0xff999999;
const CHECKER_DARK: u32 = 0xff666666;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Preset {
    #[default]
    Dark,
    Grey,
    Light,
    Checkerboard,
}

impl Preset {
    pub(crate) fn parse(name: &str) -> std::result::Result<Self, String> {
        match name {
            "dark" => Ok(Self::Dark),
            "grey" => Ok(Self::Grey),
            "light" => Ok(Self::Light),
            "checkerboard" => Ok(Self::Checkerboard),
            _ => Err("Unknown preview background".into()),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Grey => "grey",
            Self::Light => "light",
            Self::Checkerboard => "checkerboard",
        }
    }

    pub(crate) fn base_color(self) -> u32 {
        match self {
            Self::Dark => DARK,
            Self::Grey => GREY,
            Self::Light => LIGHT,
            Self::Checkerboard => CHECKER_DARK,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Background {
    Solid(u32),
    Checkerboard,
}

impl Background {
    pub(crate) fn from_frame(frame: &Value) -> Result<Self> {
        match frame.get("backgroundPreset") {
            // Saved/native verification packets can retain their authored
            // clear colour. The app supplies an explicit preset of its own.
            None | Some(Value::Null) => {
                let colour = frame["background"]
                    .as_u64()
                    .ok_or("Missing background colour")?;
                Ok(Self::Solid(u32::try_from(colour)?))
            }
            Some(Value::String(name)) => {
                let preset = Preset::parse(name)?;
                if preset == Preset::Checkerboard {
                    Ok(Self::Checkerboard)
                } else {
                    Ok(Self::Solid(preset.base_color()))
                }
            }
            _ => Err("Preview background must be a preset name".into()),
        }
    }

    pub(crate) fn base_colour(self) -> u32 {
        match self {
            Self::Solid(colour) => colour,
            Self::Checkerboard => CHECKER_DARK,
        }
    }

    /// Paint a stable 12×12 pattern at either thumbnail or viewport resolution.
    /// It is included in the ordinary pre-emissive scene copy, so distortion
    /// shaders sample actual contrast instead of a flat dummy texture.
    pub(crate) unsafe fn paint(self, device: &IDirect3DDevice9, size: u32) -> Result<()> {
        if self != Self::Checkerboard {
            return Ok(());
        }
        let divisions = 12_u32;
        let mut rectangles = Vec::with_capacity(72);
        for row in 0..divisions {
            for column in 0..divisions {
                if (row + column) % 2 == 0 {
                    rectangles.push(D3DRECT {
                        x1: (column * size / divisions) as i32,
                        y1: (row * size / divisions) as i32,
                        x2: ((column + 1) * size / divisions) as i32,
                        y2: ((row + 1) * size / divisions) as i32,
                    });
                }
            }
        }
        device.Clear(
            rectangles.len() as u32,
            rectangles.as_ptr(),
            D3DCLEAR_TARGET as u32,
            CHECKER_LIGHT,
            1.0,
            0,
        )?;
        Ok(())
    }
}
