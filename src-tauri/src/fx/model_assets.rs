//! Owned loose WaW v25 FX model reader. No game process or export step.
//!
//! Adapted from the WaW Model Viewer parser. Retains native units, XYZ,
//! UVs, BGRA and clockwise indices rather than the catalogue's WebGL conversion.
//! Vertices are the loose model's assembled bind-pose coordinates; this module
//! does not apply a bone transform or reconstruct advanced material shaders.
use super::model_reader::Reader;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Lod {
    pub slot: usize,
    pub name: String,
    pub distance: f32,
    pub materials: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Descriptor {
    pub lods: Vec<Lod>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Surface {
    pub positions: Vec<f32>,
    pub normals: Vec<f32>,
    pub uvs: Vec<f32>,
    pub colours: Vec<u8>,
    pub indices: Vec<u16>,
    pub max_bone: u16,
    #[serde(rename = "skinIndices")]
    pub skin_indices: Vec<u16>,
    #[serde(rename = "skinWeights")]
    pub skin_weights: Vec<f32>,
}

impl Surface {
    /// A successfully parsed stream can still be an empty animation carrier.
    /// Check actual indexed faces, rather than the number of stored vertices.
    pub fn has_visible_triangle(&self) -> bool {
        self.indices.chunks_exact(3).any(|triangle| {
            let points: Vec<[f64; 3]> = triangle
                .iter()
                .map(|index| {
                    let offset = usize::from(*index) * 3;
                    std::array::from_fn(|axis| f64::from(self.positions[offset + axis]))
                })
                .collect();
            let a: [f64; 3] = std::array::from_fn(|axis| points[1][axis] - points[0][axis]);
            let b: [f64; 3] = std::array::from_fn(|axis| points[2][axis] - points[0][axis]);
            let cross = [
                a[1] * b[2] - a[2] * b[1],
                a[2] * b[0] - a[0] * b[2],
                a[0] * b[1] - a[1] * b[0],
            ];
            cross.iter().any(|component| *component != 0.0)
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bone {
    pub name: String,
    pub parent: Option<usize>,
    pub position: [f64; 3],
    pub local_position: [f64; 3],
    pub local_rotation: [f64; 4],
}

pub fn asset_path(root: &Path, folder: &str, name: &str) -> Result<PathBuf, String> {
    if name.is_empty()
        || Path::new(name)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("Invalid asset path.".into());
    }
    Ok(root.join("raw").join(folder).join(name))
}

fn data(path: &Path) -> Result<Vec<u8>, String> {
    let size = fs::metadata(path)
        .map_err(|error| format!("Read {}: {error}", path.display()))?
        .len();
    if size > 128 * 1024 * 1024 {
        return Err("Asset exceeds the supported file size.".into());
    }
    fs::read(path).map_err(|error| format!("Read {}: {error}", path.display()))
}

fn version(reader: &mut Reader<'_>) -> Result<(), String> {
    let value = reader.u16()?;
    if value != 25 {
        return Err(format!(
            "Unsupported model version {value}; expected PC WaW v25."
        ));
    }
    Ok(())
}

pub fn descriptor(path: &Path) -> Result<Descriptor, String> {
    let bytes = data(path)?;
    let mut reader = Reader::new(&bytes);
    version(&mut reader)?;
    reader.skip(1 + 24)?;
    reader.string()?;
    reader.string()?;
    let mut lods = Vec::new();
    for slot in 0..4 {
        let distance = reader.f32()?;
        let name = reader.string()?;
        if !name.is_empty() {
            lods.push(Lod {
                slot,
                name,
                distance,
                materials: Vec::new(),
            });
        }
    }
    reader.skip(4)?;
    let collision_groups = reader.u32()?;
    if collision_groups > 65536 {
        return Err("Invalid collision group count.".into());
    }
    for _ in 0..collision_groups {
        let count = reader.u32()? as usize;
        let size = count
            .checked_mul(48)
            .and_then(|value| value.checked_add(36))
            .ok_or("Collision size overflow.")?;
        reader.skip(size)?;
    }
    for lod in &mut lods {
        for _ in 0..reader.u16()? {
            lod.materials.push(reader.string()?);
        }
    }
    // The descriptor continues with data outside the geometry/material table.
    // Reading it to EOF would reject valid stock models; the viewer needs only
    // the LOD references and material names above.
    Ok(Descriptor { lods })
}

pub fn surfaces(path: &Path) -> Result<Vec<Surface>, String> {
    let bytes = data(path)?;
    let mut reader = Reader::new(&bytes);
    version(&mut reader)?;
    let count = reader.u16()?;
    if count > 4096 {
        return Err("Invalid surface count.".into());
    }
    let mut output = Vec::new();
    for _ in 0..count {
        reader.skip(3)?;
        let vertices = reader.u16()? as usize;
        let triangles = reader.u16()? as usize;
        let mut group_size = reader.u16()?;
        let mut groups = Vec::new();
        let mut grouped_vertices = 0;
        while group_size != 0 {
            grouped_vertices += usize::from(group_size);
            if grouped_vertices > vertices || groups.len() > 255 {
                return Err("Invalid rigid vertex groups.".into());
            }
            groups.push((group_size, reader.u16()?));
            group_size = reader.u16()?;
        }
        let rigid = groups.len() == 1 && grouped_vertices == vertices;
        if !rigid {
            reader.skip(2)?;
        }
        let default_bone = groups.first().map(|group| group.1).unwrap_or(0);
        let mut surface = Surface {
            positions: Vec::with_capacity(vertices * 3),
            normals: Vec::with_capacity(vertices * 3),
            uvs: Vec::with_capacity(vertices * 2),
            colours: Vec::with_capacity(vertices * 4),
            indices: Vec::with_capacity(triangles * 3),
            max_bone: 0,
            skin_indices: Vec::with_capacity(vertices * 4),
            skin_weights: Vec::with_capacity(vertices * 4),
        };
        for _ in 0..vertices {
            surface.normals.extend(reader.vector()?);
            let colour = reader.bytes(4)?;
            // Keep the original PC D3DCOLOR byte order for Direct3D. Unlike
            // the WebGL catalogue, this FX reader does not convert BGRA to RGBA.
            surface.colours.extend_from_slice(colour);
            surface.uvs.extend([reader.f32()?, reader.f32()?]);
            // Tangents are not needed by this colour-only viewer. Four stock
            // vertices contain invalid tangents; their useful attributes are valid.
            reader.skip(24)?;
            let extra = if rigid { 0 } else { reader.u8()? };
            if extra > 3 {
                return Err("Unsupported vertex influence count.".into());
            }
            let primary = if rigid { default_bone } else { reader.u16()? };
            surface.max_bone = surface.max_bone.max(primary);
            surface.positions.extend(reader.vector()?);
            let mut weight_sum = 0_u32;
            let mut indices = [primary, 0, 0, 0];
            let mut weights = [0.0_f32; 4];
            for slot in 1..=usize::from(extra) {
                indices[slot] = reader.u16()?;
                surface.max_bone = surface.max_bone.max(indices[slot]);
                let weight = reader.u16()?;
                weight_sum += u32::from(weight);
                weights[slot] = f32::from(weight) / 65535.0;
            }
            if weight_sum > 65535 {
                return Err("Invalid vertex weights.".into());
            }
            weights[0] = 1.0 - weight_sum as f32 / 65535.0;
            surface.skin_indices.extend(indices);
            surface.skin_weights.extend(weights);
        }
        for _ in 0..triangles {
            let a = reader.u16()?;
            let b = reader.u16()?;
            let c = reader.u16()?;
            if [a, b, c]
                .iter()
                .any(|index| usize::from(*index) >= vertices)
            {
                return Err("Triangle index outside the vertex array.".into());
            }
            // Preserve native triangle winding. No WebGL/GLB conversion is
            // appropriate for this original Direct3D rendering research path.
            surface.indices.extend([a, b, c]);
        }
        output.push(surface);
    }
    reader.finish()?;
    Ok(output)
}

pub fn bone_count(path: &Path) -> Result<usize, String> {
    let bytes = data(path)?;
    let mut reader = Reader::new(&bytes);
    version(&mut reader)?;
    Ok(usize::from(reader.u16()?) + usize::from(reader.u16()?))
}

fn multiply(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    let [x, y, z, w] = a;
    let [xx, yy, zz, ww] = b;
    [
        w * xx + x * ww + y * zz - z * yy,
        w * yy - x * zz + y * ww + z * xx,
        w * zz + x * yy - y * xx + z * ww,
        w * ww - x * xx - y * yy - z * zz,
    ]
}

fn normalise(mut quaternion: [f64; 4]) -> [f64; 4] {
    let length = quaternion
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    for value in &mut quaternion {
        *value /= length;
    }
    quaternion
}

fn rotate(quaternion: [f64; 4], vector: [f64; 3]) -> [f64; 3] {
    let q = normalise(quaternion);
    let product = multiply(
        multiply(q, [vector[0], vector[1], vector[2], 0.0]),
        [-q[0], -q[1], -q[2], q[3]],
    );
    [product[0], product[1], product[2]]
}

pub fn bones(path: &Path) -> Result<Vec<Bone>, String> {
    let bytes = data(path)?;
    let mut reader = Reader::new(&bytes);
    version(&mut reader)?;
    let children = usize::from(reader.u16()?);
    let roots = usize::from(reader.u16()?);
    if children + roots > 256 {
        return Err("Invalid skeleton size.".into());
    }
    let mut parents = vec![None; roots];
    let mut positions = vec![[0.0; 3]; roots];
    let mut rotations = vec![[0.0, 0.0, 0.0, 1.0]; roots];
    let mut local_positions = positions.clone();
    let mut local_rotations = rotations.clone();
    for _ in 0..children {
        let parent = usize::from(reader.u8()?);
        if parent >= parents.len() {
            return Err("Bone parent does not precede its child.".into());
        }
        let position = reader.vector()?.map(f64::from);
        let xyz = [reader.i16()?, reader.i16()?, reader.i16()?].map(i64::from);
        let squared = 32767_i64.pow(2) - xyz.iter().map(|value| value * value).sum::<i64>();
        // Match native packed-W rounding before normalising the quaternion.
        let w = if squared > 0 {
            ((squared as f64).sqrt() as f32 + 0.5).floor() as f64
        } else {
            0.0
        };
        let local = normalise([xyz[0] as f64, xyz[1] as f64, xyz[2] as f64, w]);
        local_positions.push(position);
        local_rotations.push(local);
        let offset = rotate(rotations[parent], position);
        positions.push(std::array::from_fn(|axis| {
            positions[parent][axis] + offset[axis]
        }));
        rotations.push(multiply(rotations[parent], local));
        parents.push(Some(parent));
    }
    let mut output = Vec::new();
    for index in 0..parents.len() {
        output.push(Bone {
            name: reader.string()?,
            parent: parents[index],
            position: positions[index],
            local_position: local_positions[index],
            local_rotation: local_rotations[index],
        });
    }
    // Stock parts have additional metadata after their bone names. Bind-pose
    // inspection is complete here, matching the native reference traversal.
    Ok(output)
}

/// One native surface and the material paired with it in the selected LOD.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSurface {
    pub material: String,
    pub geometry: Surface,
}

/// Static bind-pose mesh data. The FX player applies its independently proved
/// quaternion/origin/uniform-scale placement to this data; no unit conversion.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub name: String,
    pub lod: usize,
    pub lod_name: String,
    pub surfaces: Vec<ModelSurface>,
    pub bones: Vec<Bone>,
    pub bounds: [[f32; 3]; 2],
    pub geometry_convention: &'static str,
}

/// Read the highest-detail authored loose LOD. Materials are named references,
/// never guessed fallbacks. Their shader/image validation belongs to rendering.
pub fn read(game: &Path, name: &str) -> Result<Model, String> {
    let descriptor = descriptor(&asset_path(game, "xmodel", name)?)?;
    let lod = descriptor.lods.first().ok_or("Model has no authored LOD")?;
    let geometry = surfaces(&asset_path(game, "xmodelsurfs", &lod.name)?)?;
    if geometry.len() != lod.materials.len() {
        return Err("Model surface and material counts differ".into());
    }
    if !geometry.iter().any(Surface::has_visible_triangle) {
        return Err("Model has no nondegenerate indexed geometry".into());
    }
    let bones = bones(&asset_path(game, "xmodelparts", &lod.name)?)?;
    if bones.is_empty()
        || geometry
            .iter()
            .any(|surface| usize::from(surface.max_bone) >= bones.len())
    {
        return Err("Model geometry references a missing bind-pose bone".into());
    }
    let mut bounds = [[f32::INFINITY; 3], [f32::NEG_INFINITY; 3]];
    for surface in &geometry {
        for point in surface.positions.chunks_exact(3) {
            for axis in 0..3 {
                bounds[0][axis] = bounds[0][axis].min(point[axis]);
                bounds[1][axis] = bounds[1][axis].max(point[axis]);
            }
        }
    }
    let surfaces = geometry
        .into_iter()
        .zip(&lod.materials)
        .map(|(geometry, material)| ModelSurface {
            material: material.clone(),
            geometry,
        })
        .collect();
    Ok(Model {
        name: name.into(),
        lod: lod.slot,
        lod_name: lod.name.clone(),
        surfaces,
        bones,
        bounds,
        geometry_convention: "native WaW XYZ and units; stored UV; BGRA; clockwise indices",
    })
}
