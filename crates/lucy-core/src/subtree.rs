use serde::Serialize;

use crate::source::{ConfigError, MAX_SUBTREE_LEVELS, SourceConfig};
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
    encode_generated_subtree(&subtree)
}

pub fn generate_subtree_bytes_with_availability(
    source: &SourceConfig,
    subtree_root: TileCoord,
    availability: &SubtreeAvailabilityBits,
) -> Result<Vec<u8>, ConfigError> {
    let subtree = generate_subtree_with_availability(source, subtree_root, availability)?;
    encode_generated_subtree(&subtree)
}

fn encode_generated_subtree(subtree: &Subtree) -> Result<Vec<u8>, ConfigError> {
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
    let layout = subtree_layout(source, subtree_root)?;
    let availability = SubtreeAvailabilityBits {
        tile: layout.local_tiles.iter().map(Option::is_some).collect(),
        content: layout
            .local_tiles
            .iter()
            .map(|tile| tile.is_some_and(|tile| tile.level >= source.tileset.content_start_level))
            .collect(),
        child_subtree: layout.child_roots.iter().map(Option::is_some).collect(),
    };

    generate_subtree_with_availability(source, subtree_root, &availability)
}

pub fn subtree_layout(
    source: &SourceConfig,
    subtree_root: TileCoord,
) -> Result<SubtreeLayout, ConfigError> {
    validate_subtree_root(source, subtree_root)?;

    if source.subtree_levels > MAX_SUBTREE_LEVELS {
        return Err(ConfigError::Validation(format!(
            "subtree_levels greater than {MAX_SUBTREE_LEVELS} would create impractically large availability arrays"
        )));
    }

    let mut local_tiles = vec![None; quadtree_node_count(source.subtree_levels)?];
    for local_level in 0..source.subtree_levels {
        let absolute_level = u16::from(subtree_root.level) + u16::from(local_level);
        if absolute_level > u16::from(source.max_level) {
            continue;
        }

        let width = 1_u32
            .checked_shl(u32::from(local_level))
            .ok_or_else(|| ConfigError::Validation("local_level is too deep".to_string()))?;
        for local_y in 0..width {
            for local_x in 0..width {
                let index = quadtree_availability_index(local_level, local_x, local_y)?;
                local_tiles[index] = Some(descendant_coord(
                    subtree_root,
                    local_level,
                    local_x,
                    local_y,
                )?);
            }
        }
    }

    let mut child_roots = vec![None; quadtree_child_subtree_count(source.subtree_levels)?];
    let child_level = u16::from(subtree_root.level) + u16::from(source.subtree_levels);
    if child_level <= u16::from(source.max_level) {
        let width = 1_u32
            .checked_shl(u32::from(source.subtree_levels))
            .ok_or_else(|| ConfigError::Validation("subtree_levels is too deep".to_string()))?;
        for local_y in 0..width {
            for local_x in 0..width {
                let index = morton_index_2d(local_x, local_y);
                child_roots[index] = Some(descendant_coord(
                    subtree_root,
                    source.subtree_levels,
                    local_x,
                    local_y,
                )?);
            }
        }
    }

    Ok(SubtreeLayout {
        local_tiles,
        child_roots,
    })
}

pub fn generate_subtree_with_availability(
    source: &SourceConfig,
    subtree_root: TileCoord,
    availability: &SubtreeAvailabilityBits,
) -> Result<Subtree, ConfigError> {
    let layout = subtree_layout(source, subtree_root)?;
    validate_availability(source, &layout, availability)?;

    let mut builder = AvailabilityBuilder::default();
    let tile_availability = builder.append(&availability.tile);
    let content_availability = builder.append(&availability.content);
    let child_subtree_availability = builder.append(&availability.child_subtree);
    let (buffers, buffer_views, binary) = builder.finish();

    Ok(Subtree {
        buffers,
        buffer_views,
        tile_availability,
        content_availability: vec![content_availability],
        child_subtree_availability,
        binary,
    })
}

fn validate_subtree_root(
    source: &SourceConfig,
    subtree_root: TileCoord,
) -> Result<(), ConfigError> {
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

    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubtreeLayout {
    pub local_tiles: Vec<Option<TileCoord>>,
    pub child_roots: Vec<Option<TileCoord>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubtreeAvailabilityBits {
    pub tile: Vec<bool>,
    pub content: Vec<bool>,
    pub child_subtree: Vec<bool>,
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

#[derive(Default)]
struct AvailabilityBuilder {
    buffer_views: Vec<SubtreeBufferView>,
    binary: Vec<u8>,
}

impl AvailabilityBuilder {
    fn append(&mut self, bits: &[bool]) -> Availability {
        let available_count = bits.iter().filter(|available| **available).count();
        if available_count == 0 {
            return Availability::constant(0);
        }
        if available_count == bits.len() {
            return Availability::constant(1);
        }

        pad_binary(&mut self.binary);
        let byte_offset = self.binary.len();
        let packed = pack_availability_bits(bits);
        let byte_length = packed.len();
        self.binary.extend_from_slice(&packed);

        let bitstream = self.buffer_views.len();
        self.buffer_views.push(SubtreeBufferView {
            buffer: 0,
            byte_offset,
            byte_length,
        });

        Availability::bitstream(bitstream, available_count)
    }

    fn finish(mut self) -> (Vec<SubtreeBuffer>, Vec<SubtreeBufferView>, Vec<u8>) {
        if self.buffer_views.is_empty() {
            return (Vec::new(), Vec::new(), Vec::new());
        }

        pad_binary(&mut self.binary);
        let buffers = vec![SubtreeBuffer {
            byte_length: self.binary.len(),
        }];
        (buffers, self.buffer_views, self.binary)
    }
}

fn validate_availability(
    source: &SourceConfig,
    layout: &SubtreeLayout,
    availability: &SubtreeAvailabilityBits,
) -> Result<(), ConfigError> {
    for (field, actual, expected) in [
        (
            "tile availability",
            availability.tile.len(),
            layout.local_tiles.len(),
        ),
        (
            "content availability",
            availability.content.len(),
            layout.local_tiles.len(),
        ),
        (
            "child subtree availability",
            availability.child_subtree.len(),
            layout.child_roots.len(),
        ),
    ] {
        if actual != expected {
            return Err(ConfigError::Validation(format!(
                "{field} has {actual} bits; expected {expected}"
            )));
        }
    }

    if !availability.tile[0] {
        return Err(ConfigError::Validation(
            "tile availability must include the subtree root".to_string(),
        ));
    }

    for (index, tile) in layout.local_tiles.iter().enumerate() {
        if tile.is_none() && (availability.tile[index] || availability.content[index]) {
            return Err(ConfigError::Validation(format!(
                "availability index {index} is beyond max_level {}",
                source.max_level
            )));
        }
        if availability.content[index] && !availability.tile[index] {
            return Err(ConfigError::Validation(format!(
                "content availability index {index} requires tile availability"
            )));
        }
    }

    for local_level in 1..source.subtree_levels {
        let width = 1_u32 << local_level;
        for local_y in 0..width {
            for local_x in 0..width {
                let index = quadtree_availability_index(local_level, local_x, local_y)?;
                if !availability.tile[index] {
                    continue;
                }

                let parent =
                    quadtree_availability_index(local_level - 1, local_x / 2, local_y / 2)?;
                if !availability.tile[parent] {
                    return Err(ConfigError::Validation(format!(
                        "tile availability index {index} requires ancestor index {parent}"
                    )));
                }
            }
        }
    }

    let child_width = 1_u32 << source.subtree_levels;
    for local_y in 0..child_width {
        for local_x in 0..child_width {
            let index = morton_index_2d(local_x, local_y);
            if !availability.child_subtree[index] {
                continue;
            }
            if layout.child_roots[index].is_none() {
                return Err(ConfigError::Validation(format!(
                    "child subtree availability index {index} is beyond max_level {}",
                    source.max_level
                )));
            }

            let parent =
                quadtree_availability_index(source.subtree_levels - 1, local_x / 2, local_y / 2)?;
            if !availability.tile[parent] {
                return Err(ConfigError::Validation(format!(
                    "child subtree availability index {index} requires parent tile index {parent}"
                )));
            }
        }
    }

    Ok(())
}

fn descendant_coord(
    subtree_root: TileCoord,
    local_level: u8,
    local_x: u32,
    local_y: u32,
) -> Result<TileCoord, ConfigError> {
    let level = subtree_root
        .level
        .checked_add(local_level)
        .ok_or_else(|| ConfigError::Validation("tile level overflowed".to_string()))?;
    let x = subtree_root
        .x
        .checked_shl(u32::from(local_level))
        .and_then(|base| base.checked_add(local_x))
        .ok_or_else(|| ConfigError::Validation("tile x coordinate overflowed".to_string()))?;
    let y = subtree_root
        .y
        .checked_shl(u32::from(local_level))
        .and_then(|base| base.checked_add(local_y))
        .ok_or_else(|| ConfigError::Validation("tile y coordinate overflowed".to_string()))?;

    TileCoord::new(level, x, y).map_err(|error| ConfigError::Validation(error.to_string()))
}

fn pad_binary(binary: &mut Vec<u8>) {
    binary.extend(std::iter::repeat_n(
        0,
        padded_len(binary.len(), BYTE_ALIGNMENT) - binary.len(),
    ));
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
            vec![Availability::bitstream(1, 5)]
        );
        assert_eq!(
            subtree.child_subtree_availability,
            Availability::constant(0)
        );
        assert_eq!(subtree.buffers[0].byte_length, 32);
        assert_eq!(subtree.buffer_views[0].byte_length, 11);
        assert_eq!(subtree.buffer_views[1].byte_offset, 16);
        assert_eq!(subtree.buffer_views[1].byte_length, 11);
        assert_eq!(subtree.binary.len(), 32);
        assert_eq!(subtree.binary[0], 0b0001_1111);
        assert_eq!(subtree.binary[16], 0b0001_1111);
        assert!(subtree.binary[1..16].iter().all(|byte| *byte == 0));
        assert!(subtree.binary[17..].iter().all(|byte| *byte == 0));

        let bytes = generate_subtree_bytes(&source, subtree_root).expect("binary subtree");
        let binary_byte_len = u64::from_le_bytes(bytes[16..24].try_into().expect("binary length"));
        assert_eq!(binary_byte_len, 32);
    }

    #[test]
    fn subtree_layout_uses_morton_order_and_marks_partial_slots() {
        let mut source = fixture_source();
        source.max_level = 5;
        let root = TileCoord::new(4, 3, 7).expect("valid subtree root");
        let layout = subtree_layout(&source, root).expect("layout should build");

        assert_eq!(layout.local_tiles.len(), 85);
        assert_eq!(layout.local_tiles[0], Some(root));
        assert_eq!(
            layout.local_tiles[1],
            Some(TileCoord::new(5, 6, 14).expect("southwest child"))
        );
        assert_eq!(
            layout.local_tiles[2],
            Some(TileCoord::new(5, 7, 14).expect("southeast child"))
        );
        assert!(layout.local_tiles[5..].iter().all(Option::is_none));
        assert_eq!(layout.child_roots.len(), 256);
        assert!(layout.child_roots.iter().all(Option::is_none));
    }

    #[test]
    fn encodes_sparse_availability_with_aligned_buffer_views() {
        let mut source = fixture_source();
        source.subtree_levels = 2;
        source.max_level = 4;
        let availability = SubtreeAvailabilityBits {
            tile: vec![true, true, false, false, false],
            content: vec![false, true, false, false, false],
            child_subtree: vec![
                true, false, false, false, false, false, false, false, false, false, false, false,
                false, false, false, false,
            ],
        };

        let subtree = generate_subtree_with_availability(&source, TileCoord::root(), &availability)
            .expect("sparse subtree should generate");

        assert_eq!(subtree.tile_availability, Availability::bitstream(0, 2));
        assert_eq!(
            subtree.content_availability,
            vec![Availability::bitstream(1, 1)]
        );
        assert_eq!(
            subtree.child_subtree_availability,
            Availability::bitstream(2, 1)
        );
        assert_eq!(
            subtree
                .buffer_views
                .iter()
                .map(|view| view.byte_offset)
                .collect::<Vec<_>>(),
            vec![0, 8, 16]
        );
        assert_eq!(subtree.buffers[0].byte_length, 24);
        assert_eq!(subtree.binary.len(), 24);
    }

    #[test]
    fn validates_sparse_availability_hierarchy_and_configured_levels() {
        let mut source = fixture_source();
        source.subtree_levels = 2;
        source.max_level = 1;

        let content_without_tile = SubtreeAvailabilityBits {
            tile: vec![true, false, false, false, false],
            content: vec![false, true, false, false, false],
            child_subtree: vec![false; 16],
        };
        let error =
            generate_subtree_with_availability(&source, TileCoord::root(), &content_without_tile)
                .expect_err("content without a tile should fail");
        assert!(error.to_string().contains("requires tile availability"));

        let beyond_max = SubtreeAvailabilityBits {
            tile: vec![true, true, false, false, false],
            content: vec![false; 5],
            child_subtree: {
                let mut bits = vec![false; 16];
                bits[0] = true;
                bits
            },
        };
        let error = generate_subtree_with_availability(&source, TileCoord::root(), &beyond_max)
            .expect_err("child roots beyond max should fail");
        assert!(error.to_string().contains("beyond max_level"));
    }

    #[test]
    fn allows_an_empty_dataset_only_when_the_global_root_tile_exists() {
        let mut source = fixture_source();
        source.subtree_levels = 2;
        source.max_level = 4;
        let empty_root = SubtreeAvailabilityBits {
            tile: vec![true, false, false, false, false],
            content: vec![false; 5],
            child_subtree: vec![false; 16],
        };
        let subtree = generate_subtree_with_availability(&source, TileCoord::root(), &empty_root)
            .expect("global root tile should remain available");
        assert_eq!(
            subtree.content_availability,
            vec![Availability::constant(0)]
        );

        let empty_non_root = SubtreeAvailabilityBits {
            tile: vec![false; 5],
            content: vec![false; 5],
            child_subtree: vec![false; 16],
        };
        let root = TileCoord::new(2, 0, 0).expect("valid subtree root");
        let error = generate_subtree_with_availability(&source, root, &empty_non_root)
            .expect_err("empty non-root subtree should not be encoded");
        assert!(error.to_string().contains("must include the subtree root"));
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
