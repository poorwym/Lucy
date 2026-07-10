use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::{Map, Value, json};

use crate::mesh::{MeshVertex, TriangleMesh};
use crate::source::MAX_PICKABLE_FEATURES_PER_TILE;

const GLB_MAGIC: u32 = 0x4654_6C67;
const GLB_VERSION: u32 = 2;
const JSON_CHUNK_TYPE: u32 = 0x4E4F_534A;
const BIN_CHUNK_TYPE: u32 = 0x004E_4942;
const FLOAT_COMPONENT_TYPE: u32 = 5126;
const UNSIGNED_INT_COMPONENT_TYPE: u32 = 5125;
const ARRAY_BUFFER_TARGET: u32 = 34962;
const ELEMENT_ARRAY_BUFFER_TARGET: u32 = 34963;
const TRIANGLES_MODE: u32 = 4;
const BYTE_ALIGNMENT: usize = 4;
const GLB_HEADER_LEN: usize = 12;
const CHUNK_HEADER_LEN: usize = 8;
const INDEX_BYTE_LEN: usize = 4;
const POSITION_COMPONENTS: usize = 3;
const POSITION_BYTE_LEN: usize = POSITION_COMPONENTS * 4;
const COLOR_BYTE_LEN: usize = 4 * 4;
const FEATURE_ID_BYTE_LEN: usize = 4;
const FEATURE_ID_PROPERTY: &str = "featureId";
const NULL_STRING_SENTINEL: &str = "\0";

#[derive(Debug, Clone, PartialEq)]
pub struct ContentFeature {
    pub id: String,
    pub mesh: TriangleMesh,
    pub base_color: [f32; 4],
    pub properties: BTreeMap<String, Option<String>>,
}

/// Encode one internal mesh as a glTF 2.0 binary GLB content tile.
pub fn encode_mesh_glb(mesh: &TriangleMesh) -> Result<Vec<u8>, GlbError> {
    encode_validated_mesh_glb(mesh)
}

/// Encode one content tile from one or more internal feature meshes.
pub fn encode_content_tile_glb(meshes: &[TriangleMesh]) -> Result<Vec<u8>, GlbError> {
    if meshes.is_empty() {
        return Err(GlbError::EmptyMesh);
    }

    let mut tile_mesh = TriangleMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    };

    for mesh in meshes {
        validate_mesh(mesh)?;
        let base_index = u32::try_from(tile_mesh.vertices.len()).map_err(|_| {
            GlbError::InvalidMesh("combined tile vertex count exceeds u32 index range".to_string())
        })?;

        tile_mesh.vertices.extend_from_slice(&mesh.vertices);
        for index in &mesh.indices {
            tile_mesh
                .indices
                .push(index.checked_add(base_index).ok_or_else(|| {
                    GlbError::InvalidMesh("combined tile index overflowed u32".to_string())
                })?);
        }
    }

    encode_validated_mesh_glb(&tile_mesh)
}

/// Encode feature-aware 3D Tiles content with vertex colors, feature IDs, and
/// an embedded structural metadata property table.
pub fn encode_feature_content_tile_glb(features: &[ContentFeature]) -> Result<Vec<u8>, GlbError> {
    if features.is_empty() {
        return Err(GlbError::EmptyMesh);
    }
    if features.len() > MAX_PICKABLE_FEATURES_PER_TILE as usize {
        return Err(GlbError::InvalidFeature(format!(
            "feature count {} exceeds the exact FLOAT feature ID limit {MAX_PICKABLE_FEATURES_PER_TILE}",
            features.len()
        )));
    }

    let mut tile_mesh = TriangleMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    };
    let mut vertex_colors = Vec::new();
    let mut vertex_feature_ids = Vec::new();
    let mut property_names = BTreeSet::new();
    let mut source_ids = BTreeSet::new();
    let mut uses_blending = false;

    for (feature_index, feature) in features.iter().enumerate() {
        validate_content_feature(feature, &mut source_ids, &mut property_names)?;
        validate_mesh(&feature.mesh)?;

        let base_index = u32::try_from(tile_mesh.vertices.len()).map_err(|_| {
            GlbError::InvalidMesh("combined tile vertex count exceeds u32 index range".to_string())
        })?;
        tile_mesh.vertices.extend_from_slice(&feature.mesh.vertices);
        vertex_colors.extend(std::iter::repeat_n(
            feature.base_color,
            feature.mesh.vertices.len(),
        ));
        vertex_feature_ids.extend(std::iter::repeat_n(
            feature_index as f32,
            feature.mesh.vertices.len(),
        ));
        uses_blending |= feature.base_color[3] < 1.0;

        for index in &feature.mesh.indices {
            tile_mesh
                .indices
                .push(index.checked_add(base_index).ok_or_else(|| {
                    GlbError::InvalidMesh("combined tile index overflowed u32".to_string())
                })?);
        }
    }

    let mut binary = Vec::new();
    let mut buffer_views = Vec::new();

    let mut index_bytes = Vec::with_capacity(tile_mesh.indices.len() * INDEX_BYTE_LEN);
    for index in &tile_mesh.indices {
        index_bytes.extend_from_slice(&index.to_le_bytes());
    }
    let index_view = append_buffer_view(
        &mut binary,
        &mut buffer_views,
        &index_bytes,
        BYTE_ALIGNMENT,
        Some(ELEMENT_ARRAY_BUFFER_TARGET),
        None,
    );

    let mut position_bytes = Vec::with_capacity(tile_mesh.vertices.len() * POSITION_BYTE_LEN);
    for vertex in &tile_mesh.vertices {
        for component in gltf_position(vertex.position) {
            position_bytes.extend_from_slice(&component.to_le_bytes());
        }
    }
    let position_view = append_buffer_view(
        &mut binary,
        &mut buffer_views,
        &position_bytes,
        BYTE_ALIGNMENT,
        Some(ARRAY_BUFFER_TARGET),
        Some(POSITION_BYTE_LEN),
    );

    let mut color_bytes = Vec::with_capacity(vertex_colors.len() * COLOR_BYTE_LEN);
    for color in &vertex_colors {
        for component in color {
            color_bytes.extend_from_slice(&component.to_le_bytes());
        }
    }
    let color_view = append_buffer_view(
        &mut binary,
        &mut buffer_views,
        &color_bytes,
        BYTE_ALIGNMENT,
        Some(ARRAY_BUFFER_TARGET),
        Some(COLOR_BYTE_LEN),
    );

    let mut feature_id_bytes = Vec::with_capacity(vertex_feature_ids.len() * FEATURE_ID_BYTE_LEN);
    for feature_id in &vertex_feature_ids {
        feature_id_bytes.extend_from_slice(&feature_id.to_le_bytes());
    }
    let feature_id_view = append_buffer_view(
        &mut binary,
        &mut buffer_views,
        &feature_id_bytes,
        BYTE_ALIGNMENT,
        Some(ARRAY_BUFFER_TARGET),
        Some(FEATURE_ID_BYTE_LEN),
    );

    let mut schema_properties = Map::new();
    let mut table_properties = Map::new();
    let feature_id_values = features
        .iter()
        .map(|feature| feature.id.as_str())
        .collect::<Vec<_>>();
    schema_properties.insert(
        FEATURE_ID_PROPERTY.to_string(),
        json!({
            "name": "Source feature identifier",
            "type": "STRING",
            "required": true
        }),
    );
    table_properties.insert(
        FEATURE_ID_PROPERTY.to_string(),
        append_string_property(&mut binary, &mut buffer_views, &feature_id_values)?,
    );

    for property_name in property_names {
        let values = features
            .iter()
            .map(|feature| {
                feature
                    .properties
                    .get(&property_name)
                    .and_then(Option::as_deref)
                    .unwrap_or(NULL_STRING_SENTINEL)
            })
            .collect::<Vec<_>>();
        schema_properties.insert(
            property_name.clone(),
            json!({
                "name": property_name,
                "type": "STRING",
                "noData": NULL_STRING_SENTINEL
            }),
        );
        table_properties.insert(
            property_name,
            append_string_property(&mut binary, &mut buffer_views, &values)?,
        );
    }

    let bounds = position_bounds(&tile_mesh.vertices);
    let binary_byte_length = align_len(binary.len(), BYTE_ALIGNMENT);
    let last_feature_id = (features.len() - 1) as f32;
    let alpha_mode = if uses_blending { "BLEND" } else { "OPAQUE" };
    let document = json!({
        "asset": {
            "version": "2.0",
            "generator": "lucy-poc"
        },
        "extensionsUsed": ["EXT_mesh_features", "EXT_structural_metadata"],
        "extensions": {
            "EXT_structural_metadata": {
                "schema": {
                    "id": "lucy_content_features",
                    "name": "Lucy content features",
                    "classes": {
                        "feature": {
                            "name": "Feature",
                            "properties": schema_properties
                        }
                    }
                },
                "propertyTables": [
                    {
                        "name": "Content features",
                        "class": "feature",
                        "count": features.len(),
                        "properties": table_properties
                    }
                ]
            }
        },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0 }],
        "materials": [
            {
                "name": "Lucy feature colors",
                "pbrMetallicRoughness": {
                    "baseColorFactor": [1.0, 1.0, 1.0, 1.0],
                    "metallicFactor": 0.0,
                    "roughnessFactor": 1.0
                },
                "alphaMode": alpha_mode
            }
        ],
        "meshes": [
            {
                "primitives": [
                    {
                        "attributes": {
                            "POSITION": 1,
                            "COLOR_0": 2,
                            "_FEATURE_ID_0": 3
                        },
                        "indices": 0,
                        "material": 0,
                        "mode": TRIANGLES_MODE,
                        "extensions": {
                            "EXT_mesh_features": {
                                "featureIds": [
                                    {
                                        "featureCount": features.len(),
                                        "attribute": 0,
                                        "propertyTable": 0,
                                        "label": "feature"
                                    }
                                ]
                            }
                        }
                    }
                ]
            }
        ],
        "buffers": [{ "byteLength": binary_byte_length }],
        "bufferViews": buffer_views,
        "accessors": [
            {
                "bufferView": index_view,
                "byteOffset": 0,
                "componentType": UNSIGNED_INT_COMPONENT_TYPE,
                "count": tile_mesh.indices.len(),
                "type": "SCALAR"
            },
            {
                "bufferView": position_view,
                "byteOffset": 0,
                "componentType": FLOAT_COMPONENT_TYPE,
                "count": tile_mesh.vertices.len(),
                "type": "VEC3",
                "min": bounds.min,
                "max": bounds.max
            },
            {
                "bufferView": color_view,
                "byteOffset": 0,
                "componentType": FLOAT_COMPONENT_TYPE,
                "count": vertex_colors.len(),
                "type": "VEC4"
            },
            {
                "bufferView": feature_id_view,
                "byteOffset": 0,
                "componentType": FLOAT_COMPONENT_TYPE,
                "count": vertex_feature_ids.len(),
                "type": "SCALAR",
                "min": [0.0],
                "max": [last_feature_id]
            }
        ]
    });

    encode_glb_document(document, binary)
}

fn encode_validated_mesh_glb(mesh: &TriangleMesh) -> Result<Vec<u8>, GlbError> {
    validate_mesh(mesh)?;

    let index_byte_offset = 0_usize;
    let index_byte_length = mesh.indices.len() * INDEX_BYTE_LEN;
    let position_byte_offset = align_len(index_byte_length, BYTE_ALIGNMENT);
    let position_byte_length = mesh.vertices.len() * POSITION_BYTE_LEN;
    let binary_byte_length = position_byte_offset + position_byte_length;

    let mut binary = Vec::with_capacity(binary_byte_length);
    for index in &mesh.indices {
        binary.extend_from_slice(&index.to_le_bytes());
    }
    binary.extend(std::iter::repeat_n(0, position_byte_offset - binary.len()));
    for vertex in &mesh.vertices {
        for component in gltf_position(vertex.position) {
            binary.extend_from_slice(&component.to_le_bytes());
        }
    }

    let bounds = position_bounds(&mesh.vertices);
    let document = json!({
        "asset": {
            "version": "2.0",
            "generator": "lucy-poc"
        },
        "scene": 0,
        "scenes": [
            {
                "nodes": [0]
            }
        ],
        "nodes": [
            {
                "mesh": 0
            }
        ],
        "meshes": [
            {
                "primitives": [
                    {
                        "attributes": {
                            "POSITION": 1
                        },
                        "indices": 0,
                        "mode": TRIANGLES_MODE
                    }
                ]
            }
        ],
        "buffers": [
            {
                "byteLength": binary_byte_length
            }
        ],
        "bufferViews": [
            {
                "buffer": 0,
                "byteOffset": index_byte_offset,
                "byteLength": index_byte_length,
                "target": ELEMENT_ARRAY_BUFFER_TARGET
            },
            {
                "buffer": 0,
                "byteOffset": position_byte_offset,
                "byteLength": position_byte_length,
                "byteStride": POSITION_BYTE_LEN,
                "target": ARRAY_BUFFER_TARGET
            }
        ],
        "accessors": [
            {
                "bufferView": 0,
                "byteOffset": 0,
                "componentType": UNSIGNED_INT_COMPONENT_TYPE,
                "count": mesh.indices.len(),
                "type": "SCALAR"
            },
            {
                "bufferView": 1,
                "byteOffset": 0,
                "componentType": FLOAT_COMPONENT_TYPE,
                "count": mesh.vertices.len(),
                "type": "VEC3",
                "min": bounds.min,
                "max": bounds.max
            }
        ]
    });

    encode_glb_document(document, binary)
}

fn encode_glb_document(document: Value, mut binary: Vec<u8>) -> Result<Vec<u8>, GlbError> {
    let mut json_bytes = serde_json::to_vec(&document)
        .map_err(|error| GlbError::Encode(format!("failed to encode glTF JSON: {error}")))?;
    pad_bytes(&mut json_bytes, b' ', BYTE_ALIGNMENT);
    pad_bytes(&mut binary, 0, BYTE_ALIGNMENT);

    let total_length = GLB_HEADER_LEN
        .checked_add(CHUNK_HEADER_LEN)
        .and_then(|length| length.checked_add(json_bytes.len()))
        .and_then(|length| length.checked_add(CHUNK_HEADER_LEN))
        .and_then(|length| length.checked_add(binary.len()))
        .ok_or_else(|| GlbError::Encode("GLB length overflowed".to_string()))?;

    let total_length_u32 = u32::try_from(total_length)
        .map_err(|_| GlbError::Encode("GLB length exceeds u32".to_string()))?;

    let mut glb = Vec::with_capacity(total_length);
    glb.extend_from_slice(&GLB_MAGIC.to_le_bytes());
    glb.extend_from_slice(&GLB_VERSION.to_le_bytes());
    glb.extend_from_slice(&total_length_u32.to_le_bytes());
    append_chunk(&mut glb, JSON_CHUNK_TYPE, &json_bytes)?;
    append_chunk(&mut glb, BIN_CHUNK_TYPE, &binary)?;

    Ok(glb)
}

fn validate_content_feature(
    feature: &ContentFeature,
    source_ids: &mut BTreeSet<String>,
    property_names: &mut BTreeSet<String>,
) -> Result<(), GlbError> {
    if feature.id.is_empty() {
        return Err(GlbError::InvalidFeature(
            "source feature id must not be empty".to_string(),
        ));
    }
    if feature.id.contains(NULL_STRING_SENTINEL) {
        return Err(GlbError::InvalidFeature(format!(
            "source feature id {:?} contains the reserved NUL sentinel",
            feature.id
        )));
    }
    if !source_ids.insert(feature.id.clone()) {
        return Err(GlbError::InvalidFeature(format!(
            "duplicate source feature id {:?}",
            feature.id
        )));
    }

    for (component, value) in feature.base_color.iter().copied().enumerate() {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(GlbError::InvalidFeature(format!(
                "feature {:?} base color component {component} must be finite and within 0..=1",
                feature.id
            )));
        }
    }

    for (property, value) in &feature.properties {
        validate_metadata_identifier(property)?;
        if property == FEATURE_ID_PROPERTY {
            return Err(GlbError::InvalidFeature(format!(
                "metadata property {FEATURE_ID_PROPERTY:?} is reserved"
            )));
        }
        if value
            .as_deref()
            .is_some_and(|value| value.contains(NULL_STRING_SENTINEL))
        {
            return Err(GlbError::InvalidFeature(format!(
                "feature {:?} property {property:?} contains the reserved NUL sentinel",
                feature.id
            )));
        }
        property_names.insert(property.clone());
    }

    Ok(())
}

fn validate_metadata_identifier(identifier: &str) -> Result<(), GlbError> {
    let mut chars = identifier.chars();
    let Some(first) = chars.next() else {
        return Err(GlbError::InvalidFeature(
            "metadata property id must not be empty".to_string(),
        ));
    };
    if !(first == '_' || first.is_ascii_alphabetic())
        || !chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return Err(GlbError::InvalidFeature(format!(
            "metadata property id {identifier:?} must match ^[a-zA-Z_][a-zA-Z0-9_]*$"
        )));
    }
    Ok(())
}

fn append_string_property(
    binary: &mut Vec<u8>,
    buffer_views: &mut Vec<Value>,
    values: &[&str],
) -> Result<Value, GlbError> {
    let mut string_bytes = Vec::new();
    let mut offset_bytes = Vec::with_capacity((values.len() + 1) * std::mem::size_of::<u32>());
    offset_bytes.extend_from_slice(&0_u32.to_le_bytes());

    for value in values {
        string_bytes.extend_from_slice(value.as_bytes());
        let offset = u32::try_from(string_bytes.len()).map_err(|_| {
            GlbError::InvalidFeature("metadata string column exceeds UINT32 offsets".to_string())
        })?;
        offset_bytes.extend_from_slice(&offset.to_le_bytes());
    }

    let values_view = append_buffer_view(binary, buffer_views, &string_bytes, 1, None, None);
    let offsets_view = append_buffer_view(
        binary,
        buffer_views,
        &offset_bytes,
        std::mem::align_of::<u32>(),
        None,
        None,
    );

    Ok(json!({
        "values": values_view,
        "stringOffsets": offsets_view,
        "stringOffsetType": "UINT32"
    }))
}

fn append_buffer_view(
    binary: &mut Vec<u8>,
    buffer_views: &mut Vec<Value>,
    data: &[u8],
    alignment: usize,
    target: Option<u32>,
    byte_stride: Option<usize>,
) -> usize {
    pad_bytes(binary, 0, alignment);
    let byte_offset = binary.len();
    let byte_length = data.len().max(1);
    if data.is_empty() {
        binary.push(0);
    } else {
        binary.extend_from_slice(data);
    }

    let mut view = Map::new();
    view.insert("buffer".to_string(), json!(0));
    view.insert("byteOffset".to_string(), json!(byte_offset));
    view.insert("byteLength".to_string(), json!(byte_length));
    if let Some(target) = target {
        view.insert("target".to_string(), json!(target));
    }
    if let Some(byte_stride) = byte_stride {
        view.insert("byteStride".to_string(), json!(byte_stride));
    }

    let index = buffer_views.len();
    buffer_views.push(Value::Object(view));
    index
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlbError {
    EmptyMesh,
    InvalidMesh(String),
    InvalidFeature(String),
    Encode(String),
}

impl fmt::Display for GlbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GlbError::EmptyMesh => write!(f, "cannot encode an empty mesh as GLB"),
            GlbError::InvalidMesh(message) => write!(f, "invalid mesh for GLB encoding: {message}"),
            GlbError::InvalidFeature(message) => {
                write!(f, "invalid feature content for GLB encoding: {message}")
            }
            GlbError::Encode(message) => write!(f, "failed to encode GLB: {message}"),
        }
    }
}

impl std::error::Error for GlbError {}

struct PositionBounds {
    min: [f32; 3],
    max: [f32; 3],
}

fn validate_mesh(mesh: &TriangleMesh) -> Result<(), GlbError> {
    if mesh.vertices.is_empty() || mesh.indices.is_empty() {
        return Err(GlbError::EmptyMesh);
    }

    if !mesh.indices.len().is_multiple_of(3) {
        return Err(GlbError::InvalidMesh(
            "index count must be a multiple of 3 for triangle primitives".to_string(),
        ));
    }

    let vertex_count = u32::try_from(mesh.vertices.len())
        .map_err(|_| GlbError::InvalidMesh("vertex count exceeds u32 index range".to_string()))?;

    for (index, vertex) in mesh.vertices.iter().enumerate() {
        if !vertex
            .position
            .iter()
            .all(|component| component.is_finite())
        {
            return Err(GlbError::InvalidMesh(format!(
                "vertex {index} position contains a non-finite component"
            )));
        }
    }

    for (offset, index) in mesh.indices.iter().copied().enumerate() {
        if index >= vertex_count {
            return Err(GlbError::InvalidMesh(format!(
                "index {offset} references vertex {index}, but vertex count is {vertex_count}"
            )));
        }
    }

    Ok(())
}

fn position_bounds(vertices: &[MeshVertex]) -> PositionBounds {
    let mut min = gltf_position(vertices[0].position);
    let mut max = min;

    for vertex in vertices.iter().skip(1) {
        let position = gltf_position(vertex.position);
        for component in 0..POSITION_COMPONENTS {
            min[component] = min[component].min(position[component]);
            max[component] = max[component].max(position[component]);
        }
    }

    PositionBounds { min, max }
}

fn gltf_position(enu_position: [f32; 3]) -> [f32; 3] {
    let [east, north, up] = enu_position;
    [east, up, -north]
}

fn append_chunk(glb: &mut Vec<u8>, chunk_type: u32, chunk: &[u8]) -> Result<(), GlbError> {
    let chunk_length = u32::try_from(chunk.len())
        .map_err(|_| GlbError::Encode("GLB chunk length exceeds u32".to_string()))?;
    glb.extend_from_slice(&chunk_length.to_le_bytes());
    glb.extend_from_slice(&chunk_type.to_le_bytes());
    glb.extend_from_slice(chunk);
    Ok(())
}

fn pad_bytes(bytes: &mut Vec<u8>, padding: u8, alignment: usize) {
    bytes.extend(std::iter::repeat_n(
        padding,
        align_len(bytes.len(), alignment) - bytes.len(),
    ));
}

fn align_len(length: usize, alignment: usize) -> usize {
    length.div_ceil(alignment) * alignment
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn fixture_mesh() -> TriangleMesh {
        TriangleMesh {
            vertices: vec![
                MeshVertex {
                    position: [0.0, 0.0, 0.0],
                },
                MeshVertex {
                    position: [1.0, 0.0, 0.0],
                },
                MeshVertex {
                    position: [1.0, 1.0, 0.0],
                },
                MeshVertex {
                    position: [0.0, 1.0, 0.0],
                },
            ],
            indices: vec![0, 1, 2, 0, 2, 3],
        }
    }

    fn fixture_content_features() -> Vec<ContentFeature> {
        let first = ContentFeature {
            id: "101".to_string(),
            mesh: fixture_mesh(),
            base_color: [0.72, 0.70, 0.65, 1.0],
            properties: BTreeMap::from([
                ("name".to_string(), Some("Alpha".to_string())),
                ("building_type".to_string(), None),
            ]),
        };
        let mut second_mesh = fixture_mesh();
        for vertex in &mut second_mesh.vertices {
            vertex.position[0] += 2.0;
        }
        let second = ContentFeature {
            id: "202".to_string(),
            mesh: second_mesh,
            base_color: [0.25, 0.5, 0.75, 0.5],
            properties: BTreeMap::from([
                ("name".to_string(), Some("Beta".to_string())),
                ("building_type".to_string(), Some("office".to_string())),
            ]),
        };

        vec![first, second]
    }

    #[test]
    fn encodes_valid_glb_header_chunks_and_gltf_json() {
        let glb = encode_mesh_glb(&fixture_mesh()).expect("GLB should encode");
        let parsed = parse_glb(&glb);

        assert_eq!(parsed.magic, GLB_MAGIC);
        assert_eq!(parsed.version, GLB_VERSION);
        assert_eq!(parsed.length as usize, glb.len());
        assert_eq!(parsed.json_chunk_type, JSON_CHUNK_TYPE);
        assert_eq!(parsed.bin_chunk_type, BIN_CHUNK_TYPE);
        assert_eq!(parsed.bin.len(), 72);
        assert_eq!(parsed.document["asset"]["version"], "2.0");
        assert_eq!(parsed.document["scene"], 0);
        assert_eq!(parsed.document["buffers"][0]["byteLength"], 72);
        assert_eq!(parsed.document["bufferViews"][0]["byteLength"], 24);
        assert_eq!(parsed.document["bufferViews"][1]["byteOffset"], 24);
        assert_eq!(parsed.document["accessors"][0]["componentType"], 5125);
        assert_eq!(parsed.document["accessors"][0]["count"], 6);
        assert_eq!(parsed.document["accessors"][1]["componentType"], 5126);
        assert_eq!(parsed.document["accessors"][1]["count"], 4);
        assert_eq!(
            parsed.document["accessors"][1]["min"],
            json!([0.0, 0.0, -1.0])
        );
        assert_eq!(
            parsed.document["accessors"][1]["max"],
            json!([1.0, 0.0, -0.0])
        );
    }

    #[test]
    fn binary_chunk_contains_little_endian_indices_and_positions() {
        let glb = encode_mesh_glb(&fixture_mesh()).expect("GLB should encode");
        let parsed = parse_glb(&glb);

        let indices = parsed.bin[0..24]
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("u32 chunk")))
            .collect::<Vec<_>>();
        assert_eq!(indices, vec![0, 1, 2, 0, 2, 3]);

        let positions = parsed.bin[24..]
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("f32 chunk")))
            .collect::<Vec<_>>();
        assert_eq!(
            positions,
            vec![
                0.0, 0.0, -0.0, 1.0, 0.0, -0.0, 1.0, 0.0, -1.0, 0.0, 0.0, -1.0
            ]
        );
    }

    #[test]
    fn content_tile_glb_concatenates_multiple_meshes() {
        let first = fixture_mesh();
        let mut second = fixture_mesh();
        for vertex in &mut second.vertices {
            vertex.position[0] += 2.0;
        }

        let glb = encode_content_tile_glb(&[first, second]).expect("tile GLB should encode");
        let parsed = parse_glb(&glb);

        assert_eq!(parsed.document["accessors"][0]["count"], 12);
        assert_eq!(parsed.document["accessors"][1]["count"], 8);

        let indices = parsed.bin[0..48]
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("u32 chunk")))
            .collect::<Vec<_>>();
        assert_eq!(indices, vec![0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7]);
    }

    #[test]
    fn feature_content_emits_material_colors_and_pickable_metadata() {
        let features = fixture_content_features();
        let glb = encode_feature_content_tile_glb(&features).expect("feature GLB should encode");
        let parsed = parse_glb(&glb);
        let document = &parsed.document;

        assert_eq!(
            document["extensionsUsed"],
            json!(["EXT_mesh_features", "EXT_structural_metadata"])
        );
        assert!(document.get("extensionsRequired").is_none());
        assert_eq!(
            document["materials"][0]["pbrMetallicRoughness"]["baseColorFactor"],
            json!([1.0, 1.0, 1.0, 1.0])
        );
        assert_eq!(document["materials"][0]["alphaMode"], "BLEND");

        let primitive = &document["meshes"][0]["primitives"][0];
        assert_eq!(primitive["attributes"]["POSITION"], 1);
        assert_eq!(primitive["attributes"]["COLOR_0"], 2);
        assert_eq!(primitive["attributes"]["_FEATURE_ID_0"], 3);
        assert_eq!(
            primitive["extensions"]["EXT_mesh_features"]["featureIds"][0],
            json!({
                "featureCount": 2,
                "attribute": 0,
                "propertyTable": 0,
                "label": "feature"
            })
        );

        let colors = read_f32_accessor(&parsed, 2, 4);
        assert_eq!(&colors[0..4], &[0.72, 0.70, 0.65, 1.0]);
        assert_eq!(&colors[12..16], &[0.72, 0.70, 0.65, 1.0]);
        assert_eq!(&colors[16..20], &[0.25, 0.5, 0.75, 0.5]);
        assert_eq!(&colors[28..32], &[0.25, 0.5, 0.75, 0.5]);

        let feature_ids = read_f32_accessor(&parsed, 3, 1);
        assert_eq!(feature_ids, vec![0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0]);

        let metadata = &document["extensions"]["EXT_structural_metadata"];
        assert_eq!(metadata["schema"]["id"], "lucy_content_features");
        assert_eq!(metadata["propertyTables"][0]["count"], 2);
        assert_eq!(
            metadata["schema"]["classes"]["feature"]["properties"]["featureId"]["required"],
            true
        );
        assert_eq!(
            metadata["schema"]["classes"]["feature"]["properties"]["building_type"]["noData"],
            NULL_STRING_SENTINEL
        );
        assert_eq!(
            read_string_property(&parsed, FEATURE_ID_PROPERTY),
            vec!["101", "202"]
        );
        assert_eq!(read_string_property(&parsed, "name"), vec!["Alpha", "Beta"]);
        assert_eq!(
            read_string_property(&parsed, "building_type"),
            vec![NULL_STRING_SENTINEL, "office"]
        );
    }

    #[test]
    fn feature_content_rejects_invalid_colors_and_metadata_ids() {
        let mut features = fixture_content_features();
        features[0].base_color[0] = 1.5;
        let error = encode_feature_content_tile_glb(&features).expect_err("color should fail");
        assert!(error.to_string().contains("base color component 0"));

        let mut features = fixture_content_features();
        features[0]
            .properties
            .insert("bad-property".to_string(), Some("x".to_string()));
        let error = encode_feature_content_tile_glb(&features).expect_err("property should fail");
        assert!(error.to_string().contains("must match"));
    }

    #[test]
    fn rejects_empty_mesh() {
        let error = encode_mesh_glb(&TriangleMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        })
        .expect_err("empty mesh should fail");

        assert_eq!(error, GlbError::EmptyMesh);
    }

    #[test]
    fn rejects_out_of_range_indices() {
        let mut mesh = fixture_mesh();
        mesh.indices[2] = 4;

        let error = encode_mesh_glb(&mesh).expect_err("invalid index should fail");
        assert!(
            error.to_string().contains("references vertex 4"),
            "unexpected error: {error}"
        );
    }

    struct ParsedGlb {
        magic: u32,
        version: u32,
        length: u32,
        json_chunk_type: u32,
        bin_chunk_type: u32,
        document: Value,
        bin: Vec<u8>,
    }

    fn read_f32_accessor(parsed: &ParsedGlb, accessor_index: usize, components: usize) -> Vec<f32> {
        let accessor = &parsed.document["accessors"][accessor_index];
        let view_index = accessor["bufferView"].as_u64().expect("bufferView") as usize;
        let view = &parsed.document["bufferViews"][view_index];
        let start = view["byteOffset"].as_u64().expect("view offset") as usize
            + accessor["byteOffset"].as_u64().unwrap_or(0) as usize;
        let count = accessor["count"].as_u64().expect("accessor count") as usize;
        let byte_length = count * components * std::mem::size_of::<f32>();

        parsed.bin[start..start + byte_length]
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("f32 chunk")))
            .collect()
    }

    fn read_string_property(parsed: &ParsedGlb, property: &str) -> Vec<String> {
        let table = &parsed.document["extensions"]["EXT_structural_metadata"]["propertyTables"][0];
        let property = &table["properties"][property];
        let values_view = property["values"].as_u64().expect("values view") as usize;
        let offsets_view = property["stringOffsets"].as_u64().expect("offsets view") as usize;
        let values = buffer_view_bytes(parsed, values_view);
        let offsets = buffer_view_bytes(parsed, offsets_view)
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("u32 offset")) as usize)
            .collect::<Vec<_>>();

        offsets
            .windows(2)
            .map(|range| {
                std::str::from_utf8(&values[range[0]..range[1]])
                    .expect("valid UTF-8 metadata")
                    .to_string()
            })
            .collect()
    }

    fn buffer_view_bytes(parsed: &ParsedGlb, view_index: usize) -> &[u8] {
        let view = &parsed.document["bufferViews"][view_index];
        let start = view["byteOffset"].as_u64().expect("view offset") as usize;
        let length = view["byteLength"].as_u64().expect("view length") as usize;
        &parsed.bin[start..start + length]
    }

    fn parse_glb(glb: &[u8]) -> ParsedGlb {
        let magic = u32::from_le_bytes(glb[0..4].try_into().expect("magic"));
        let version = u32::from_le_bytes(glb[4..8].try_into().expect("version"));
        let length = u32::from_le_bytes(glb[8..12].try_into().expect("length"));
        let json_len = u32::from_le_bytes(glb[12..16].try_into().expect("json len")) as usize;
        let json_chunk_type = u32::from_le_bytes(glb[16..20].try_into().expect("json type"));
        let json_start = 20;
        let json_end = json_start + json_len;
        let document =
            serde_json::from_slice::<Value>(&glb[json_start..json_end]).expect("parse padded JSON");
        let bin_len =
            u32::from_le_bytes(glb[json_end..json_end + 4].try_into().expect("bin len")) as usize;
        let bin_chunk_type = u32::from_le_bytes(
            glb[json_end + 4..json_end + 8]
                .try_into()
                .expect("bin type"),
        );
        let bin_start = json_end + 8;
        let bin = glb[bin_start..bin_start + bin_len].to_vec();

        ParsedGlb {
            magic,
            version,
            length,
            json_chunk_type,
            bin_chunk_type,
            document,
            bin,
        }
    }
}
