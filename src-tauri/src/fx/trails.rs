//! Trail mesh preview, adapted from the pinned MIT iw3xo `fx_convert.cpp`
//! and `fx_draw.cpp`. Native WaW compiler/writer checks live in trail-checks.
//!
//! A trail is an indexed ribbon/profile swept through emitter history. It is
//! not the camera-facing single quad called a `tail`. This module accepts
//! already sampled segment visual state; it does not invent emitter movement.

#[derive(Clone, Debug)]
pub struct ProfileVertex {
    pub position: [f32; 2],
    pub normal: [f32; 2],
    pub texture_v: f32,
}

#[derive(Clone, Debug)]
pub struct TrailDefinition {
    pub vertices: Vec<ProfileVertex>,
    pub edges: Vec<u16>,
    pub split_distance: i32,
    pub repeat_distance: i32,
    pub scroll_msec: i32,
}

#[derive(Clone, Debug)]
pub struct SegmentState {
    pub position: [f32; 3],
    pub basis: [[f32; 3]; 2],
    pub rotation: f32,
    pub size: [f32; 2],
    pub texture_u: f32,
    /// Native vertex bytes, in BGRA order, not float RGBA.
    pub color: [u8; 4],
}

fn length(vector: [f32; 2]) -> f32 {
    (vector[0] * vector[0] + vector[1] * vector[1]).sqrt()
}

fn normalize(vector: [f32; 2]) -> [f32; 2] {
    let magnitude = length(vector);

    if magnitude == 0.0 {
        return vector;
    }

    [vector[0] / magnitude, vector[1] / magnitude]
}

fn distance(left: [f32; 2], right: [f32; 2]) -> f32 {
    length([left[0] - right[0], left[1] - right[1]])
}

fn edge_normal(vertices: &[[f32; 3]], edge: &[u16]) -> [f32; 2] {
    let first = vertices[usize::from(edge[0])];
    let second = vertices[usize::from(edge[1])];
    normalize([first[1] - second[1], second[0] - first[0]])
}

/// Compile the two authored trailDef blocks: triples (x, y, texture V), then
/// pairs of vertex indices defining profile edges. WaW generates smooth normals
/// and merges only vertices satisfying its position, V and normal tolerances.
pub fn compile_profile(
    source_vertices: &[[f32; 3]],
    source_edges: &[u16],
    split_distance: i32,
    repeat_distance: i32,
    scroll_seconds: f32,
) -> Result<TrailDefinition, String> {
    if source_vertices.len() > 64 || source_vertices.is_empty() {
        return Err("Trail profile requires 1 to 64 authored vertices".into());
    }

    if source_edges.is_empty() || source_edges.len() > 128 || source_edges.len() % 2 != 0 {
        return Err("Trail profile requires 1 to 64 edge pairs".into());
    }

    if source_edges
        .iter()
        .any(|index| usize::from(*index) >= source_vertices.len())
    {
        return Err("Trail edge points outside the authored profile".into());
    }

    if split_distance <= 0 || repeat_distance <= 0 || !scroll_seconds.is_finite() {
        return Err("Trail split/repeat distances must be positive; scroll must be finite".into());
    }

    if source_vertices
        .iter()
        .flatten()
        .any(|value| !value.is_finite())
    {
        return Err("Trail profile contains a non-finite value".into());
    }

    let mut expanded = Vec::with_capacity(source_edges.len());

    for primary_edge in source_edges.chunks_exact(2) {
        let primary_normal = edge_normal(source_vertices, primary_edge);

        for index in primary_edge {
            let source = source_vertices[usize::from(*index)];
            let position = [source[0], source[1]];
            let mut accumulated = [0.0; 2];

            for secondary_edge in source_edges.chunks_exact(2) {
                let first = source_vertices[usize::from(secondary_edge[0])];
                let second = source_vertices[usize::from(secondary_edge[1])];

                if distance(position, [first[0], first[1]]) > 0.01
                    && distance(position, [second[0], second[1]]) > 0.01
                {
                    continue;
                }

                let secondary_normal = edge_normal(source_vertices, secondary_edge);
                let dot = primary_normal[0] * secondary_normal[0]
                    + primary_normal[1] * secondary_normal[1];

                if dot > 0.0 {
                    accumulated[0] += secondary_normal[0];
                    accumulated[1] += secondary_normal[1];
                }
            }

            expanded.push(ProfileVertex {
                position,
                normal: normalize(accumulated),
                texture_v: source[2],
            });
        }
    }

    let mut vertices: Vec<ProfileVertex> = Vec::new();
    let mut edges = Vec::with_capacity(source_edges.len());

    for vertex in expanded {
        let existing = vertices.iter().position(|candidate| {
            distance(candidate.position, vertex.position) <= 0.01
                && (candidate.texture_v - vertex.texture_v).abs() <= 0.01
                && candidate.normal[0] * vertex.normal[0] + candidate.normal[1] * vertex.normal[1]
                    >= 0.95
        });

        let index = match existing {
            Some(index) => index,
            None => {
                vertices.push(vertex);
                vertices.len() - 1
            }
        };

        edges.push(index as u16);
    }

    Ok(TrailDefinition {
        vertices,
        edges,
        split_distance,
        repeat_distance,
        scroll_msec: (f64::from(scroll_seconds) * 1000.0) as i32,
    })
}

fn pack_texture(value: f32) -> u16 {
    let word = value.to_bits();
    let mantissa = (((word.wrapping_shl(1) ^ 0x80003fff) as i32) >> 14).clamp(-16384, 16383);
    ((mantissa as u32 & 0x3fff) | ((word >> 16) & 0xc000)) as u16
}

/// Emit native 32-byte D3D9 vertices. Stock trail writing leaves binormal-sign
/// bytes (+12) untouched; our owned output initializes them to zero. The original
/// trail shader's treatment of that unwritten field is a separate render check.
pub fn generate_ring(definition: &TrailDefinition, state: &SegmentState) -> Vec<u8> {
    let cosine = state.rotation.cos();
    let sine = state.rotation.sin();
    let left: [f32; 3] =
        std::array::from_fn(|axis| cosine * state.basis[0][axis] + sine * state.basis[1][axis]);
    let up: [f32; 3] =
        std::array::from_fn(|axis| sine * state.basis[0][axis] - cosine * state.basis[1][axis]);
    let mut bytes = Vec::with_capacity(definition.vertices.len() * 32);

    for vertex in &definition.vertices {
        let width = vertex.position[0] * state.size[0];
        let height = vertex.position[1] * state.size[1];

        for axis in 0..3 {
            // x87 retains intermediate products until this final f32 store.
            let coordinate = ((f64::from(width) * f64::from(left[axis]))
                + f64::from(state.position[axis])
                + f64::from(height) * f64::from(up[axis])) as f32;
            bytes.extend_from_slice(&coordinate.to_le_bytes());
        }

        bytes.extend_from_slice(&0.0_f32.to_le_bytes());
        bytes.extend_from_slice(&state.color);
        let packed = (u32::from(pack_texture(state.texture_u)) << 16)
            | u32::from(pack_texture(vertex.texture_v));
        bytes.extend_from_slice(&packed.to_le_bytes());

        for axis in 0..3 {
            let normal = (f64::from(vertex.normal[0]) * f64::from(left[axis])
                + f64::from(vertex.normal[1]) * f64::from(up[axis]))
                as f32;
            bytes.push((f64::from(normal) * 127.0 + 127.5) as i32 as u8);
        }

        bytes.push(63);
        bytes.extend_from_slice(&0x3f7f7ffe_u32.to_le_bytes());
    }

    bytes
}

/// Bridge adjacent profile rings. These six-index pairs retain WaW's actual
/// triangle winding, rather than assuming the ordinary sprite index order.
pub fn stitch_indices(definition: &TrailDefinition, rings: usize) -> Result<Vec<u16>, String> {
    let vertex_count = definition.vertices.len();

    if rings
        .checked_mul(vertex_count)
        .is_none_or(|count| count > 65536)
    {
        return Err("Trail mesh exceeds native 16-bit vertex indices".into());
    }

    let mut indices = Vec::new();

    for segment in 0..rings.saturating_sub(1) {
        let near = segment * vertex_count;
        let far = near + vertex_count;

        for edge in definition.edges.chunks_exact(2) {
            let first = usize::from(edge[0]);
            let second = usize::from(edge[1]);
            indices.extend_from_slice(&[
                (near + first) as u16,
                (near + second) as u16,
                (far + first) as u16,
                (far + second) as u16,
                (far + first) as u16,
                (near + second) as u16,
            ]);
        }
    }

    Ok(indices)
}

/// Native signed-byte orientation compression; deliberately no normalization.
pub fn compress_basis(basis: [[f32; 3]; 2]) -> [[i8; 3]; 2] {
    basis.map(|axis| {
        axis.map(|value| (f64::from(value) * 127.0) as i32)
            .map(|value| value.clamp(-128, 127) as i8)
    })
}

pub fn uncompress_basis(basis: [[i8; 3]; 2]) -> [[f32; 3]; 2] {
    basis.map(|axis| axis.map(|value| f32::from(value) * 0.0078740157))
}

/// Texture origin follows the first retained segment; direction uses signed
/// authored scroll time and C/Rust signed remainder, not a positive modulo.
pub fn texture_offset(definition: &TrailDefinition, first_distance: f32, draw_msec: i32) -> f32 {
    let mut offset = -(first_distance / definition.repeat_distance as f32).floor();

    if definition.scroll_msec < 0 {
        offset += 1.0 - (draw_msec % definition.scroll_msec) as f32 / definition.scroll_msec as f32;
    } else if definition.scroll_msec > 0 {
        offset -= (draw_msec % definition.scroll_msec) as f32 / definition.scroll_msec as f32;
    }

    offset
}

#[derive(Clone, Debug)]
pub struct DrawSegment {
    pub state: SegmentState,
    pub normalized_age: f32,
    pub spawn_distance: f32,
    pub sequence: u8,
}

#[derive(Clone, Debug)]
pub struct TrailMesh {
    pub vertices: Vec<u8>,
    pub indices: Vec<u16>,
    pub ring_count: usize,
}

/// Assemble an already sampled chronological trail list. The caller must retain
/// the last dead segment so WaW can interpolate the lifespan boundary into the
/// next live segment. It must also exclude points whose birth is in the future.
/// This is drawing/list clipping, not a substitute for native spawn/update.
pub fn assemble_mesh(
    definition: &TrailDefinition,
    segments: &[DrawSegment],
    draw_msec: i32,
) -> Result<TrailMesh, String> {
    let mut output = TrailMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
        ring_count: 0,
    };

    let Some(first) = segments.first() else {
        return Ok(output);
    };

    let offset = texture_offset(definition, first.spawn_distance, draw_msec);
    let mut previous_age = 1.0_f32;
    let mut previous_state: Option<SegmentState> = None;

    for (index, segment) in segments.iter().enumerate() {
        let mut state = segment.state.clone();
        state.texture_u = (f64::from(segment.spawn_distance)
            / f64::from(definition.repeat_distance)
            + f64::from(offset)) as f32;

        if segment.normalized_age < 1.0 {
            if index == 0 {
                if segment.sequence == 0 {
                    state.color[3] = 0;
                }
            } else {
                if previous_age >= 1.0 {
                    let mut boundary = previous_state
                        .clone()
                        .ok_or("Missing previous trail segment")?;
                    let blend = ((1.0_f64 - f64::from(previous_age))
                        / (f64::from(segment.normalized_age) - f64::from(previous_age)))
                        as f32;
                    boundary.texture_u = (f64::from(boundary.texture_u)
                        + (f64::from(state.texture_u) - f64::from(boundary.texture_u))
                            * f64::from(blend)) as f32;

                    for axis in 0..3 {
                        boundary.position[axis] = (f64::from(boundary.position[axis])
                            + (f64::from(state.position[axis])
                                - f64::from(boundary.position[axis]))
                                * f64::from(blend))
                            as f32;

                        for basis in 0..2 {
                            boundary.basis[basis][axis] = (f64::from(boundary.basis[basis][axis])
                                + (f64::from(state.basis[basis][axis])
                                    - f64::from(boundary.basis[basis][axis]))
                                    * f64::from(blend))
                                as f32;
                        }
                    }

                    output.vertices.extend(generate_ring(definition, &boundary));
                    output.ring_count += 1;
                }

                // Unlike the COD4-derived source, WaW does not clear the
                // final segment alpha. Native 0x452960 goes straight to
                // ring emission after the expired-tail interpolation.
            }

            output.vertices.extend(generate_ring(definition, &state));
            output.ring_count += 1;
        }

        previous_age = segment.normalized_age;
        previous_state = Some(state);
    }

    output.indices = stitch_indices(definition, output.ring_count)?;
    Ok(output)
}

#[derive(Clone, Debug)]
pub struct SplitRequest {
    pub begin_msec: i32,
    pub distance: f32,
    pub origin: [f32; 3],
}

/// Distance crossings for a source translating linearly with fixed orientation.
/// A crossing exactly at the update endpoint is deferred, matching native's
/// strict `remaining > before_next` loop. Rotation/frame interpolation is not
/// claimed here; callers must use the fixed-orientation restriction explicitly.
pub fn split_requests(
    definition: &TrailDefinition,
    begin_distance: f32,
    end_distance: f32,
    begin_msec: i32,
    end_msec: i32,
    begin_origin: [f32; 3],
    end_origin: [f32; 3],
) -> Result<Vec<SplitRequest>, String> {
    if !begin_distance.is_finite()
        || !end_distance.is_finite()
        || begin_distance < 0.0
        || end_distance < begin_distance
        || end_msec < begin_msec
        || definition.split_distance <= 0
    {
        return Err("Invalid trail source distance/time interval".into());
    }

    let total_distance = f64::from(end_distance) - f64::from(begin_distance);
    let total = (total_distance / f64::from(definition.split_distance)) as f32;
    // Native stores the quotient and its fractional part as f32 before
    // testing a crossing. Retaining extra precision can lose a real split.
    let normalized_begin =
        (f64::from(begin_distance) / f64::from(definition.split_distance)) as f32;
    let floor = normalized_begin.floor();
    let fraction = (f64::from(normalized_begin) - f64::from(floor)) as f32;
    let mut before_next = (1.0 - f64::from(fraction)) as f32;
    let mut traversed = 0.0_f32;
    let mut remaining = total;
    let mut requests = Vec::new();

    while remaining > before_next {
        if requests.len() >= 2048 {
            return Err("Trail update exceeds the native trail-point pool capacity".into());
        }

        traversed += before_next;
        let blend = (f64::from(traversed) / f64::from(total)) as f32;
        let distance = (total_distance * f64::from(blend) + f64::from(begin_distance)) as f32;
        let origin = std::array::from_fn(|axis| {
            ((f64::from(end_origin[axis]) - f64::from(begin_origin[axis])) * f64::from(blend)
                + f64::from(begin_origin[axis])) as f32
        });
        let time = (f64::from(end_msec - begin_msec) * f64::from(blend)) as i32 + begin_msec;
        requests.push(SplitRequest {
            begin_msec: time,
            distance,
            origin,
        });
        remaining -= before_next;
        before_next = 1.0;
    }

    Ok(requests)
}
