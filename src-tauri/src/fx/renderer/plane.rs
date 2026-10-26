//! Deliberately simple owned preview floor. Its base colour is a viewer setting;
//! the optional point-light pass is the original installed diffuse shader pair.
//! No preview room, lightmaps, sunlight, specular, shadow or bounced light.
use super::*;

pub(super) struct Resources {
    declaration: IDirect3DVertexDeclaration9,
    vertex_shader: IDirect3DVertexShader9,
    pixel_shader: IDirect3DPixelShader9,
    spot_pixel_shader: IDirect3DPixelShader9,
    base_pixel_shader: IDirect3DPixelShader9,
    white: IDirect3DTexture9,
    attenuation: IDirect3DTexture9,
    pub(super) float_z: IDirect3DTexture9,
    float_z_surface: IDirect3DSurface9,
    depth_declaration: IDirect3DVertexDeclaration9,
    depth_vertex_shader: IDirect3DVertexShader9,
    depth_pixel_shader: IDirect3DPixelShader9,
}

/// Original EffectsEd 0x4b3de0's spot constant packet. The x87 subtraction is
/// performed before storing its reciprocal as f32; keeping that distinction
/// avoids changing cone boundaries by rounding the denominator prematurely.
fn spot_constants(light: &Value) -> Result<([f32; 4], [f32; 4])> {
    let direction = vector(&light["direction"], [0.0; 3])?;
    if !light["direction"].is_array() {
        return Err("Spot light needs its native direction vector".into());
    }
    let outer = light["cosHalfFovOuter"]
        .as_f64()
        .ok_or("Missing spot outer cosine")? as f32;
    let inner = light["cosHalfFovInner"]
        .as_f64()
        .ok_or("Missing spot inner cosine")? as f32;
    let exponent = light["spotExponent"]
        .as_u64()
        .ok_or("Missing native spot exponent")?;
    if !(-1.0..=1.0).contains(&outer)
        || !(-1.0..=1.0).contains(&inner)
        || inner <= outer
        || exponent > u32::MAX as u64
    {
        return Err("Invalid native spot cone range or exponent".into());
    }
    let scale = (1.0f64 / (inner as f64 - outer as f64)) as f32;
    let bias = (-(scale as f64) * outer as f64) as f32;
    Ok((
        [direction[0], direction[1], direction[2], 0.0],
        [scale, bias, exponent as u32 as f32, 0.0],
    ))
}

#[derive(Clone, Copy)]
struct Settings {
    x_axis: bool,
    normal_sign: i8,
    height: f32,
    size: f32,
    base: [f32; 3],
    albedo: [f32; 3],
}

fn vector(value: &Value, fallback: [f32; 3]) -> Result<[f32; 3]> {
    if value.is_null() {
        return Ok(fallback);
    }
    let values = value
        .as_array()
        .ok_or("Plane/light vector must be an array")?;
    if values.len() != 3 {
        return Err("Plane/light vector needs three numbers".into());
    }
    let mut result = [0.0; 3];
    for (index, value) in values.iter().enumerate() {
        let number = value.as_f64().ok_or("Invalid plane/light vector")? as f32;
        if !number.is_finite() {
            return Err("Nonfinite plane/light vector".into());
        }
        result[index] = number;
    }
    Ok(result)
}

impl Settings {
    fn read(value: &Value) -> Result<Self> {
        if !value.is_object() {
            return Err("previewPlane must be an explicit settings object".into());
        }
        let height = value["height"].as_f64().unwrap_or(-16.0) as f32;
        let size = value["size"].as_f64().unwrap_or(256.0) as f32;
        let base = vector(&value["baseRgb"], [0.04; 3])?;
        let albedo = vector(&value["albedoRgb"], [0.5; 3])?;
        let x_axis = match value["axis"].as_str().unwrap_or("z") {
            "x" => true,
            "z" => false,
            _ => return Err("Preview plane axis must be x or z".into()),
        };
        let normal_sign = match value["normalSign"].as_i64().unwrap_or(1) {
            -1 => -1,
            1 => 1,
            _ => return Err("Preview plane normalSign must be -1 or 1".into()),
        };
        if value.get("normalSign").is_some() && value["normalSign"].as_i64().is_none() {
            return Err("Preview plane normalSign must be an integer".into());
        }
        if !height.is_finite() || !size.is_finite() || size <= 0.0 {
            return Err("Plane size must be positive and its position finite".into());
        }
        if base
            .iter()
            .chain(&albedo)
            .any(|number| !(0.0..=1.0).contains(number))
        {
            return Err("Plane base/albedo RGB must be in 0..1".into());
        }
        Ok(Self {
            x_axis,
            normal_sign,
            height,
            size,
            base,
            albedo,
        })
    }

    fn points(self) -> [[f32; 3]; 4] {
        let half = self.size * 0.5;
        let points = [
            [-half, -half, self.height],
            [-half, half, self.height],
            [half, half, self.height],
            [half, -half, self.height],
        ];
        if self.x_axis {
            points.map(|point| [point[2], point[0], point[1]])
        } else {
            points
        }
    }
}

/// D3DCOLOR byte order is BGRA in the vertex stream. Alpha zero is intentional:
/// each light independently starts with an empty native destination-alpha mask.
fn packed_colour(rgb: [f32; 3]) -> u32 {
    let bytes = rgb.map(|channel| (channel * 255.0).round().clamp(0.0, 255.0) as u32);
    (bytes[0] << 16) | (bytes[1] << 8) | bytes[2]
}

fn light_vertices(settings: Settings) -> Vec<u8> {
    let colour = packed_colour(settings.albedo) | 0xff00_0000;
    let mut vertices = Vec::with_capacity(112);
    for (index, position) in settings.points().into_iter().enumerate() {
        for coordinate in position {
            vertices.extend(coordinate.to_le_bytes());
        }
        vertices.extend(colour.to_le_bytes());
        let uv = [[0.0f32, 1.0], [0.0, 0.0], [1.0, 0.0], [1.0, 1.0]][index];
        for coordinate in uv {
            vertices.extend(coordinate.to_le_bytes());
        }
        // Original packed normal decodes to +Z; the shader normalizes it.
        let component = if settings.normal_sign == 1 { 254 } else { 0 };
        vertices.extend(if settings.x_axis {
            [component, 127, 127, 0]
        } else {
            [127, 127, component, 0]
        });
    }
    vertices
}

/// Decode the installed one-dimensional attenuation image without inventing
/// a falloff ramp. Raw luminance (IWI format 4) and wavelet luminance (format 9)
/// are different encodings; both upload their original mip values to D3D L8.
fn attenuation_levels(bytes: &[u8]) -> Result<Vec<(u32, u32, Vec<u8>)>> {
    if bytes.len() < 28 || bytes[..4] != *b"IWi\x06" {
        return Err("Expected a WaW v6 light attenuation image".into());
    }
    let width = u16::from_le_bytes(bytes[6..8].try_into()?) as u32;
    let height = u16::from_le_bytes(bytes[8..10].try_into()?);
    let depth = u16::from_le_bytes(bytes[10..12].try_into()?);
    if width == 0 || width > 8192 || height != 1 || depth != 1 || bytes[5] & 12 != 0 {
        return Err("Light attenuation must be a bounded one-dimensional image".into());
    }
    if bytes[4] == 9 {
        return wavelet_luminance_levels(bytes);
    }
    if bytes[4] != 4 {
        return Err("Unsupported light attenuation image encoding".into());
    }
    let count = if bytes[5] & 2 != 0 {
        1
    } else {
        (32 - width.leading_zeros()) as usize
    };
    let mut offset = 28_usize;
    let mut levels = Vec::with_capacity(count);
    // IWI stores the smallest mip first. Return the top mip first, matching
    // D3D9 level numbers and the family's independently checked IWI decoder.
    for mip in (0..count).rev() {
        let mip_width = (width >> mip).max(1);
        let end = offset
            .checked_add(mip_width as usize)
            .ok_or("Attenuation mip offset overflow")?;
        let pixels = bytes
            .get(offset..end)
            .ok_or("Incomplete attenuation mip data")?
            .to_vec();
        levels.push((mip_width, 1, pixels));
        offset = end;
    }
    if offset != bytes.len() {
        return Err("Unexpected trailing attenuation image data".into());
    }
    levels.reverse();
    Ok(levels)
}

unsafe fn l8_texture(device: &IDirect3DDevice9, levels: &Value) -> Result<IDirect3DTexture9> {
    let levels = levels.as_array().ok_or("Missing attenuation mip levels")?;
    let mut texture = None;
    device.CreateTexture(
        levels[0]["width"]
            .as_u64()
            .ok_or("Invalid attenuation width")? as u32,
        1,
        levels.len() as u32,
        0,
        D3DFMT_L8,
        D3DPOOL_MANAGED,
        &mut texture,
        null_mut(),
    )?;
    let texture = texture.ok_or("No preview texture returned")?;
    for (mip, level) in levels.iter().enumerate() {
        let bytes = level["pixelsL8"]
            .as_array()
            .ok_or("Missing attenuation pixels")?
            .iter()
            .map(|value| {
                value
                    .as_u64()
                    .map(|number| number as u8)
                    .ok_or_else(|| "Invalid attenuation sample".into())
            })
            .collect::<Result<Vec<_>>>()?;
        let mut locked = D3DLOCKED_RECT::default();
        texture.LockRect(mip as u32, &mut locked, null(), 0)?;
        copy_nonoverlapping(bytes.as_ptr(), locked.pBits.cast(), bytes.len());
        texture.UnlockRect(mip as u32)?;
    }
    Ok(texture)
}

impl Resources {
    /// Populate float-Z only where the model's already rendered opaque/cutout
    /// surface owns hardware depth. EQUAL preserves its texture alpha holes.
    pub(super) unsafe fn models_depth(
        &self,
        device: &IDirect3DDevice9,
        frame: &Value,
        declaration: &IDirect3DVertexDeclaration9,
        materials: &HashMap<String, LoadedMaterial>,
    ) -> Result<()> {
        let state = device.CreateStateBlock(D3DSBT_ALL)?;
        let target = device.GetRenderTarget(0)?;
        device.SetTexture(4, None::<&IDirect3DBaseTexture9>)?;
        device.SetTexture(5, None::<&IDirect3DBaseTexture9>)?;
        device.SetRenderTarget(0, &self.float_z_surface)?;
        device.BeginScene()?;
        let result = (|| -> Result<()> {
            let mut projection: Vec<f32> = frame["matrixRows"]
                .as_array()
                .ok_or("Missing projection")?
                .iter()
                .flat_map(|row| row.as_array().unwrap().iter())
                .map(|number| number.as_f64().unwrap() as f32)
                .collect();
            let eye: [f32; 3] = serde_json::from_value(frame["cameraOrigin"].clone())?;
            for row in 0..4 {
                for axis in 0..3 {
                    projection[row * 4 + 3] += projection[row * 4 + axis] * eye[axis];
                }
            }
            device.SetVertexDeclaration(declaration)?;
            device.SetVertexShader(&self.depth_vertex_shader)?;
            device.SetPixelShader(&self.depth_pixel_shader)?;
            device.SetVertexShaderConstantF(0, projection.as_ptr(), 4)?;
            device.SetVertexShaderConstantF(20, [0.0f32, 0.0, 0.0, 1.0].as_ptr(), 1)?;
            for (key, value) in [
                (D3DRS_ZENABLE, 1),
                (D3DRS_ZWRITEENABLE, 0),
                (D3DRS_ZFUNC, D3DCMP_EQUAL.0 as u32),
                (D3DRS_ALPHABLENDENABLE, 0),
                (D3DRS_ALPHATESTENABLE, 0),
                (D3DRS_COLORWRITEENABLE, 15),
                (D3DRS_STENCILENABLE, 0),
                (D3DRS_SRGBWRITEENABLE, 0),
            ] {
                device.SetRenderState(key, value)?;
            }
            for draw in frame["draws"]
                .as_array()
                .ok_or("Missing draws")?
                .iter()
                .filter(|draw| draw["vertexFormat"] == "model32")
            {
                let name = draw["texture"].as_str().ok_or("Missing material")?;
                let material = &materials[name].material;
                let operation = crate::fx::material_states::operation(
                    material.metadata["word"]
                        .as_u64()
                        .ok_or("Missing state word")? as u32,
                    material.metadata["stateMap"]
                        .as_str()
                        .ok_or("Missing state map")?,
                )?;
                if !operation.depth_write {
                    // Blended model surfaces do not own the scene depth.
                    // Even an accidental depth tie must not add soft-FX depth.
                    continue;
                }
                let placement: crate::fx::model_pose::Placement =
                    serde_json::from_value(draw["modelPlacement"].clone())?;
                let world: Vec<f32> = crate::fx::model_pose::world_rows(&placement, eye)
                    .into_iter()
                    .flatten()
                    .collect();
                device.SetVertexShaderConstantF(4, world.as_ptr(), 4)?;
                device.SetRenderState(D3DRS_CULLMODE, materials[name].material.cull_mode)?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(draw["vertices"].as_str().ok_or("Missing vertices")?)?;
                let indices = validated_indices(draw, bytes.len())?;
                device.DrawIndexedPrimitiveUP(
                    D3DPT_TRIANGLELIST,
                    0,
                    (bytes.len() / 32) as u32,
                    (indices.len() / 3) as u32,
                    indices.as_ptr().cast(),
                    D3DFMT_INDEX16,
                    bytes.as_ptr().cast(),
                    32,
                )?;
            }
            Ok(())
        })();
        device.EndScene()?;
        device.SetRenderTarget(0, &target)?;
        state.Apply()?;
        result
    }
    pub(super) unsafe fn new(device: &IDirect3DDevice9, game: &Path, size: u32) -> Result<Self> {
        let vs = shader(
            game,
            "vs_3_0_a538814b",
            "6a406adbb9df012589fb1ceb07a7ae2c65305bd089a958a954648b7f08ce8168",
        )?;
        let ps = shader(
            game,
            "ps_3_0_c732aeb2",
            "e462941d510efc66c0b071c4ad4c9ead74ab98bd9bb1f98f9e0d5a3c9158cc94",
        )?;
        // Verify the original installed attenuation source independently of the
        // installed image's decoded L8 levels before creating the GPU resource.
        let mut archive = zip::ZipArchive::new(fs::File::open(game.join("main/iw_02.iwd"))?)?;
        let mut bytes = Vec::new();
        archive
            .by_name("images/falloff_linear.iwi")?
            .read_to_end(&mut bytes)?;
        if digest(&bytes) != "851dfdbe211a3488e8387926c62018726d5e2d7085a79bd2384d5ddf0aa5bfa4" {
            return Err("Installed verified point-light attenuation image changed".into());
        }
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
                Offset: 12,
                Type: 4,
                Method: 0,
                Usage: 10,
                UsageIndex: 0,
            },
            D3DVERTEXELEMENT9 {
                Stream: 0,
                Offset: 16,
                Type: 1,
                Method: 0,
                Usage: 5,
                UsageIndex: 0,
            },
            D3DVERTEXELEMENT9 {
                Stream: 0,
                Offset: 24,
                Type: 5,
                Method: 0,
                Usage: 3,
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
        let depth_elements = [
            D3DVERTEXELEMENT9 {
                Stream: 0,
                Offset: 0,
                Type: 2,
                Method: 0,
                Usage: 0,
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
        let depth_vs = shader(
            game,
            "vs_3_0_b4b2e8cf",
            "99aa8dd3695adb4fef066593088aa94f154d38318616a3963b6f5096b016fed5",
        )?;
        let depth_ps = shader(
            game,
            "ps_3_0_add7ff09",
            "f7ab2b35a6fd844526b8d21359251cde19e65554eaf366c4ab945d8c6a7c7066",
        )?;
        let spot_ps = shader(
            game,
            "ps_3_0_5f4c649c",
            "b180eea67cc108b52a24c7b7cb90adceafe13c354e2d49350dfc2908aac08534",
        )?;
        let base_ps = shader(
            game,
            "ps_3_0_1d8b95b0",
            "b039d763a26a34ada465b421178a3a0dc02981e09817dfea983e87d73becc5cd",
        )?;
        let mut float_z = None;
        device.CreateTexture(
            size,
            size,
            1,
            D3DUSAGE_RENDERTARGET as u32,
            D3DFMT_R32F,
            D3DPOOL_DEFAULT,
            &mut float_z,
            null_mut(),
        )?;
        let float_z = float_z.ok_or("No preview float-depth texture returned")?;
        let float_z_surface = float_z.GetSurfaceLevel(0)?;
        Ok(Self {
            declaration: device.CreateVertexDeclaration(elements.as_ptr())?,
            vertex_shader: device.CreateVertexShader(vs.as_ptr())?,
            pixel_shader: device.CreatePixelShader(ps.as_ptr())?,
            spot_pixel_shader: device.CreatePixelShader(spot_ps.as_ptr())?,
            base_pixel_shader: device.CreatePixelShader(base_ps.as_ptr())?,
            white: l8_texture(device, &json!([{ "width": 1, "pixelsL8": [255] }]))?,
            attenuation: l8_texture(
                device,
                &json!(attenuation_levels(&bytes)?
                    .into_iter()
                    .map(|(width, _height, pixels)| json!({"width":width,"pixelsL8":pixels}))
                    .collect::<Vec<_>>()),
            )?,
            float_z,
            float_z_surface,
            depth_declaration: device.CreateVertexDeclaration(depth_elements.as_ptr())?,
            depth_vertex_shader: device.CreateVertexShader(depth_vs.as_ptr())?,
            depth_pixel_shader: device.CreatePixelShader(depth_ps.as_ptr())?,
        })
    }

    /// All state is restored before ordinary FX work, including the shader
    /// constants that the native receiver uses differently from particles.
    pub(super) unsafe fn draw(&self, device: &IDirect3DDevice9, frame: &Value) -> Result<()> {
        let saved = device.CreateStateBlock(D3DSBT_ALL)?;
        let result = self.draw_inner(device, frame);
        saved.Apply()?;
        result
    }

    unsafe fn draw_inner(&self, device: &IDirect3DDevice9, frame: &Value) -> Result<()> {
        let settings = Settings::read(&frame["previewPlane"])?;
        let rows = frame["matrixRows"]
            .as_array()
            .ok_or("Missing plane projection")?;
        let mut projection = [0.0f32; 16];
        for (row_index, row) in rows.iter().enumerate() {
            if row_index >= 4 || row.as_array().map(Vec::len) != Some(4) {
                return Err("Plane projection must contain four rows".into());
            }
            for column in 0..4 {
                projection[row_index * 4 + column] = row[column]
                    .as_f64()
                    .ok_or("Invalid plane projection coordinate")?
                    as f32;
            }
        }
        if rows.len() != 4 || projection.iter().any(|number| !number.is_finite()) {
            return Err("Plane projection must contain sixteen finite values".into());
        }
        let identity = [
            1.0f32, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ];
        let eye = vector(&frame["cameraOrigin"], [0.0; 3])?;
        let mut relative_projection = projection;
        for row in 0..4 {
            for axis in 0..3 {
                relative_projection[row * 4 + 3] += relative_projection[row * 4 + axis] * eye[axis];
            }
        }
        let world = [
            1.0f32, 0.0, 0.0, -eye[0], 0.0, 1.0, 0.0, -eye[1], 0.0, 0.0, 1.0, -eye[2], 0.0, 0.0,
            0.0, 1.0,
        ];
        let vertices = light_vertices(settings);
        let indices = [0u16, 1, 2, 2, 3, 0];
        // All three floor passes use the same camera-relative world/projection
        // constants and the same DP4 position sequence. A fixed-function base
        // transform can round depth differently at large world coordinates,
        // making a valid LESS_EQUAL light pass disappear completely.
        let depth_only = frame["previewPlane"]["depthOnly"].as_bool() == Some(true);
        self.write_depth(
            device,
            settings,
            &relative_projection,
            &world,
            &identity,
            !depth_only,
        )?;
        if depth_only {
            return Ok(());
        }
        self.base_state(device, &relative_projection, &world, settings.base)?;
        device.DrawIndexedPrimitiveUP(
            D3DPT_TRIANGLELIST,
            0,
            4,
            2,
            indices.as_ptr().cast(),
            D3DFMT_INDEX16,
            vertices.as_ptr().cast(),
            28,
        )?;
        let lights = frame["lights"]
            .as_array()
            .ok_or("Plane scene requires an explicit lights array")?;
        for light in lights {
            let spot = match light["type"].as_str() {
                None | Some("point") => None,
                Some("spot") => Some(spot_constants(light)?),
                _ => return Err("Unsupported scene light type".into()),
            };
            let origin = vector(&light["origin"], [0.0; 3])?;
            let rgb = vector(&light["rgb"], [0.0; 3])?;
            let radius = light["radius"]
                .as_f64()
                .ok_or("Missing point-light radius")? as f32;
            if !radius.is_finite() || radius <= 0.0 || rgb.iter().any(|channel| *channel < 0.0) {
                return Err("Point-light radius must be positive and RGB nonnegative".into());
            }
            // Reset only the floor's alpha mask, retaining accumulated RGB and
            // its hardware depth. Otherwise the first light would suppress all
            // later lights through INVDESTALPHA blending.
            self.base_state(device, &relative_projection, &world, settings.base)?;
            device.SetRenderState(D3DRS_ZWRITEENABLE, 0)?;
            // The original clear-alpha pass disables depth; its purpose is a
            // fresh light mask, not a second geometry visibility decision.
            device.SetRenderState(D3DRS_ZENABLE, 0)?;
            device.SetRenderState(D3DRS_COLORWRITEENABLE, 8)?;
            device.DrawIndexedPrimitiveUP(
                D3DPT_TRIANGLELIST,
                0,
                4,
                2,
                indices.as_ptr().cast(),
                D3DFMT_INDEX16,
                vertices.as_ptr().cast(),
                28,
            )?;
            self.light_state(device)?;
            if let Some((direction, factors)) = spot {
                device.SetPixelShader(&self.spot_pixel_shader)?;
                device.SetPixelShaderConstantF(7, direction.as_ptr(), 1)?;
                device.SetPixelShaderConstantF(8, factors.as_ptr(), 1)?;
            }
            device.SetVertexShaderConstantF(0, relative_projection.as_ptr(), 4)?;
            device.SetVertexShaderConstantF(4, world.as_ptr(), 4)?;
            device.SetPixelShaderConstantF(
                5,
                [
                    origin[0] - eye[0],
                    origin[1] - eye[1],
                    origin[2] - eye[2],
                    1.0 / radius,
                ]
                .as_ptr(),
                1,
            )?;
            device.SetPixelShaderConstantF(6, [rgb[0], rgb[1], rgb[2], 1.0].as_ptr(), 1)?;
            device.DrawIndexedPrimitiveUP(
                D3DPT_TRIANGLELIST,
                0,
                4,
                2,
                indices.as_ptr().cast(),
                D3DFMT_INDEX16,
                vertices.as_ptr().cast(),
                28,
            )?;
        }
        Ok(())
    }

    /// The installed float-Z shaders write dot(clipPosition, c20). Our owned
    /// perspective uses clip W as the same linear depth consumed by FX feather
    /// shaders. A full-screen shader draw clears R32F to 2,000,000 without the
    /// normalized/integer colour conversion of D3D9 Clear.
    unsafe fn write_depth(
        &self,
        device: &IDirect3DDevice9,
        settings: Settings,
        projection: &[f32; 16],
        world: &[f32; 16],
        identity: &[f32; 16],
        draw_plane: bool,
    ) -> Result<()> {
        let target = device.GetRenderTarget(0)?;
        device.SetTexture(4, None::<&IDirect3DBaseTexture9>)?;
        device.SetTexture(5, None::<&IDirect3DBaseTexture9>)?;
        device.SetRenderTarget(0, &self.float_z_surface)?;
        let result = (|| -> Result<()> {
            device.SetVertexDeclaration(&self.depth_declaration)?;
            device.SetVertexShader(&self.depth_vertex_shader)?;
            device.SetPixelShader(&self.depth_pixel_shader)?;
            for (state, value) in [
                (D3DRS_ZENABLE, 0),
                (D3DRS_ZWRITEENABLE, 0),
                (D3DRS_ALPHABLENDENABLE, 0),
                (D3DRS_ALPHATESTENABLE, 0),
                (D3DRS_CULLMODE, D3DCULL_NONE.0 as u32),
                (D3DRS_COLORWRITEENABLE, 15),
                (D3DRS_SRGBWRITEENABLE, 0),
                (D3DRS_STENCILENABLE, 0),
                (D3DRS_SCISSORTESTENABLE, 0),
                (D3DRS_SEPARATEALPHABLENDENABLE, 0),
            ] {
                device.SetRenderState(state, value)?;
            }
            device.SetVertexShaderConstantF(0, identity.as_ptr(), 4)?;
            device.SetVertexShaderConstantF(4, identity.as_ptr(), 4)?;
            device.SetVertexShaderConstantF(20, [0.0f32, 0.0, 0.0, 2_000_000.0].as_ptr(), 1)?;
            let full_screen = [
                -1.0f32, -1.0, 0.0, -1.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, -1.0, 0.0,
            ];
            let indices = [0u16, 1, 2, 2, 3, 0];
            device.DrawIndexedPrimitiveUP(
                D3DPT_TRIANGLELIST,
                0,
                4,
                2,
                indices.as_ptr().cast(),
                D3DFMT_INDEX16,
                full_screen.as_ptr().cast(),
                12,
            )?;
            device.SetVertexShaderConstantF(0, projection.as_ptr(), 4)?;
            device.SetVertexShaderConstantF(4, world.as_ptr(), 4)?;
            device.SetVertexShaderConstantF(20, [0.0f32, 0.0, 0.0, 1.0].as_ptr(), 1)?;
            if draw_plane {
                let points = settings.points();
                device.DrawIndexedPrimitiveUP(
                    D3DPT_TRIANGLELIST,
                    0,
                    4,
                    2,
                    indices.as_ptr().cast(),
                    D3DFMT_INDEX16,
                    points.as_ptr().cast(),
                    12,
                )?;
            }
            Ok(())
        })();
        device.SetRenderTarget(0, &target)?;
        result
    }

    unsafe fn base_state(
        &self,
        device: &IDirect3DDevice9,
        projection: &[f32; 16],
        world: &[f32; 16],
        base: [f32; 3],
    ) -> Result<()> {
        device.SetVertexDeclaration(&self.declaration)?;
        device.SetVertexShader(&self.vertex_shader)?;
        device.SetPixelShader(&self.base_pixel_shader)?;
        device.SetVertexShaderConstantF(0, projection.as_ptr(), 4)?;
        device.SetVertexShaderConstantF(4, world.as_ptr(), 4)?;
        device.SetVertexShaderConstantF(21, [0.0f32, 0.0, 1.0, 0.0].as_ptr(), 1)?;
        // Preserve the previous byte-quantized viewer base shade exactly. The
        // original unlit PS multiplies its white texture by c3, including A=0.
        let base = base.map(|value| (value * 255.0).round() / 255.0);
        device.SetPixelShaderConstantF(3, [base[0], base[1], base[2], 0.0].as_ptr(), 1)?;
        device.SetTexture(0, &self.white)?;
        for (state, value) in [(1, 3), (2, 3), (5, 1), (6, 1), (7, 0), (11, 0)] {
            device.SetSamplerState(0, D3DSAMPLERSTATETYPE(state), value)?;
        }
        for (state, value) in [
            (D3DRS_ZENABLE, 1),
            (D3DRS_ZWRITEENABLE, 1),
            (D3DRS_ZFUNC, D3DCMP_LESSEQUAL.0 as u32),
            (D3DRS_ALPHABLENDENABLE, 0),
            (D3DRS_ALPHATESTENABLE, 0),
            (D3DRS_CULLMODE, D3DCULL_NONE.0 as u32),
            (D3DRS_COLORWRITEENABLE, 15),
            (D3DRS_LIGHTING, 0),
            (D3DRS_FOGENABLE, 0),
            (D3DRS_SRGBWRITEENABLE, 0),
            (D3DRS_STENCILENABLE, 0),
            (D3DRS_SEPARATEALPHABLENDENABLE, 0),
            (D3DRS_SCISSORTESTENABLE, 0),
        ] {
            device.SetRenderState(state, value)?;
        }
        Ok(())
    }

    unsafe fn light_state(&self, device: &IDirect3DDevice9) -> Result<()> {
        device.SetVertexDeclaration(&self.declaration)?;
        device.SetVertexShader(&self.vertex_shader)?;
        device.SetPixelShader(&self.pixel_shader)?;
        device.SetVertexShaderConstantF(21, [0.0f32, 0.0, 1.0, 0.0].as_ptr(), 1)?;
        device.SetPixelShaderConstantF(0, [0.0f32; 4].as_ptr(), 1)?;
        for (state, value) in [
            (D3DRS_ZENABLE, 1),
            (D3DRS_ZWRITEENABLE, 0),
            (D3DRS_ZFUNC, D3DCMP_LESSEQUAL.0 as u32),
            (D3DRS_ALPHABLENDENABLE, 1),
            (D3DRS_BLENDOP, D3DBLENDOP_ADD.0 as u32),
            (D3DRS_SRCBLEND, D3DBLEND_INVDESTALPHA.0 as u32),
            (D3DRS_DESTBLEND, D3DBLEND_ONE.0 as u32),
            (D3DRS_COLORWRITEENABLE, 15),
        ] {
            device.SetRenderState(state, value)?;
        }
        for slot in [0, 4] {
            for (state, value) in [
                (1, 3),
                (2, 3),
                (3, 3),
                (5, 2),
                (6, 2),
                (7, 0),
                (10, 1),
                (11, 0),
            ] {
                device.SetSamplerState(slot, D3DSAMPLERSTATETYPE(state), value)?;
            }
        }
        device.SetTexture(0, &self.white)?;
        device.SetTexture(4, &self.attenuation)?;
        Ok(())
    }
}
