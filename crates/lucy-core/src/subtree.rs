use serde::Serialize;

use crate::{ConfigError, SourceConfig};

const SUBTREE_MAGIC: &[u8; 4] = b"subt";
const SUBTREE_VERSION: u32 = 1;
const BYTE_ALIGNMENT: usize = 8;

pub fn generate_root_subtree_json(source: &SourceConfig) -> Result<String, ConfigError> {
    let subtree = generate_root_subtree(source)?;

    serde_json::to_string_pretty(&subtree)
        .map_err(|error| ConfigError::Validation(format!("failed to encode subtree JSON: {error}")))
}

pub fn generate_root_subtree_bytes(source: &SourceConfig) -> Result<Vec<u8>, ConfigError> {
    let json = generate_root_subtree_json(source)?;
    Ok(encode_subtree_binary(json.as_bytes()))
}

pub fn encode_subtree_binary(json: &[u8]) -> Vec<u8> {
    let padded_json_length = padded_len(json.len(), BYTE_ALIGNMENT);
    let mut bytes = Vec::with_capacity(24 + padded_json_length);

    bytes.extend_from_slice(SUBTREE_MAGIC);
    bytes.extend_from_slice(&SUBTREE_VERSION.to_le_bytes());
    bytes.extend_from_slice(&(padded_json_length as u64).to_le_bytes());
    bytes.extend_from_slice(&0_u64.to_le_bytes());
    bytes.extend_from_slice(json);
    bytes.extend(std::iter::repeat_n(b' ', padded_json_length - json.len()));

    bytes
}

pub fn generate_root_subtree(source: &SourceConfig) -> Result<Subtree, ConfigError> {
    if source.min_level != 0 {
        return Err(ConfigError::Validation(
            "Phase 0 root subtree generation requires min_level = 0".to_string(),
        ));
    }

    if source.subtree_levels == 0 {
        return Err(ConfigError::Validation(
            "subtree_levels must be greater than zero".to_string(),
        ));
    }

    let child_subtrees_available =
        u8::from(u32::from(source.max_level) + 1 > u32::from(source.subtree_levels));

    Ok(Subtree {
        tile_availability: Availability::constant(1),
        content_availability: vec![Availability::constant(1)],
        child_subtree_availability: Availability::constant(child_subtrees_available),
    })
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Subtree {
    pub tile_availability: Availability,
    pub content_availability: Vec<Availability>,
    pub child_subtree_availability: Availability,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Availability {
    pub constant: u8,
}

impl Availability {
    fn constant(value: u8) -> Self {
        Self { constant: value }
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
    use crate::SourceCatalog;

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
