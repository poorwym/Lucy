use std::fmt;

use serde_json::json;

use crate::mesh::{MeshVertex, TriangleMesh};

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
        for component in vertex.position {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlbError {
    EmptyMesh,
    InvalidMesh(String),
    Encode(String),
}

impl fmt::Display for GlbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GlbError::EmptyMesh => write!(f, "cannot encode an empty mesh as GLB"),
            GlbError::InvalidMesh(message) => write!(f, "invalid mesh for GLB encoding: {message}"),
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

    if mesh.indices.len() % 3 != 0 {
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
    let mut min = vertices[0].position;
    let mut max = vertices[0].position;

    for vertex in vertices.iter().skip(1) {
        for component in 0..POSITION_COMPONENTS {
            min[component] = min[component].min(vertex.position[component]);
            max[component] = max[component].max(vertex.position[component]);
        }
    }

    PositionBounds { min, max }
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
    use std::env;
    use std::path::Path;

    use serde_json::Value;
    use tokio_postgres::NoTls;

    use super::*;
    use crate::SourceCatalog;
    use crate::mesh::{MeshFrame, wkb_footprint_to_mesh};
    use crate::postgis::query_tile_geometry_wkb;
    use crate::tile::TileCoord;

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
            json!([0.0, 0.0, 0.0])
        );
        assert_eq!(
            parsed.document["accessors"][1]["max"],
            json!([1.0, 1.0, 0.0])
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
            vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 0.0]
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

    #[tokio::test]
    async fn postgis_fixture_geometry_encodes_to_glb_content_tile() {
        let Ok(database_url) = env::var("DATABASE_URL") else {
            return;
        };

        let (client, connection) = tokio_postgres::connect(&database_url, NoTls)
            .await
            .expect("connect to PostGIS fixture database");
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("PostGIS connection error: {error}");
            }
        });

        client
            .batch_execute(include_str!("../../../fixtures/postgis/poc_buildings.sql"))
            .await
            .expect("load fixture data");

        let config_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/poc-sources.yaml");
        let mut catalog = SourceCatalog::load(config_path).expect("fixture config should load");
        let source = catalog
            .sources
            .remove("poc_buildings")
            .expect("poc source should exist");
        let features = query_tile_geometry_wkb(&client, &source, TileCoord::root())
            .await
            .expect("root tile should query");
        let first_feature = features.first().expect("fixture has at least one feature");
        let mesh = wkb_footprint_to_mesh(
            &first_feature.geometry_wkb,
            MeshFrame::from_source_bounds(&source.bounds),
        )
        .expect("fixture WKB should build a mesh");

        let glb = encode_content_tile_glb(&[mesh]).expect("fixture GLB should encode");
        let parsed = parse_glb(&glb);

        assert_eq!(parsed.magic, GLB_MAGIC);
        assert_eq!(parsed.document["asset"]["version"], "2.0");
        assert!(!parsed.bin.is_empty());
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
