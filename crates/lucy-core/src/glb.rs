use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use draco_core::{
    DataType, EncoderBuffer, EncoderOptions, GeometryAttributeType, Mesh as DracoMesh, MeshEncoder,
    PointAttribute,
};
use serde_json::{Map, Value, json};

use crate::mesh::{MeshVertex, TriangleMesh};
use crate::source::{Compression, MAX_PICKABLE_FEATURES_PER_TILE};

const GLB_MAGIC: u32 = 0x4654_6C67;
const GLB_VERSION: u32 = 2;
const JSON_CHUNK_TYPE: u32 = 0x4E4F_534A;
const BIN_CHUNK_TYPE: u32 = 0x004E_4942;
const BYTE_COMPONENT_TYPE: u32 = 5120;
const UNSIGNED_SHORT_COMPONENT_TYPE: u32 = 5123;
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
const QUANTIZED_POSITION_BYTE_STRIDE: usize = 8;
const NORMAL_BYTE_LEN: usize = POSITION_COMPONENTS * 4;
const QUANTIZED_NORMAL_BYTE_STRIDE: usize = 4;
const COLOR_BYTE_LEN: usize = 4 * 4;
const FEATURE_ID_BYTE_LEN: usize = 4;
const FEATURE_ID_PROPERTY: &str = "featureId";
const NULL_STRING_SENTINEL: &str = "\0";
const EXT_MESHOPT_COMPRESSION: &str = "EXT_meshopt_compression";
const KHR_DRACO_MESH_COMPRESSION: &str = "KHR_draco_mesh_compression";
const KHR_MESH_QUANTIZATION: &str = "KHR_mesh_quantization";
const U16_NORMALIZATION_SCALE: f32 = u16::MAX as f32;
const I8_NORMALIZATION_SCALE: f32 = i8::MAX as f32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlbEncodingOptions {
    pub compression: Compression,
    pub quantization: bool,
}

impl Default for GlbEncodingOptions {
    fn default() -> Self {
        Self {
            compression: Compression::Meshopt,
            quantization: true,
        }
    }
}

#[derive(Clone, Copy)]
enum MeshoptMode {
    Attributes,
    Triangles,
}

impl MeshoptMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Attributes => "ATTRIBUTES",
            Self::Triangles => "TRIANGLES",
        }
    }
}

struct GeometryBufferView {
    view_index: usize,
    count: usize,
    byte_stride: usize,
    mode: MeshoptMode,
}

struct GeometryAttributeStream {
    semantic: &'static str,
    attribute_type: GeometryAttributeType,
    components: u8,
    data_type: DataType,
    normalized: bool,
    packed_bytes: Vec<u8>,
}

struct EncodedAttribute {
    buffer_view_bytes: Vec<u8>,
    packed_bytes: Vec<u8>,
    byte_stride: usize,
    component_type: u32,
    data_type: DataType,
    normalized: bool,
}

struct EncodedPosition {
    attribute: EncodedAttribute,
    accessor_min: [f32; 3],
    accessor_max: [f32; 3],
    node_transform: [f64; 16],
}

struct DracoPayload {
    bytes: Vec<u8>,
    attribute_ids: Map<String, Value>,
    vertex_count: usize,
    index_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContentFeature {
    pub id: String,
    pub mesh: TriangleMesh,
    pub base_color: [f32; 4],
    pub double_sided: bool,
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
#[tracing::instrument(
    level = "debug",
    skip(features, node_transform),
    fields(feature_count = features.len())
)]
pub fn encode_feature_content_tile_glb(
    features: &[ContentFeature],
    node_transform: [f64; 16],
) -> Result<Vec<u8>, GlbError> {
    encode_feature_content_tile_glb_with_options(
        features,
        node_transform,
        GlbEncodingOptions::default(),
    )
}

/// Encode feature-aware content with an explicitly selected geometry backend.
pub fn encode_feature_content_tile_glb_with_options(
    features: &[ContentFeature],
    node_transform: [f64; 16],
    options: GlbEncodingOptions,
) -> Result<Vec<u8>, GlbError> {
    if features.is_empty() {
        return Err(GlbError::EmptyMesh);
    }
    validate_node_transform(node_transform)?;
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
    let mut double_sided = false;

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
        double_sided |= feature.double_sided;

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
    let mut geometry_views = vec![GeometryBufferView {
        view_index: index_view,
        count: tile_mesh.indices.len(),
        byte_stride: INDEX_BYTE_LEN,
        mode: MeshoptMode::Triangles,
    }];

    let encoded_position =
        encode_positions(&tile_mesh.vertices, node_transform, options.quantization)?;
    let position_view = append_buffer_view(
        &mut binary,
        &mut buffer_views,
        &encoded_position.attribute.buffer_view_bytes,
        BYTE_ALIGNMENT,
        Some(ARRAY_BUFFER_TARGET),
        Some(encoded_position.attribute.byte_stride),
    );
    geometry_views.push(GeometryBufferView {
        view_index: position_view,
        count: tile_mesh.vertices.len(),
        byte_stride: encoded_position.attribute.byte_stride,
        mode: MeshoptMode::Attributes,
    });

    let encoded_normal = encode_normals(&tile_mesh.vertices, options.quantization);
    let normal_view = append_buffer_view(
        &mut binary,
        &mut buffer_views,
        &encoded_normal.buffer_view_bytes,
        BYTE_ALIGNMENT,
        Some(ARRAY_BUFFER_TARGET),
        Some(encoded_normal.byte_stride),
    );
    geometry_views.push(GeometryBufferView {
        view_index: normal_view,
        count: tile_mesh.vertices.len(),
        byte_stride: encoded_normal.byte_stride,
        mode: MeshoptMode::Attributes,
    });

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
    geometry_views.push(GeometryBufferView {
        view_index: color_view,
        count: vertex_colors.len(),
        byte_stride: COLOR_BYTE_LEN,
        mode: MeshoptMode::Attributes,
    });

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
    geometry_views.push(GeometryBufferView {
        view_index: feature_id_view,
        count: vertex_feature_ids.len(),
        byte_stride: FEATURE_ID_BYTE_LEN,
        mode: MeshoptMode::Attributes,
    });
    let geometry_attributes = vec![
        GeometryAttributeStream {
            semantic: "POSITION",
            attribute_type: GeometryAttributeType::Position,
            components: 3,
            data_type: encoded_position.attribute.data_type,
            normalized: encoded_position.attribute.normalized,
            packed_bytes: encoded_position.attribute.packed_bytes,
        },
        GeometryAttributeStream {
            semantic: "NORMAL",
            attribute_type: GeometryAttributeType::Normal,
            components: 3,
            data_type: encoded_normal.data_type,
            normalized: encoded_normal.normalized,
            packed_bytes: encoded_normal.packed_bytes,
        },
        GeometryAttributeStream {
            semantic: "COLOR_0",
            attribute_type: GeometryAttributeType::Color,
            components: 4,
            data_type: DataType::Float32,
            normalized: false,
            packed_bytes: color_bytes,
        },
        GeometryAttributeStream {
            semantic: "_FEATURE_ID_0",
            attribute_type: GeometryAttributeType::Generic,
            components: 1,
            data_type: DataType::Float32,
            normalized: false,
            packed_bytes: feature_id_bytes,
        },
    ];

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

    let binary_byte_length = align_len(binary.len(), BYTE_ALIGNMENT);
    let last_feature_id = (features.len() - 1) as f32;
    let alpha_mode = if uses_blending { "BLEND" } else { "OPAQUE" };
    let document = json!({
        "asset": {
            "version": "2.0",
            "generator": "lucy"
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
        "nodes": [{ "mesh": 0, "matrix": encoded_position.node_transform }],
        "materials": [
            {
                "name": "Lucy feature colors",
                "pbrMetallicRoughness": {
                    "baseColorFactor": [1.0, 1.0, 1.0, 1.0],
                    "metallicFactor": 0.0,
                    "roughnessFactor": 1.0
                },
                "alphaMode": alpha_mode,
                "doubleSided": double_sided
            }
        ],
        "meshes": [
            {
                "primitives": [
                    {
                        "attributes": {
                            "POSITION": 1,
                            "NORMAL": 2,
                            "COLOR_0": 3,
                            "_FEATURE_ID_0": 4
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
            attribute_accessor(
                position_view,
                encoded_position.attribute.component_type,
                encoded_position.attribute.normalized,
                tile_mesh.vertices.len(),
                "VEC3",
                Some(encoded_position.accessor_min),
                Some(encoded_position.accessor_max)
            ),
            attribute_accessor(
                normal_view,
                encoded_normal.component_type,
                encoded_normal.normalized,
                tile_mesh.vertices.len(),
                "VEC3",
                None,
                None
            ),
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

    let mut document = document;
    if options.quantization {
        add_required_extension(&mut document, KHR_MESH_QUANTIZATION)?;
    }
    let glb = encode_glb_document_with_compression(
        document,
        binary,
        &geometry_views,
        &geometry_attributes,
        &tile_mesh.indices,
        tile_mesh.vertices.len(),
        options.compression,
    )?;
    tracing::debug!(
        vertex_count = tile_mesh.vertices.len(),
        triangle_count = tile_mesh.indices.len() / 3,
        glb_bytes = glb.len(),
        "feature content GLB encoded"
    );
    Ok(glb)
}

fn encode_positions(
    vertices: &[MeshVertex],
    node_transform: [f64; 16],
    quantization: bool,
) -> Result<EncodedPosition, GlbError> {
    let bounds = position_bounds(vertices);
    if !quantization {
        let mut bytes = Vec::with_capacity(vertices.len() * POSITION_BYTE_LEN);
        for vertex in vertices {
            for component in gltf_position(vertex.position) {
                bytes.extend_from_slice(&component.to_le_bytes());
            }
        }
        return Ok(EncodedPosition {
            attribute: EncodedAttribute {
                buffer_view_bytes: bytes.clone(),
                packed_bytes: bytes,
                byte_stride: POSITION_BYTE_LEN,
                component_type: FLOAT_COMPONENT_TYPE,
                data_type: DataType::Float32,
                normalized: false,
            },
            accessor_min: bounds.min,
            accessor_max: bounds.max,
            node_transform,
        });
    }

    let spans = [
        f64::from(bounds.max[0] - bounds.min[0]),
        f64::from(bounds.max[1] - bounds.min[1]),
        f64::from(bounds.max[2] - bounds.min[2]),
    ];
    let largest_extent = spans.into_iter().fold(0.0_f64, f64::max);
    let dequantization_scale = if largest_extent > 0.0 {
        largest_extent
    } else {
        1.0
    };
    let origin = bounds.min.map(f64::from);
    let dequantization = [
        dequantization_scale,
        0.0,
        0.0,
        0.0,
        0.0,
        dequantization_scale,
        0.0,
        0.0,
        0.0,
        0.0,
        dequantization_scale,
        0.0,
        origin[0],
        origin[1],
        origin[2],
        1.0,
    ];
    let composed_transform = multiply_column_major_matrices(node_transform, dequantization);
    validate_node_transform(composed_transform)?;

    let mut buffer_view_bytes = Vec::with_capacity(vertices.len() * QUANTIZED_POSITION_BYTE_STRIDE);
    let mut packed_bytes = Vec::with_capacity(vertices.len() * POSITION_COMPONENTS * 2);
    let mut quantized_min = [u16::MAX; 3];
    let mut quantized_max = [u16::MIN; 3];
    for vertex in vertices {
        let position = gltf_position(vertex.position);
        for component in 0..POSITION_COMPONENTS {
            let normalized =
                (f64::from(position[component]) - origin[component]) / dequantization_scale;
            let quantized = (normalized.clamp(0.0, 1.0) * f64::from(u16::MAX)).round() as u16;
            quantized_min[component] = quantized_min[component].min(quantized);
            quantized_max[component] = quantized_max[component].max(quantized);
            let bytes = quantized.to_le_bytes();
            buffer_view_bytes.extend_from_slice(&bytes);
            packed_bytes.extend_from_slice(&bytes);
        }
        buffer_view_bytes.extend_from_slice(&[0, 0]);
    }

    Ok(EncodedPosition {
        attribute: EncodedAttribute {
            buffer_view_bytes,
            packed_bytes,
            byte_stride: QUANTIZED_POSITION_BYTE_STRIDE,
            component_type: UNSIGNED_SHORT_COMPONENT_TYPE,
            data_type: DataType::Uint16,
            normalized: true,
        },
        accessor_min: quantized_min.map(|value| f32::from(value) / U16_NORMALIZATION_SCALE),
        accessor_max: quantized_max.map(|value| f32::from(value) / U16_NORMALIZATION_SCALE),
        node_transform: composed_transform,
    })
}

fn encode_normals(vertices: &[MeshVertex], quantization: bool) -> EncodedAttribute {
    if !quantization {
        let mut bytes = Vec::with_capacity(vertices.len() * NORMAL_BYTE_LEN);
        for vertex in vertices {
            for component in gltf_direction(vertex.normal) {
                bytes.extend_from_slice(&component.to_le_bytes());
            }
        }
        return EncodedAttribute {
            buffer_view_bytes: bytes.clone(),
            packed_bytes: bytes,
            byte_stride: NORMAL_BYTE_LEN,
            component_type: FLOAT_COMPONENT_TYPE,
            data_type: DataType::Float32,
            normalized: false,
        };
    }

    let mut buffer_view_bytes = Vec::with_capacity(vertices.len() * QUANTIZED_NORMAL_BYTE_STRIDE);
    let mut packed_bytes = Vec::with_capacity(vertices.len() * POSITION_COMPONENTS);
    for vertex in vertices {
        for component in gltf_direction(vertex.normal) {
            let quantized = (component.clamp(-1.0, 1.0) * I8_NORMALIZATION_SCALE).round() as i8;
            buffer_view_bytes.push(quantized as u8);
            packed_bytes.push(quantized as u8);
        }
        buffer_view_bytes.push(0);
    }

    EncodedAttribute {
        buffer_view_bytes,
        packed_bytes,
        byte_stride: QUANTIZED_NORMAL_BYTE_STRIDE,
        component_type: BYTE_COMPONENT_TYPE,
        data_type: DataType::Int8,
        normalized: true,
    }
}

fn attribute_accessor(
    buffer_view: usize,
    component_type: u32,
    normalized: bool,
    count: usize,
    accessor_type: &str,
    min: Option<[f32; 3]>,
    max: Option<[f32; 3]>,
) -> Value {
    let mut accessor = Map::new();
    accessor.insert("bufferView".to_string(), json!(buffer_view));
    accessor.insert("byteOffset".to_string(), json!(0));
    accessor.insert("componentType".to_string(), json!(component_type));
    if normalized {
        accessor.insert("normalized".to_string(), json!(true));
    }
    accessor.insert("count".to_string(), json!(count));
    accessor.insert("type".to_string(), json!(accessor_type));
    if let Some(min) = min {
        accessor.insert("min".to_string(), json!(min));
    }
    if let Some(max) = max {
        accessor.insert("max".to_string(), json!(max));
    }
    Value::Object(accessor)
}

fn multiply_column_major_matrices(left: [f64; 16], right: [f64; 16]) -> [f64; 16] {
    let mut result = [0.0; 16];
    for column in 0..4 {
        for row in 0..4 {
            result[column * 4 + row] = (0..4)
                .map(|component| left[component * 4 + row] * right[column * 4 + component])
                .sum();
        }
    }
    result
}

fn encode_validated_mesh_glb(mesh: &TriangleMesh) -> Result<Vec<u8>, GlbError> {
    validate_mesh(mesh)?;

    let index_byte_offset = 0_usize;
    let index_byte_length = mesh.indices.len() * INDEX_BYTE_LEN;
    let position_byte_offset = align_len(index_byte_length, BYTE_ALIGNMENT);
    let position_byte_length = mesh.vertices.len() * POSITION_BYTE_LEN;
    let normal_byte_offset = align_len(position_byte_offset + position_byte_length, BYTE_ALIGNMENT);
    let normal_byte_length = mesh.vertices.len() * NORMAL_BYTE_LEN;
    let binary_byte_length = normal_byte_offset + normal_byte_length;

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
    binary.extend(std::iter::repeat_n(0, normal_byte_offset - binary.len()));
    for vertex in &mesh.vertices {
        for component in gltf_direction(vertex.normal) {
            binary.extend_from_slice(&component.to_le_bytes());
        }
    }

    let bounds = position_bounds(&mesh.vertices);
    let document = json!({
        "asset": {
            "version": "2.0",
            "generator": "lucy"
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
                            "POSITION": 1,
                            "NORMAL": 2
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
            },
            {
                "buffer": 0,
                "byteOffset": normal_byte_offset,
                "byteLength": normal_byte_length,
                "byteStride": NORMAL_BYTE_LEN,
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
            },
            {
                "bufferView": 2,
                "byteOffset": 0,
                "componentType": FLOAT_COMPONENT_TYPE,
                "count": mesh.vertices.len(),
                "type": "VEC3"
            }
        ]
    });

    encode_glb_document(document, binary)
}

fn encode_glb_document_with_compression(
    document: Value,
    binary: Vec<u8>,
    geometry_views: &[GeometryBufferView],
    attributes: &[GeometryAttributeStream],
    indices: &[u32],
    vertex_count: usize,
    compression: Compression,
) -> Result<Vec<u8>, GlbError> {
    match compression {
        Compression::Meshopt => {
            encode_meshopt_glb(document, &binary, geometry_views, indices, vertex_count)
        }
        Compression::Draco => encode_draco_glb(
            document,
            &binary,
            geometry_views.len(),
            attributes,
            indices,
            vertex_count,
        ),
        Compression::None => encode_glb_document(document, binary),
    }
}

fn encode_meshopt_glb(
    mut document: Value,
    source_binary: &[u8],
    geometry_views: &[GeometryBufferView],
    indices: &[u32],
    vertex_count: usize,
) -> Result<Vec<u8>, GlbError> {
    let source_views = document["bufferViews"]
        .as_array()
        .cloned()
        .ok_or_else(|| GlbError::Encode("glTF bufferViews must be an array".to_string()))?;
    let mut compressed_by_view = BTreeMap::new();

    for geometry in geometry_views {
        let view = source_views
            .get(geometry.view_index)
            .ok_or_else(|| GlbError::Encode("geometry bufferView is missing".to_string()))?;
        let raw = buffer_view_slice(source_binary, view)?;
        let compressed = match geometry.mode {
            MeshoptMode::Triangles => {
                meshopt::encode_index_buffer(indices, vertex_count).map_err(|error| {
                    GlbError::Encode(format!("meshopt index compression failed: {error}"))
                })?
            }
            MeshoptMode::Attributes => {
                meshopt_encode_vertex_buffer(raw, geometry.count, geometry.byte_stride)?
            }
        };
        compressed_by_view.insert(geometry.view_index, compressed);
    }

    let mut output_binary = Vec::new();
    let mut output_views = source_views.clone();
    for (view_index, view) in output_views.iter_mut().enumerate() {
        let view_object = view.as_object_mut().ok_or_else(|| {
            GlbError::Encode(format!("bufferView {view_index} must be an object"))
        })?;
        if let Some(compressed) = compressed_by_view.get(&view_index) {
            let geometry = geometry_views
                .iter()
                .find(|geometry| geometry.view_index == view_index)
                .ok_or_else(|| {
                    GlbError::Encode("meshopt geometry descriptor is missing".to_string())
                })?;
            pad_bytes(&mut output_binary, 0, BYTE_ALIGNMENT);
            let byte_offset = output_binary.len();
            output_binary.extend_from_slice(compressed);

            view_object.insert("buffer".to_string(), json!(1));
            let extensions = view_object
                .entry("extensions".to_string())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or_else(|| {
                    GlbError::Encode(format!(
                        "bufferView {view_index} extensions must be an object"
                    ))
                })?;
            extensions.insert(
                EXT_MESHOPT_COMPRESSION.to_string(),
                json!({
                    "buffer": 0,
                    "byteOffset": byte_offset,
                    "byteLength": compressed.len(),
                    "byteStride": geometry.byte_stride,
                    "count": geometry.count,
                    "mode": geometry.mode.as_str(),
                    "filter": "NONE"
                }),
            );
        } else {
            let raw = buffer_view_slice(source_binary, &source_views[view_index])?;
            pad_bytes(&mut output_binary, 0, BYTE_ALIGNMENT);
            let byte_offset = output_binary.len();
            output_binary.extend_from_slice(raw);
            view_object.insert("buffer".to_string(), json!(0));
            view_object.insert("byteOffset".to_string(), json!(byte_offset));
        }
    }
    pad_bytes(&mut output_binary, 0, BYTE_ALIGNMENT);

    document["bufferViews"] = Value::Array(output_views);
    document["buffers"] = json!([
        {
            "byteLength": output_binary.len()
        },
        {
            "byteLength": source_binary.len(),
            "extensions": {
                EXT_MESHOPT_COMPRESSION: {
                    "fallback": true
                }
            }
        }
    ]);
    add_required_extension(&mut document, EXT_MESHOPT_COMPRESSION)?;
    encode_glb_document(document, output_binary)
}

fn meshopt_encode_vertex_buffer(
    bytes: &[u8],
    vertex_count: usize,
    byte_stride: usize,
) -> Result<Vec<u8>, GlbError> {
    let expected_len = vertex_count
        .checked_mul(byte_stride)
        .ok_or_else(|| GlbError::Encode("meshopt vertex byte length overflowed".to_string()))?;
    if bytes.len() != expected_len {
        return Err(GlbError::Encode(format!(
            "meshopt vertex stream has {} bytes, expected {expected_len}",
            bytes.len()
        )));
    }

    let bound = unsafe { meshopt::ffi::meshopt_encodeVertexBufferBound(vertex_count, byte_stride) };
    let mut compressed = vec![0_u8; bound];
    let encoded_len = unsafe {
        // EXT_meshopt_compression requires the v0 vertex bitstream.
        meshopt::ffi::meshopt_encodeVertexBufferLevel(
            compressed.as_mut_ptr(),
            compressed.len(),
            bytes.as_ptr().cast(),
            vertex_count,
            byte_stride,
            0,
            0,
        )
    };
    if encoded_len == 0 {
        return Err(GlbError::Encode(
            "meshopt vertex compression returned an empty payload".to_string(),
        ));
    }
    compressed.truncate(encoded_len);
    Ok(compressed)
}

fn encode_draco_glb(
    mut document: Value,
    source_binary: &[u8],
    geometry_view_count: usize,
    attributes: &[GeometryAttributeStream],
    indices: &[u32],
    vertex_count: usize,
) -> Result<Vec<u8>, GlbError> {
    let draco = encode_draco_mesh(attributes, indices, vertex_count)?;
    let source_views = document["bufferViews"]
        .as_array()
        .cloned()
        .ok_or_else(|| GlbError::Encode("glTF bufferViews must be an array".to_string()))?;
    if source_views.len() < geometry_view_count {
        return Err(GlbError::Encode(
            "geometry bufferView count exceeds document bufferViews".to_string(),
        ));
    }

    let mut output_binary = draco.bytes;
    let draco_byte_length = output_binary.len();
    let mut output_views = vec![json!({
        "buffer": 0,
        "byteOffset": 0,
        "byteLength": draco_byte_length
    })];
    let mut metadata_view_map = BTreeMap::new();
    for (old_index, view) in source_views.iter().enumerate().skip(geometry_view_count) {
        let raw = buffer_view_slice(source_binary, view)?;
        pad_bytes(&mut output_binary, 0, BYTE_ALIGNMENT);
        let byte_offset = output_binary.len();
        output_binary.extend_from_slice(raw);
        let new_index = output_views.len();
        metadata_view_map.insert(old_index, new_index);

        let mut output_view = view.clone();
        let object = output_view
            .as_object_mut()
            .ok_or_else(|| GlbError::Encode(format!("bufferView {old_index} must be an object")))?;
        object.insert("buffer".to_string(), json!(0));
        object.insert("byteOffset".to_string(), json!(byte_offset));
        output_views.push(output_view);
    }
    pad_bytes(&mut output_binary, 0, BYTE_ALIGNMENT);

    remap_structural_metadata_views(&mut document, &metadata_view_map)?;
    let accessors = document["accessors"]
        .as_array_mut()
        .ok_or_else(|| GlbError::Encode("glTF accessors must be an array".to_string()))?;
    for (accessor_index, accessor) in accessors.iter_mut().enumerate() {
        let object = accessor.as_object_mut().ok_or_else(|| {
            GlbError::Encode(format!("accessor {accessor_index} must be an object"))
        })?;
        object.remove("bufferView");
        object.remove("byteOffset");
        object.insert(
            "count".to_string(),
            json!(if accessor_index == 0 {
                draco.index_count
            } else {
                draco.vertex_count
            }),
        );
    }

    let primitive = document["meshes"][0]["primitives"][0]
        .as_object_mut()
        .ok_or_else(|| GlbError::Encode("glTF primitive must be an object".to_string()))?;
    let extensions = primitive
        .entry("extensions".to_string())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| GlbError::Encode("primitive extensions must be an object".to_string()))?;
    extensions.insert(
        KHR_DRACO_MESH_COMPRESSION.to_string(),
        json!({
            "bufferView": 0,
            "attributes": draco.attribute_ids
        }),
    );

    document["bufferViews"] = Value::Array(output_views);
    document["buffers"] = json!([{ "byteLength": output_binary.len() }]);
    add_required_extension(&mut document, KHR_DRACO_MESH_COMPRESSION)?;
    encode_glb_document(document, output_binary)
}

fn encode_draco_mesh(
    attributes: &[GeometryAttributeStream],
    indices: &[u32],
    vertex_count: usize,
) -> Result<DracoPayload, GlbError> {
    let mut mesh = DracoMesh::new();
    mesh.set_num_points(vertex_count);
    mesh.set_num_faces(indices.len() / 3);
    mesh.set_faces_from_flat_indices(indices);

    let mut semantic_by_attribute = BTreeMap::new();
    for stream in attributes {
        let expected_len = vertex_count
            .checked_mul(stream.components as usize)
            .and_then(|length| length.checked_mul(stream.data_type.byte_length()))
            .ok_or_else(|| GlbError::Encode("Draco attribute length overflowed".to_string()))?;
        if stream.packed_bytes.len() != expected_len {
            return Err(GlbError::Encode(format!(
                "Draco {} stream has {} bytes, expected {expected_len}",
                stream.semantic,
                stream.packed_bytes.len()
            )));
        }
        let mut attribute = PointAttribute::new();
        attribute.init(
            stream.attribute_type,
            stream.components,
            stream.data_type,
            stream.normalized,
            vertex_count,
        );
        attribute.buffer_mut().update(&stream.packed_bytes, None);
        let attribute_id = mesh.add_attribute(attribute);
        semantic_by_attribute.insert(attribute_id, stream.semantic);
    }

    let mut options = EncoderOptions::new();
    options.set_global_int("encoding_speed", 5);
    options.set_global_int("decoding_speed", 5);
    let mut encoder = MeshEncoder::new();
    encoder.set_mesh(mesh);
    let mut buffer = EncoderBuffer::new();
    encoder
        .encode(&options, &mut buffer)
        .map_err(|error| GlbError::Encode(format!("Draco compression failed: {error}")))?;
    let info = encoder.encoded_mesh_info().ok_or_else(|| {
        GlbError::Encode("Draco encoder did not report encoded mesh information".to_string())
    })?;

    let mut attribute_ids = Map::new();
    for attribute in &info.attributes {
        let semantic = semantic_by_attribute
            .get(&attribute.source_attribute_id)
            .ok_or_else(|| {
                GlbError::Encode(format!(
                    "Draco attribute {} has no glTF semantic",
                    attribute.source_attribute_id
                ))
            })?;
        attribute_ids.insert((*semantic).to_string(), json!(attribute.unique_id));
    }
    if attribute_ids.len() != attributes.len() {
        return Err(GlbError::Encode(
            "Draco encoder did not preserve every glTF attribute".to_string(),
        ));
    }

    Ok(DracoPayload {
        bytes: buffer.data().to_vec(),
        attribute_ids,
        vertex_count: info.num_encoded_points,
        index_count: info.num_encoded_faces * 3,
    })
}

fn remap_structural_metadata_views(
    document: &mut Value,
    view_map: &BTreeMap<usize, usize>,
) -> Result<(), GlbError> {
    let Some(tables) =
        document["extensions"]["EXT_structural_metadata"]["propertyTables"].as_array_mut()
    else {
        return Ok(());
    };
    for table in tables {
        let properties = table["properties"].as_object_mut().ok_or_else(|| {
            GlbError::Encode("structural metadata properties must be an object".to_string())
        })?;
        for property in properties.values_mut() {
            let object = property.as_object_mut().ok_or_else(|| {
                GlbError::Encode("structural metadata property must be an object".to_string())
            })?;
            for field in ["values", "stringOffsets", "arrayOffsets"] {
                let Some(old_index) = object.get(field).and_then(Value::as_u64) else {
                    continue;
                };
                let old_index = usize::try_from(old_index).map_err(|_| {
                    GlbError::Encode("metadata bufferView index exceeds usize".to_string())
                })?;
                let new_index = view_map.get(&old_index).ok_or_else(|| {
                    GlbError::Encode(format!(
                        "metadata {field} bufferView {old_index} was not preserved"
                    ))
                })?;
                object.insert(field.to_string(), json!(new_index));
            }
        }
    }
    Ok(())
}

fn add_required_extension(document: &mut Value, extension: &str) -> Result<(), GlbError> {
    for field in ["extensionsUsed", "extensionsRequired"] {
        let extensions = document
            .as_object_mut()
            .ok_or_else(|| GlbError::Encode("glTF document must be an object".to_string()))?
            .entry(field.to_string())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| GlbError::Encode(format!("glTF {field} must be an array")))?;
        if !extensions.iter().any(|value| value == extension) {
            extensions.push(json!(extension));
        }
    }
    Ok(())
}

fn buffer_view_slice<'a>(binary: &'a [u8], view: &Value) -> Result<&'a [u8], GlbError> {
    let byte_offset = view["byteOffset"].as_u64().unwrap_or(0);
    let byte_length = view["byteLength"]
        .as_u64()
        .ok_or_else(|| GlbError::Encode("bufferView byteLength is missing".to_string()))?;
    let start = usize::try_from(byte_offset)
        .map_err(|_| GlbError::Encode("bufferView byteOffset exceeds usize".to_string()))?;
    let length = usize::try_from(byte_length)
        .map_err(|_| GlbError::Encode("bufferView byteLength exceeds usize".to_string()))?;
    let end = start
        .checked_add(length)
        .ok_or_else(|| GlbError::Encode("bufferView range overflowed".to_string()))?;
    binary
        .get(start..end)
        .ok_or_else(|| GlbError::Encode("bufferView range exceeds binary buffer".to_string()))
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
    InvalidTransform(String),
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
            GlbError::InvalidTransform(message) => {
                write!(f, "invalid GLB node transform: {message}")
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

fn validate_node_transform(transform: [f64; 16]) -> Result<(), GlbError> {
    if !transform.iter().all(|component| component.is_finite()) {
        return Err(GlbError::InvalidTransform(
            "all matrix components must be finite".to_string(),
        ));
    }
    if transform[3].abs() > 1.0e-12
        || transform[7].abs() > 1.0e-12
        || transform[11].abs() > 1.0e-12
        || (transform[15] - 1.0).abs() > 1.0e-12
    {
        return Err(GlbError::InvalidTransform(
            "matrix must be affine with bottom row [0, 0, 0, 1]".to_string(),
        ));
    }
    Ok(())
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
        if !vertex.normal.iter().all(|component| component.is_finite()) {
            return Err(GlbError::InvalidMesh(format!(
                "vertex {index} normal contains a non-finite component"
            )));
        }
        let normal_length = vertex
            .normal
            .iter()
            .map(|component| component * component)
            .sum::<f32>()
            .sqrt();
        if (normal_length - 1.0).abs() > 1.0e-3 {
            return Err(GlbError::InvalidMesh(format!(
                "vertex {index} normal must be unit length, got {normal_length}"
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

fn gltf_direction(enu_direction: [f32; 3]) -> [f32; 3] {
    let [east, north, up] = enu_direction;
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
    use std::time::Instant;

    use draco_core::{DecoderBuffer, MeshDecoder, PointIndex};
    use serde_json::Value;

    use super::*;

    fn fixture_mesh() -> TriangleMesh {
        TriangleMesh {
            vertices: vec![
                MeshVertex {
                    position: [0.0, 0.0, 0.0],
                    normal: [0.0, 0.0, 1.0],
                },
                MeshVertex {
                    position: [1.0, 0.0, 0.0],
                    normal: [0.0, 0.0, 1.0],
                },
                MeshVertex {
                    position: [1.0, 1.0, 0.0],
                    normal: [0.0, 0.0, 1.0],
                },
                MeshVertex {
                    position: [0.0, 1.0, 0.0],
                    normal: [0.0, 0.0, 1.0],
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
            double_sided: false,
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
            double_sided: true,
            properties: BTreeMap::from([
                ("name".to_string(), Some("Beta".to_string())),
                ("building_type".to_string(), Some("office".to_string())),
            ]),
        };

        vec![first, second]
    }

    fn identity_transform() -> [f64; 16] {
        [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ]
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
        assert_eq!(parsed.bin.len(), 120);
        assert_eq!(parsed.document["asset"]["version"], "2.0");
        assert_eq!(parsed.document["scene"], 0);
        assert_eq!(parsed.document["buffers"][0]["byteLength"], 120);
        assert_eq!(parsed.document["bufferViews"][0]["byteLength"], 24);
        assert_eq!(parsed.document["bufferViews"][1]["byteOffset"], 24);
        assert_eq!(parsed.document["bufferViews"][2]["byteOffset"], 72);
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

        let positions = parsed.bin[24..72]
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("f32 chunk")))
            .collect::<Vec<_>>();
        assert_eq!(
            positions,
            vec![
                0.0, 0.0, -0.0, 1.0, 0.0, -0.0, 1.0, 0.0, -1.0, 0.0, 0.0, -1.0
            ]
        );

        let normals = parsed.bin[72..120]
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("f32 chunk")))
            .collect::<Vec<_>>();
        assert_eq!(
            normals,
            vec![
                0.0, 1.0, -0.0, 0.0, 1.0, -0.0, 0.0, 1.0, -0.0, 0.0, 1.0, -0.0
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
        let mut node_transform = identity_transform();
        node_transform[12..15].copy_from_slice(&[125.25, -32.5, 7.75]);
        let glb = encode_feature_content_tile_glb_with_options(
            &features,
            node_transform,
            GlbEncodingOptions {
                compression: Compression::None,
                quantization: false,
            },
        )
        .expect("feature GLB should encode");
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
        assert_eq!(document["materials"][0]["doubleSided"], true);
        assert_eq!(document["nodes"][0]["matrix"], json!(node_transform));

        let primitive = &document["meshes"][0]["primitives"][0];
        assert_eq!(primitive["attributes"]["POSITION"], 1);
        assert_eq!(primitive["attributes"]["NORMAL"], 2);
        assert_eq!(primitive["attributes"]["COLOR_0"], 3);
        assert_eq!(primitive["attributes"]["_FEATURE_ID_0"], 4);
        assert_eq!(
            primitive["extensions"]["EXT_mesh_features"]["featureIds"][0],
            json!({
                "featureCount": 2,
                "attribute": 0,
                "propertyTable": 0,
                "label": "feature"
            })
        );

        let colors = read_f32_accessor(&parsed, 3, 4);
        assert_eq!(&colors[0..4], &[0.72, 0.70, 0.65, 1.0]);
        assert_eq!(&colors[12..16], &[0.72, 0.70, 0.65, 1.0]);
        assert_eq!(&colors[16..20], &[0.25, 0.5, 0.75, 0.5]);
        assert_eq!(&colors[28..32], &[0.25, 0.5, 0.75, 0.5]);

        let feature_ids = read_f32_accessor(&parsed, 4, 1);
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
    fn feature_content_meshopt_compression_is_lossless_when_quantization_is_disabled() {
        let features = fixture_content_features();
        let compressed = encode_feature_content_tile_glb_with_options(
            &features,
            identity_transform(),
            GlbEncodingOptions {
                compression: Compression::Meshopt,
                quantization: false,
            },
        )
        .expect("meshopt GLB");
        let uncompressed = encode_feature_content_tile_glb_with_options(
            &features,
            identity_transform(),
            GlbEncodingOptions {
                compression: Compression::None,
                quantization: false,
            },
        )
        .expect("uncompressed GLB");
        let compressed = parse_glb(&compressed);
        let uncompressed = parse_glb(&uncompressed);

        assert_eq!(
            compressed.document["extensionsRequired"],
            json!([EXT_MESHOPT_COMPRESSION])
        );
        assert_eq!(compressed.document["buffers"].as_array().unwrap().len(), 2);
        assert_eq!(
            compressed.document["buffers"][1]["extensions"][EXT_MESHOPT_COMPRESSION]["fallback"],
            true
        );

        for view_index in 0..5 {
            let view = &compressed.document["bufferViews"][view_index];
            let extension = &view["extensions"][EXT_MESHOPT_COMPRESSION];
            assert_eq!(view["buffer"], 1);
            assert_eq!(extension["buffer"], 0);
            assert_eq!(extension["filter"], "NONE");
            assert_eq!(
                extension["mode"],
                if view_index == 0 {
                    "TRIANGLES"
                } else {
                    "ATTRIBUTES"
                }
            );

            let decoded = decode_meshopt_view(&compressed, view_index);
            let expected = buffer_view_bytes(&uncompressed, view_index);
            if view_index == 0 {
                assert_eq!(
                    canonical_triangles_from_index_bytes(&decoded),
                    canonical_triangles_from_index_bytes(expected)
                );
            } else {
                assert_eq!(decoded, expected);
            }
        }

        assert_eq!(
            read_string_property(&compressed, FEATURE_ID_PROPERTY),
            vec!["101", "202"]
        );
        assert_eq!(
            read_string_property(&compressed, "building_type"),
            vec![NULL_STRING_SENTINEL, "office"]
        );
    }

    #[test]
    fn feature_content_draco_round_trips_topology_attributes_and_metadata() {
        let features = fixture_content_features();
        let glb = encode_feature_content_tile_glb_with_options(
            &features,
            identity_transform(),
            GlbEncodingOptions {
                compression: Compression::Draco,
                quantization: false,
            },
        )
        .expect("Draco GLB");
        let parsed = parse_glb(&glb);
        let document = &parsed.document;

        assert_eq!(
            document["extensionsRequired"],
            json!([KHR_DRACO_MESH_COMPRESSION])
        );
        assert_eq!(document["bufferViews"].as_array().unwrap().len(), 7);
        assert_eq!(document["accessors"][0].get("bufferView"), None);
        assert_eq!(document["accessors"][4].get("bufferView"), None);
        let draco_extension =
            &document["meshes"][0]["primitives"][0]["extensions"][KHR_DRACO_MESH_COMPRESSION];
        assert_eq!(draco_extension["bufferView"], 0);

        let draco_bytes = buffer_view_bytes(&parsed, 0);
        let mut decoded = DracoMesh::new();
        MeshDecoder::new()
            .decode(&mut DecoderBuffer::new(draco_bytes), &mut decoded)
            .expect("Draco payload should decode");
        assert_eq!(decoded.num_faces(), 4);
        assert_eq!(decoded.num_points(), 8);

        let position = decoded_attribute(&decoded, draco_extension, "POSITION");
        let normal = decoded_attribute(&decoded, draco_extension, "NORMAL");
        let color = decoded_attribute(&decoded, draco_extension, "COLOR_0");
        let feature_id = decoded_attribute(&decoded, draco_extension, "_FEATURE_ID_0");
        assert_eq!(position.data_type(), DataType::Float32);
        assert_eq!(normal.data_type(), DataType::Float32);
        assert_eq!(color.data_type(), DataType::Float32);
        assert_eq!(feature_id.data_type(), DataType::Float32);

        let mut decoded_feature_ids = (0..decoded.num_points())
            .map(|point| read_draco_f32(feature_id, PointIndex(point as u32), 1)[0])
            .collect::<Vec<_>>();
        decoded_feature_ids.sort_by(f32::total_cmp);
        assert_eq!(
            decoded_feature_ids,
            vec![0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0]
        );
        assert_eq!(
            canonical_decoded_draco_triangles(&decoded, position, normal, color, feature_id),
            canonical_fixture_triangles(&features)
        );
        assert_eq!(
            read_string_property(&parsed, FEATURE_ID_PROPERTY),
            vec!["101", "202"]
        );
        assert_eq!(read_string_property(&parsed, "name"), vec!["Alpha", "Beta"]);
    }

    #[test]
    fn compression_quantization_matrix_preserves_geometry_and_feature_contracts() {
        let mut features = fixture_content_features();
        features[0].mesh.vertices[1].position[0] += 0.123_45;
        let normal = [
            0.31_f32,
            0.71_f32,
            (1.0_f32 - 0.31_f32.powi(2) - 0.71_f32.powi(2)).sqrt(),
        ];
        for feature in &mut features {
            for vertex in &mut feature.mesh.vertices {
                vertex.normal = normal;
            }
        }

        let expected_positions = sorted_fixture_positions(&features);
        let expected_normal = normalize_vector(gltf_direction(normal).map(f64::from));
        let bounds = combined_position_bounds(&features);
        let largest_extent = (0..3)
            .map(|component| f64::from(bounds.max[component] - bounds.min[component]))
            .fold(0.0_f64, f64::max);
        let quantized_position_error =
            f64::sqrt(3.0) * largest_extent / (2.0 * f64::from(u16::MAX)) + 1.0e-6;

        for compression in [Compression::None, Compression::Meshopt, Compression::Draco] {
            for quantization in [false, true] {
                let glb = encode_feature_content_tile_glb_with_options(
                    &features,
                    identity_transform(),
                    GlbEncodingOptions {
                        compression,
                        quantization,
                    },
                )
                .expect("compression/quantization combination should encode");
                let parsed = parse_glb(&glb);
                let required = parsed.document["extensionsRequired"].as_array();
                let compression_extension = match compression {
                    Compression::Meshopt => Some(EXT_MESHOPT_COMPRESSION),
                    Compression::Draco => Some(KHR_DRACO_MESH_COMPRESSION),
                    Compression::None => None,
                };
                assert_eq!(
                    required.is_some_and(|required| compression_extension
                        .is_some_and(|extension| required.iter().any(|value| value == extension))),
                    compression_extension.is_some()
                );
                assert_eq!(
                    required.is_some_and(|required| required
                        .iter()
                        .any(|value| value == KHR_MESH_QUANTIZATION)),
                    quantization
                );
                assert_eq!(
                    parsed.document["accessors"][1]["componentType"],
                    if quantization {
                        UNSIGNED_SHORT_COMPONENT_TYPE
                    } else {
                        FLOAT_COMPONENT_TYPE
                    }
                );
                assert_eq!(
                    parsed.document["accessors"][2]["componentType"],
                    if quantization {
                        BYTE_COMPONENT_TYPE
                    } else {
                        FLOAT_COMPONENT_TYPE
                    }
                );
                assert_eq!(
                    parsed.document["accessors"][1]["normalized"]
                        .as_bool()
                        .unwrap_or(false),
                    quantization
                );
                assert_eq!(
                    parsed.document["accessors"][2]["normalized"]
                        .as_bool()
                        .unwrap_or(false),
                    quantization
                );

                let decoded = decoded_feature_geometry(&parsed, compression);
                assert_eq!(decoded.triangle_count, 4);
                assert_eq!(
                    decoded.feature_ids,
                    vec![0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0]
                );
                assert_eq!(decoded.positions.len(), expected_positions.len());
                let allowed_error = if quantization {
                    quantized_position_error
                } else {
                    1.0e-6
                };
                for (actual, expected) in decoded.positions.iter().zip(&expected_positions) {
                    let error = actual
                        .iter()
                        .zip(expected)
                        .map(|(actual, expected)| (actual - expected).powi(2))
                        .sum::<f64>()
                        .sqrt();
                    assert!(
                        error <= allowed_error,
                        "{compression:?}/{quantization}: position error {error} exceeds {allowed_error}"
                    );
                }
                for actual in decoded.normals {
                    let dot = actual
                        .iter()
                        .zip(expected_normal)
                        .map(|(actual, expected)| actual * expected)
                        .sum::<f64>()
                        .clamp(-1.0, 1.0);
                    let angular_error_deg = dot.acos().to_degrees();
                    assert!(
                        angular_error_deg <= 1.0,
                        "{compression:?}/{quantization}: normal error is {angular_error_deg} degrees"
                    );
                }
                assert_eq!(
                    read_string_property(&parsed, FEATURE_ID_PROPERTY),
                    vec!["101", "202"]
                );
                assert_eq!(
                    read_string_property(&parsed, "building_type"),
                    vec![NULL_STRING_SENTINEL, "office"]
                );
            }
        }
    }

    #[test]
    fn feature_content_default_encoding_matches_explicit_meshopt_quantization() {
        let features = fixture_content_features();
        let default =
            encode_feature_content_tile_glb(&features, identity_transform()).expect("default GLB");
        let explicit = encode_feature_content_tile_glb_with_options(
            &features,
            identity_transform(),
            GlbEncodingOptions {
                compression: Compression::Meshopt,
                quantization: true,
            },
        )
        .expect("explicit default GLB");
        assert_eq!(default, explicit);

        let parsed = parse_glb(&default);
        let required = parsed.document["extensionsRequired"]
            .as_array()
            .expect("required extensions");
        assert!(
            required
                .iter()
                .any(|value| value == EXT_MESHOPT_COMPRESSION)
        );
        assert!(required.iter().any(|value| value == KHR_MESH_QUANTIZATION));
    }

    #[test]
    fn representative_fixture_records_compression_size_and_encoding_time() {
        let mut features = Vec::new();
        for index in 0..256 {
            let mut mesh = fixture_mesh();
            let x = (index % 16) as f32 * 2.0;
            let y = (index / 16) as f32 * 2.0;
            for (vertex_index, vertex) in mesh.vertices.iter_mut().enumerate() {
                let seed = (index * 17 + vertex_index * 31) as f32;
                vertex.position[0] += x + (seed * 0.013_37).sin() * 0.2;
                vertex.position[1] += y + (seed * 0.021_11).cos() * 0.2;
                vertex.position[2] += (seed * 0.007_91).sin() * 0.1;
            }
            features.push(ContentFeature {
                id: format!("building-{index}"),
                mesh,
                base_color: [0.72, 0.70, 0.65, 1.0],
                double_sided: false,
                properties: BTreeMap::from([(
                    "building_type".to_string(),
                    Some("fixture".to_string()),
                )]),
            });
        }

        let mut results = Vec::new();
        for (compression, quantization) in [
            (Compression::None, false),
            (Compression::Meshopt, false),
            (Compression::Meshopt, true),
            (Compression::Draco, false),
            (Compression::Draco, true),
        ] {
            let started = Instant::now();
            let glb = encode_feature_content_tile_glb_with_options(
                &features,
                identity_transform(),
                GlbEncodingOptions {
                    compression,
                    quantization,
                },
            )
            .expect("representative fixture should encode");
            results.push((compression, quantization, glb.len(), started.elapsed()));
        }

        let baseline = results[0].2;
        assert!(
            results[1].2 < baseline,
            "meshopt should reduce fixture size"
        );
        assert!(results[3].2 < baseline, "Draco should reduce fixture size");
        eprintln!(
            "representative fixture: none={} bytes/{:?}, meshopt-float={} bytes/{:?}, meshopt-quantized={} bytes/{:?}, draco-float={} bytes/{:?}, draco-quantized={} bytes/{:?}",
            results[0].2,
            results[0].3,
            results[1].2,
            results[1].3,
            results[2].2,
            results[2].3,
            results[3].2,
            results[3].3,
            results[4].2,
            results[4].3
        );
        assert!(
            results[2].2 < results[1].2,
            "quantization should improve meshopt size"
        );
        assert!(
            results[4].2 < results[3].2,
            "quantization should improve Draco size"
        );
    }

    #[test]
    fn feature_content_rejects_invalid_colors_and_metadata_ids() {
        let mut features = fixture_content_features();
        features[0].base_color[0] = 1.5;
        let error = encode_feature_content_tile_glb(&features, identity_transform())
            .expect_err("color should fail");
        assert!(error.to_string().contains("base color component 0"));

        let mut features = fixture_content_features();
        features[0]
            .properties
            .insert("bad-property".to_string(), Some("x".to_string()));
        let error = encode_feature_content_tile_glb(&features, identity_transform())
            .expect_err("property should fail");
        assert!(error.to_string().contains("must match"));
    }

    #[test]
    fn feature_content_rejects_non_affine_node_transform() {
        let mut transform = identity_transform();
        transform[3] = 0.5;
        let error = encode_feature_content_tile_glb(&fixture_content_features(), transform)
            .expect_err("perspective transform should fail");
        assert!(matches!(error, GlbError::InvalidTransform(_)));
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

    struct DecodedFeatureGeometry {
        positions: Vec<[f64; 3]>,
        normals: Vec<[f64; 3]>,
        feature_ids: Vec<f32>,
        triangle_count: usize,
    }

    fn decoded_feature_geometry(
        parsed: &ParsedGlb,
        compression: Compression,
    ) -> DecodedFeatureGeometry {
        let transform = parsed.document["nodes"][0]["matrix"]
            .as_array()
            .expect("node matrix")
            .iter()
            .map(|value| value.as_f64().expect("matrix component"))
            .collect::<Vec<_>>()
            .try_into()
            .expect("4x4 matrix");

        let mut decoded = match compression {
            Compression::Meshopt => {
                let positions = decode_meshopt_view(parsed, 1);
                let position_stride = parsed.document["bufferViews"][1]["extensions"]
                    [EXT_MESHOPT_COMPRESSION]["byteStride"]
                    .as_u64()
                    .expect("position stride") as usize;
                let positions = positions
                    .chunks_exact(position_stride)
                    .map(
                        |bytes| match parsed.document["accessors"][1]["componentType"].as_u64() {
                            Some(value) if value == u64::from(FLOAT_COMPONENT_TYPE) => [
                                f64::from(f32::from_le_bytes(bytes[0..4].try_into().unwrap())),
                                f64::from(f32::from_le_bytes(bytes[4..8].try_into().unwrap())),
                                f64::from(f32::from_le_bytes(bytes[8..12].try_into().unwrap())),
                            ],
                            Some(value) if value == u64::from(UNSIGNED_SHORT_COMPONENT_TYPE) => [
                                f64::from(u16::from_le_bytes(bytes[0..2].try_into().unwrap()))
                                    / f64::from(u16::MAX),
                                f64::from(u16::from_le_bytes(bytes[2..4].try_into().unwrap()))
                                    / f64::from(u16::MAX),
                                f64::from(u16::from_le_bytes(bytes[4..6].try_into().unwrap()))
                                    / f64::from(u16::MAX),
                            ],
                            other => panic!("unexpected meshopt position component type {other:?}"),
                        },
                    )
                    .map(|position| transform_point(transform, position))
                    .collect();

                let normals = decode_meshopt_view(parsed, 2);
                let normal_stride = parsed.document["bufferViews"][2]["extensions"]
                    [EXT_MESHOPT_COMPRESSION]["byteStride"]
                    .as_u64()
                    .expect("normal stride") as usize;
                let normals = normals
                    .chunks_exact(normal_stride)
                    .map(
                        |bytes| match parsed.document["accessors"][2]["componentType"].as_u64() {
                            Some(value) if value == u64::from(FLOAT_COMPONENT_TYPE) => [
                                f64::from(f32::from_le_bytes(bytes[0..4].try_into().unwrap())),
                                f64::from(f32::from_le_bytes(bytes[4..8].try_into().unwrap())),
                                f64::from(f32::from_le_bytes(bytes[8..12].try_into().unwrap())),
                            ],
                            Some(value) if value == u64::from(BYTE_COMPONENT_TYPE) => [
                                f64::from(bytes[0] as i8) / f64::from(i8::MAX),
                                f64::from(bytes[1] as i8) / f64::from(i8::MAX),
                                f64::from(bytes[2] as i8) / f64::from(i8::MAX),
                            ],
                            other => panic!("unexpected meshopt normal component type {other:?}"),
                        },
                    )
                    .map(normalize_vector)
                    .collect();

                let feature_ids = decode_meshopt_view(parsed, 4)
                    .chunks_exact(FEATURE_ID_BYTE_LEN)
                    .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()))
                    .collect();
                let index_count = parsed.document["accessors"][0]["count"]
                    .as_u64()
                    .expect("index count") as usize;
                DecodedFeatureGeometry {
                    positions,
                    normals,
                    feature_ids,
                    triangle_count: index_count / 3,
                }
            }
            Compression::Draco => {
                let extension = &parsed.document["meshes"][0]["primitives"][0]["extensions"]
                    [KHR_DRACO_MESH_COMPRESSION];
                let mut mesh = DracoMesh::new();
                MeshDecoder::new()
                    .decode(
                        &mut DecoderBuffer::new(buffer_view_bytes(parsed, 0)),
                        &mut mesh,
                    )
                    .expect("Draco payload should decode");
                let position = decoded_attribute(&mesh, extension, "POSITION");
                let normal = decoded_attribute(&mesh, extension, "NORMAL");
                let feature_id = decoded_attribute(&mesh, extension, "_FEATURE_ID_0");
                assert_eq!(
                    position.normalized(),
                    parsed.document["accessors"][1]["normalized"]
                        .as_bool()
                        .unwrap_or(false)
                );
                assert_eq!(
                    normal.normalized(),
                    parsed.document["accessors"][2]["normalized"]
                        .as_bool()
                        .unwrap_or(false)
                );

                let positions = (0..mesh.num_points())
                    .map(|point| {
                        let point = PointIndex(point as u32);
                        let value = match position.data_type() {
                            DataType::Float32 => {
                                <[f32; 3]>::try_from(read_draco_f32(position, point, 3))
                                    .expect("three position components")
                                    .map(f64::from)
                            }
                            DataType::Uint16 => read_draco_u16(position, point, 3)
                                .map(|component| f64::from(component) / f64::from(u16::MAX)),
                            other => panic!("unexpected Draco position type {other:?}"),
                        };
                        transform_point(transform, value)
                    })
                    .collect();
                let normals = (0..mesh.num_points())
                    .map(|point| {
                        let point = PointIndex(point as u32);
                        let value = match normal.data_type() {
                            DataType::Float32 => {
                                <[f32; 3]>::try_from(read_draco_f32(normal, point, 3))
                                    .expect("three normal components")
                                    .map(f64::from)
                            }
                            DataType::Int8 => read_draco_i8(normal, point, 3)
                                .map(|component| f64::from(component) / f64::from(i8::MAX)),
                            other => panic!("unexpected Draco normal type {other:?}"),
                        };
                        normalize_vector(value)
                    })
                    .collect();
                let feature_ids = (0..mesh.num_points())
                    .map(|point| read_draco_f32(feature_id, PointIndex(point as u32), 1)[0])
                    .collect();
                DecodedFeatureGeometry {
                    positions,
                    normals,
                    feature_ids,
                    triangle_count: mesh.num_faces(),
                }
            }
            Compression::None => {
                let position_stride = parsed.document["bufferViews"][1]["byteStride"]
                    .as_u64()
                    .expect("position stride") as usize;
                let positions = buffer_view_bytes(parsed, 1)
                    .chunks_exact(position_stride)
                    .map(
                        |bytes| match parsed.document["accessors"][1]["componentType"].as_u64() {
                            Some(value) if value == u64::from(FLOAT_COMPONENT_TYPE) => [
                                f64::from(f32::from_le_bytes(bytes[0..4].try_into().unwrap())),
                                f64::from(f32::from_le_bytes(bytes[4..8].try_into().unwrap())),
                                f64::from(f32::from_le_bytes(bytes[8..12].try_into().unwrap())),
                            ],
                            Some(value) if value == u64::from(UNSIGNED_SHORT_COMPONENT_TYPE) => [
                                f64::from(u16::from_le_bytes(bytes[0..2].try_into().unwrap()))
                                    / f64::from(u16::MAX),
                                f64::from(u16::from_le_bytes(bytes[2..4].try_into().unwrap()))
                                    / f64::from(u16::MAX),
                                f64::from(u16::from_le_bytes(bytes[4..6].try_into().unwrap()))
                                    / f64::from(u16::MAX),
                            ],
                            other => {
                                panic!("unexpected uncompressed position component type {other:?}")
                            }
                        },
                    )
                    .map(|position| transform_point(transform, position))
                    .collect();
                let normal_stride = parsed.document["bufferViews"][2]["byteStride"]
                    .as_u64()
                    .expect("normal stride") as usize;
                let normals = buffer_view_bytes(parsed, 2)
                    .chunks_exact(normal_stride)
                    .map(
                        |bytes| match parsed.document["accessors"][2]["componentType"].as_u64() {
                            Some(value) if value == u64::from(FLOAT_COMPONENT_TYPE) => [
                                f64::from(f32::from_le_bytes(bytes[0..4].try_into().unwrap())),
                                f64::from(f32::from_le_bytes(bytes[4..8].try_into().unwrap())),
                                f64::from(f32::from_le_bytes(bytes[8..12].try_into().unwrap())),
                            ],
                            Some(value) if value == u64::from(BYTE_COMPONENT_TYPE) => [
                                f64::from(bytes[0] as i8) / f64::from(i8::MAX),
                                f64::from(bytes[1] as i8) / f64::from(i8::MAX),
                                f64::from(bytes[2] as i8) / f64::from(i8::MAX),
                            ],
                            other => {
                                panic!("unexpected uncompressed normal component type {other:?}")
                            }
                        },
                    )
                    .map(normalize_vector)
                    .collect();
                let feature_ids = buffer_view_bytes(parsed, 4)
                    .chunks_exact(FEATURE_ID_BYTE_LEN)
                    .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()))
                    .collect();
                let index_count = parsed.document["accessors"][0]["count"]
                    .as_u64()
                    .expect("index count") as usize;
                DecodedFeatureGeometry {
                    positions,
                    normals,
                    feature_ids,
                    triangle_count: index_count / 3,
                }
            }
        };
        sort_positions(&mut decoded.positions);
        decoded.feature_ids.sort_by(f32::total_cmp);
        decoded
    }

    fn sorted_fixture_positions(features: &[ContentFeature]) -> Vec<[f64; 3]> {
        let mut positions = features
            .iter()
            .flat_map(|feature| {
                feature
                    .mesh
                    .vertices
                    .iter()
                    .map(|vertex| gltf_position(vertex.position).map(f64::from))
            })
            .collect::<Vec<_>>();
        sort_positions(&mut positions);
        positions
    }

    fn combined_position_bounds(features: &[ContentFeature]) -> PositionBounds {
        let vertices = features
            .iter()
            .flat_map(|feature| feature.mesh.vertices.iter().copied())
            .collect::<Vec<_>>();
        position_bounds(&vertices)
    }

    fn sort_positions(positions: &mut [[f64; 3]]) {
        positions.sort_by(|left, right| {
            left.iter()
                .zip(right)
                .find_map(|(left, right)| {
                    let order = left.total_cmp(right);
                    (order != std::cmp::Ordering::Equal).then_some(order)
                })
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    fn transform_point(matrix: [f64; 16], point: [f64; 3]) -> [f64; 3] {
        [
            matrix[0] * point[0] + matrix[4] * point[1] + matrix[8] * point[2] + matrix[12],
            matrix[1] * point[0] + matrix[5] * point[1] + matrix[9] * point[2] + matrix[13],
            matrix[2] * point[0] + matrix[6] * point[1] + matrix[10] * point[2] + matrix[14],
        ]
    }

    fn normalize_vector(vector: [f64; 3]) -> [f64; 3] {
        let length = vector
            .iter()
            .map(|component| component * component)
            .sum::<f64>()
            .sqrt();
        vector.map(|component| component / length)
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

    fn decode_meshopt_view(parsed: &ParsedGlb, view_index: usize) -> Vec<u8> {
        let view = &parsed.document["bufferViews"][view_index];
        let extension = &view["extensions"][EXT_MESHOPT_COMPRESSION];
        let start = extension["byteOffset"].as_u64().expect("meshopt offset") as usize;
        let length = extension["byteLength"].as_u64().expect("meshopt length") as usize;
        let count = extension["count"].as_u64().expect("meshopt count") as usize;
        let stride = extension["byteStride"].as_u64().expect("meshopt stride") as usize;
        let encoded = &parsed.bin[start..start + length];

        if extension["mode"] == "TRIANGLES" {
            meshopt::decode_index_buffer::<u32>(encoded, count)
                .expect("meshopt indices should decode")
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect()
        } else {
            let mut decoded = vec![0_u8; count * stride];
            let result = unsafe {
                meshopt::ffi::meshopt_decodeVertexBuffer(
                    decoded.as_mut_ptr().cast(),
                    count,
                    stride,
                    encoded.as_ptr(),
                    encoded.len(),
                )
            };
            assert_eq!(result, 0, "meshopt attributes should decode");
            decoded
        }
    }

    fn canonical_triangles_from_index_bytes(bytes: &[u8]) -> Vec<[u32; 3]> {
        let mut triangles = bytes
            .chunks_exact(12)
            .map(|triangle| {
                let mut indices = [
                    u32::from_le_bytes(triangle[0..4].try_into().unwrap()),
                    u32::from_le_bytes(triangle[4..8].try_into().unwrap()),
                    u32::from_le_bytes(triangle[8..12].try_into().unwrap()),
                ];
                indices.sort_unstable();
                indices
            })
            .collect::<Vec<_>>();
        triangles.sort_unstable();
        triangles
    }

    fn decoded_attribute<'a>(
        mesh: &'a DracoMesh,
        extension: &Value,
        semantic: &str,
    ) -> &'a PointAttribute {
        let unique_id = extension["attributes"][semantic]
            .as_u64()
            .expect("Draco attribute id") as u32;
        mesh.attribute_by_unique_id(unique_id)
            .expect("decoded Draco attribute")
    }

    fn read_draco_f32(
        attribute: &PointAttribute,
        point: PointIndex,
        components: usize,
    ) -> Vec<f32> {
        let mapped = attribute.mapped_index(point).0 as usize;
        let start = mapped * attribute.byte_stride() as usize;
        attribute.buffer().data()[start..start + components * 4]
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
            .collect()
    }

    fn read_draco_u16(
        attribute: &PointAttribute,
        point: PointIndex,
        components: usize,
    ) -> [u16; 3] {
        assert_eq!(components, 3);
        let mapped = attribute.mapped_index(point).0 as usize;
        let start = mapped * attribute.byte_stride() as usize;
        attribute.buffer().data()[start..start + components * 2]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes(chunk.try_into().unwrap()))
            .collect::<Vec<_>>()
            .try_into()
            .expect("three u16 components")
    }

    fn read_draco_i8(attribute: &PointAttribute, point: PointIndex, components: usize) -> [i8; 3] {
        assert_eq!(components, 3);
        let mapped = attribute.mapped_index(point).0 as usize;
        let start = mapped * attribute.byte_stride() as usize;
        attribute.buffer().data()[start..start + components]
            .iter()
            .map(|value| *value as i8)
            .collect::<Vec<_>>()
            .try_into()
            .expect("three i8 components")
    }

    fn vertex_key(position: &[f32], normal: &[f32], color: &[f32], feature_id: f32) -> Vec<u32> {
        position
            .iter()
            .chain(normal)
            .chain(color)
            .copied()
            .chain(std::iter::once(feature_id))
            .map(f32::to_bits)
            .collect()
    }

    fn canonical_decoded_draco_triangles(
        mesh: &DracoMesh,
        position: &PointAttribute,
        normal: &PointAttribute,
        color: &PointAttribute,
        feature_id: &PointAttribute,
    ) -> Vec<Vec<Vec<u32>>> {
        let mut triangles = (0..mesh.num_faces())
            .map(|face_index| {
                let mut triangle = mesh
                    .face(draco_core::FaceIndex(face_index as u32))
                    .into_iter()
                    .map(|point| {
                        vertex_key(
                            &read_draco_f32(position, point, 3),
                            &read_draco_f32(normal, point, 3),
                            &read_draco_f32(color, point, 4),
                            read_draco_f32(feature_id, point, 1)[0],
                        )
                    })
                    .collect::<Vec<_>>();
                triangle.sort();
                triangle
            })
            .collect::<Vec<_>>();
        triangles.sort();
        triangles
    }

    fn canonical_fixture_triangles(features: &[ContentFeature]) -> Vec<Vec<Vec<u32>>> {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        for (feature_index, feature) in features.iter().enumerate() {
            let base = vertices.len() as u32;
            vertices.extend(feature.mesh.vertices.iter().map(|vertex| {
                vertex_key(
                    &gltf_position(vertex.position),
                    &gltf_direction(vertex.normal),
                    &feature.base_color,
                    feature_index as f32,
                )
            }));
            indices.extend(feature.mesh.indices.iter().map(|index| index + base));
        }

        let mut triangles = indices
            .chunks_exact(3)
            .map(|indices| {
                let mut triangle = indices
                    .iter()
                    .map(|index| vertices[*index as usize].clone())
                    .collect::<Vec<_>>();
                triangle.sort();
                triangle
            })
            .collect::<Vec<_>>();
        triangles.sort();
        triangles
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
