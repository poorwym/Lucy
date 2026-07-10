use serde::Serialize;

use crate::source::{ConfigError, SourceConfig};
use crate::tile::TileCoord;

const SUBTREE_MAGIC: &[u8; 4] = b"subt";
const SUBTREE_VERSION: u32 = 1;
const BYTE_ALIGNMENT: usize = 8;

pub fn generate_root_subtree_json(source: &SourceConfig) -> Result<String, ConfigError> {
    generate_subtree_json(source, TileCoord::root())
}

pub fn generate_subtree_json(
    source: &SourceConfig,
    subtree_root: TileCoord,
) -> Result<String, ConfigError> {
    let subtree = generate_subtree(source, subtree_root)?;

    serde_json::to_string_pretty(&subtree)
        .map_err(|error| ConfigError::Validation(format!("failed to encode subtree JSON: {error}")))
}

pub fn generate_root_subtree_bytes(source: &SourceConfig) -> Result<Vec<u8>, ConfigError> {
    generate_subtree_bytes(source, TileCoord::root())
}

pub fn generate_subtree_bytes(
    source: &SourceConfig,
    subtree_root: TileCoord,
) -> Result<Vec<u8>, ConfigError> {
    let subtree = generate_subtree(source, subtree_root)?;
    let json = serde_json::to_vec_pretty(&subtree).map_err(|error| {
        ConfigError::Validation(format!("failed to encode subtree JSON: {error}"))
    })?;

    Ok(encode_subtree_binary_with_buffer(&json, &subtree.binary))
}

pub fn encode_subtree_binary(json: &[u8]) -> Vec<u8> {
    encode_subtree_binary_with_buffer(json, &[])
}

fn encode_subtree_binary_with_buffer(json: &[u8], binary: &[u8]) -> Vec<u8> {
    let padded_json_length = padded_len(json.len(), BYTE_ALIGNMENT);
    let padded_binary_length = padded_len(binary.len(), BYTE_ALIGNMENT);
    let mut bytes = Vec::with_capacity(24 + padded_json_length + padded_binary_length);

    bytes.extend_from_slice(SUBTREE_MAGIC);
    bytes.extend_from_slice(&SUBTREE_VERSION.to_le_bytes());
    bytes.extend_from_slice(&(padded_json_length as u64).to_le_bytes());
    bytes.extend_from_slice(&(padded_binary_length as u64).to_le_bytes());
    bytes.extend_from_slice(json);
    bytes.extend(std::iter::repeat_n(b' ', padded_json_length - json.len()));
    bytes.extend_from_slice(binary);
    bytes.extend(std::iter::repeat_n(0, padded_binary_length - binary.len()));

    bytes
}

pub fn generate_root_subtree(source: &SourceConfig) -> Result<Subtree, ConfigError> {
    generate_subtree(source, TileCoord::root())
}

pub fn generate_subtree(
    source: &SourceConfig,
    subtree_root: TileCoord,
) -> Result<Subtree, ConfigError> {
    if source.min_level != 0 {
        return Err(ConfigError::Validation(
            "implicit subtree generation requires min_level = 0".to_string(),
        ));
    }

    if source.subtree_levels == 0 {
        return Err(ConfigError::Validation(
            "subtree_levels must be greater than zero".to_string(),
        ));
    }

    if subtree_root.level < source.min_level || subtree_root.level > source.max_level {
        return Err(ConfigError::Validation(format!(
            "subtree root level {} is outside configured levels {}..={}",
            subtree_root.level, source.min_level, source.max_level
        )));
    }

    if !subtree_root.level.is_multiple_of(source.subtree_levels) {
        return Err(ConfigError::Validation(format!(
            "level {} is not a subtree root; expected a multiple of subtree_levels {}",
            subtree_root.level, source.subtree_levels
        )));
    }

    let remaining_levels = u16::from(source.max_level) - u16::from(subtree_root.level) + 1;
    let local_available_levels =
        u8::try_from(remaining_levels.min(u16::from(source.subtree_levels)))
            .expect("subtree level count is bounded by u8");
    let next_subtree_level = u16::from(subtree_root.level) + u16::from(source.subtree_levels);
    let child_subtrees_available = u8::from(next_subtree_level <= u16::from(source.max_level));

    let (buffers, buffer_views, binary, tile_availability, content_availability) =
        if local_available_levels == source.subtree_levels {
            (
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Availability::constant(1),
                Availability::constant(1),
            )
        } else {
            let total_node_count = quadtree_node_count(source.subtree_levels)?;
            let available_node_count = quadtree_node_count(local_available_levels)?;
            let mut bits = vec![false; total_node_count];
            bits[..available_node_count].fill(true);
            let binary = pack_availability_bits(&bits);
            let byte_length = binary.len();
            let availability = Availability::bitstream(0, available_node_count);

            (
                vec![SubtreeBuffer { byte_length }],
                vec![SubtreeBufferView {
                    buffer: 0,
                    byte_offset: 0,
                    byte_length,
                }],
                binary,
                availability.clone(),
                availability,
            )
        };

    Ok(Subtree {
        buffers,
        buffer_views,
        tile_availability,
        content_availability: vec![content_availability],
        child_subtree_availability: Availability::constant(child_subtrees_available),
        binary,
    })
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Subtree {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub buffers: Vec<SubtreeBuffer>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub buffer_views: Vec<SubtreeBufferView>,
    pub tile_availability: Availability,
    pub content_availability: Vec<Availability>,
    pub child_subtree_availability: Availability,
    #[serde(skip)]
    pub binary: Vec<u8>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubtreeBuffer {
    pub byte_length: usize,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubtreeBufferView {
    pub buffer: usize,
    pub byte_offset: usize,
    pub byte_length: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Availability {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constant: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitstream: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_count: Option<usize>,
}

impl Availability {
    fn constant(value: u8) -> Self {
        Self {
            constant: Some(value),
            bitstream: None,
            available_count: None,
        }
    }

    fn bitstream(bitstream: usize, available_count: usize) -> Self {
        Self {
            constant: None,
            bitstream: Some(bitstream),
            available_count: Some(available_count),
        }
    }
}

pub fn quadtree_node_count(subtree_levels: u8) -> Result<usize, ConfigError> {
    if subtree_levels == 0 {
        return Err(ConfigError::Validation(
            "subtree_levels must be greater than zero".to_string(),
        ));
    }

    let mut total = 0_usize;
    for level in 0..subtree_levels {
        total = total
            .checked_add(4_usize.pow(u32::from(level)))
            .ok_or_else(|| ConfigError::Validation("subtree node count overflowed".to_string()))?;
    }

    Ok(total)
}

pub fn quadtree_child_subtree_count(subtree_levels: u8) -> Result<usize, ConfigError> {
    if subtree_levels == 0 {
        return Err(ConfigError::Validation(
            "subtree_levels must be greater than zero".to_string(),
        ));
    }

    4_usize
        .checked_pow(u32::from(subtree_levels))
        .ok_or_else(|| ConfigError::Validation("child subtree count overflowed".to_string()))
}

pub fn quadtree_availability_index(
    local_level: u8,
    local_x: u32,
    local_y: u32,
) -> Result<usize, ConfigError> {
    let level_width = 1_u32
        .checked_shl(u32::from(local_level))
        .ok_or_else(|| ConfigError::Validation("local_level is too deep".to_string()))?;

    if local_x >= level_width || local_y >= level_width {
        return Err(ConfigError::Validation(format!(
            "local coordinate level={local_level} x={local_x} y={local_y} is outside 0..{}",
            level_width.saturating_sub(1)
        )));
    }

    let level_offset = ((4_usize.pow(u32::from(local_level))) - 1) / 3;
    Ok(level_offset + morton_index_2d(local_x, local_y))
}

pub fn morton_index_2d(x: u32, y: u32) -> usize {
    let mut index = 0_usize;

    for bit in 0..u32::BITS {
        index |= (((x >> bit) & 1) as usize) << (2 * bit);
        index |= (((y >> bit) & 1) as usize) << (2 * bit + 1);
    }

    index
}

pub fn pack_availability_bits(bits: &[bool]) -> Vec<u8> {
    let mut bytes = vec![0_u8; bits.len().div_ceil(8)];

    for (index, available) in bits.iter().copied().enumerate() {
        if available {
            bytes[index / 8] |= 1 << (index % 8);
        }
    }

    bytes
}

fn padded_len(length: usize, alignment: usize) -> usize {
    length.div_ceil(alignment) * alignment
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SourceCatalog;

    fn fixture_source() -> SourceConfig {
        let mut catalog =
            SourceCatalog::from_yaml_str(include_str!("../../../config/poc-sources.yaml"))
                .expect("fixture config should load");
        catalog
            .sources
            .remove("poc_buildings")
            .expect("poc source should exist")
    }

    #[test]
    fn generates_root_subtree_json_golden() {
        let source = fixture_source();
        let json = generate_root_subtree_json(&source).expect("root subtree JSON should generate");
        let expected = include_str!("../tests/golden/poc_buildings_root.subtree.json").trim_end();

        assert_eq!(json, expected);
    }

    #[test]
    fn generates_first_non_root_subtree() {
        let source = fixture_source();
        let subtree_root = TileCoord::new(source.subtree_levels, 3, 7).expect("valid root");
        let subtree = generate_subtree(&source, subtree_root).expect("subtree should generate");

        assert_eq!(subtree.tile_availability, Availability::constant(1));
        assert_eq!(
            subtree.content_availability,
            vec![Availability::constant(1)]
        );
        assert_eq!(
            subtree.child_subtree_availability,
            Availability::constant(1)
        );
        assert!(subtree.buffers.is_empty());
        assert!(subtree.binary.is_empty());
    }

    #[test]
    fn clips_final_partial_subtree_at_max_level() {
        let mut source = fixture_source();
        source.max_level = 5;
        let subtree_root = TileCoord::new(4, 9, 6).expect("valid root");
        let subtree = generate_subtree(&source, subtree_root).expect("subtree should generate");

        assert_eq!(subtree.tile_availability, Availability::bitstream(0, 5));
        assert_eq!(
            subtree.content_availability,
            vec![Availability::bitstream(0, 5)]
        );
        assert_eq!(
            subtree.child_subtree_availability,
            Availability::constant(0)
        );
        assert_eq!(subtree.buffers[0].byte_length, 11);
        assert_eq!(subtree.buffer_views[0].byte_length, 11);
        assert_eq!(subtree.binary.len(), 11);
        assert_eq!(subtree.binary[0], 0b0001_1111);
        assert!(subtree.binary[1..].iter().all(|byte| *byte == 0));

        let bytes = generate_subtree_bytes(&source, subtree_root).expect("binary subtree");
        let binary_byte_len = u64::from_le_bytes(bytes[16..24].try_into().expect("binary length"));
        assert_eq!(binary_byte_len, 16);
    }

    #[test]
    fn rejects_non_root_levels_and_levels_outside_source_bounds() {
        let source = fixture_source();
        let non_root = TileCoord::new(1, 0, 0).expect("valid tile coordinate");
        let error = generate_subtree(&source, non_root).expect_err("level should be rejected");
        assert!(error.to_string().contains("not a subtree root"));

        let beyond_max = TileCoord::new(20, 0, 0).expect("valid tile coordinate");
        let error =
            generate_subtree(&source, beyond_max).expect_err("level should be rejected early");
        assert!(error.to_string().contains("outside configured levels"));
    }

    #[test]
    fn encodes_binary_subtree_header_and_padding() {
        let source = fixture_source();
        let json = generate_root_subtree_json(&source).expect("root subtree JSON should generate");
        let bytes = generate_root_subtree_bytes(&source).expect("root subtree should generate");
        let json_byte_len = u64::from_le_bytes(bytes[8..16].try_into().expect("json length"));
        let binary_byte_len = u64::from_le_bytes(bytes[16..24].try_into().expect("binary length"));

        assert_eq!(&bytes[0..4], b"subt");
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().expect("version")),
            1
        );
        assert_eq!(
            json_byte_len as usize,
            padded_len(json.len(), BYTE_ALIGNMENT)
        );
        assert_eq!(binary_byte_len, 0);
        assert_eq!(&bytes[24..24 + json.len()], json.as_bytes());
        assert!(bytes[24 + json.len()..].iter().all(|byte| *byte == b' '));
    }

    #[test]
    fn quadtree_counts_match_subtree_levels() {
        assert_eq!(quadtree_node_count(1).expect("count"), 1);
        assert_eq!(quadtree_node_count(4).expect("count"), 85);
        assert_eq!(quadtree_child_subtree_count(4).expect("count"), 256);
    }

    #[test]
    fn quadtree_availability_indices_use_morton_order_per_level() {
        assert_eq!(quadtree_availability_index(0, 0, 0).expect("index"), 0);
        assert_eq!(quadtree_availability_index(1, 0, 0).expect("index"), 1);
        assert_eq!(quadtree_availability_index(1, 1, 0).expect("index"), 2);
        assert_eq!(quadtree_availability_index(1, 0, 1).expect("index"), 3);
        assert_eq!(quadtree_availability_index(1, 1, 1).expect("index"), 4);
        assert_eq!(quadtree_availability_index(2, 2, 1).expect("index"), 11);
    }

    #[test]
    fn packs_availability_bits_little_endian_with_zero_trailing_bits() {
        let bytes =
            pack_availability_bits(&[true, false, true, true, false, false, false, true, true]);

        assert_eq!(bytes, vec![0b1000_1101, 0b0000_0001]);
    }
}
