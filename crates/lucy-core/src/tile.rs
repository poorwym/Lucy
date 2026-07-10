use std::fmt;

use crate::source::{ConfigError, SourceBounds};

pub const MAX_TILE_LEVEL: u8 = u32::BITS as u8 - 1;

/// Phase 0 QUADTREE math.
///
/// Tile coordinates subdivide the configured source bounds evenly at each level:
/// `x` increases from west to east, and `y` increases from south to north. The
/// 3D Tiles `region` output uses `[west, south, east, north, minHeight,
/// maxHeight]`, with angular values in radians and heights in meters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileCoord {
    pub level: u8,
    pub x: u32,
    pub y: u32,
}

impl TileCoord {
    pub fn new(level: u8, x: u32, y: u32) -> Result<Self, TileCoordError> {
        let tile_count = tile_count_for_level(level)?;

        if x >= tile_count || y >= tile_count {
            return Err(TileCoordError::OutOfRange {
                level,
                x,
                y,
                tile_count,
            });
        }

        Ok(Self { level, x, y })
    }

    pub fn root() -> Self {
        Self {
            level: 0,
            x: 0,
            y: 0,
        }
    }

    pub fn geographic_region_degrees(
        self,
        source_bounds: &SourceBounds,
    ) -> Result<GeographicRegionDegrees, ConfigError> {
        source_bounds.validate_region("tile source bounds")?;

        let tile_count = tile_count_for_level(self.level)
            .map_err(|error| ConfigError::Validation(error.to_string()))?
            as f64;
        if self.x >= tile_count as u32 || self.y >= tile_count as u32 {
            return Err(ConfigError::Validation(
                TileCoordError::OutOfRange {
                    level: self.level,
                    x: self.x,
                    y: self.y,
                    tile_count: tile_count as u32,
                }
                .to_string(),
            ));
        }

        let x = f64::from(self.x);
        let y = f64::from(self.y);
        let longitude_span = source_bounds.east - source_bounds.west;
        let latitude_span = source_bounds.north - source_bounds.south;

        Ok(GeographicRegionDegrees {
            west: source_bounds.west + longitude_span * (x / tile_count),
            south: source_bounds.south + latitude_span * (y / tile_count),
            east: source_bounds.west + longitude_span * ((x + 1.0) / tile_count),
            north: source_bounds.south + latitude_span * ((y + 1.0) / tile_count),
            min_height_m: source_bounds.min_height_m,
            max_height_m: source_bounds.max_height_m,
        })
    }

    pub fn tiles_region(self, source_bounds: &SourceBounds) -> Result<TilesRegion, ConfigError> {
        Ok(self
            .geographic_region_degrees(source_bounds)?
            .to_tiles_region())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeographicRegionDegrees {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
    pub min_height_m: f64,
    pub max_height_m: f64,
}

impl GeographicRegionDegrees {
    pub fn to_tiles_region(self) -> TilesRegion {
        TilesRegion {
            west: self.west.to_radians(),
            south: self.south.to_radians(),
            east: self.east.to_radians(),
            north: self.north.to_radians(),
            min_height_m: self.min_height_m,
            max_height_m: self.max_height_m,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TilesRegion {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
    pub min_height_m: f64,
    pub max_height_m: f64,
}

impl TilesRegion {
    pub fn as_array(self) -> [f64; 6] {
        [
            self.west,
            self.south,
            self.east,
            self.north,
            self.min_height_m,
            self.max_height_m,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TileCoordError {
    LevelTooDeep {
        level: u8,
    },
    OutOfRange {
        level: u8,
        x: u32,
        y: u32,
        tile_count: u32,
    },
}

impl fmt::Display for TileCoordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TileCoordError::LevelTooDeep { level } => {
                write!(f, "level {level} is too deep for u32 tile coordinates")
            }
            TileCoordError::OutOfRange {
                level,
                x,
                y,
                tile_count,
            } => write!(
                f,
                "tile coordinate level={level} x={x} y={y} is outside 0..{}",
                tile_count.saturating_sub(1)
            ),
        }
    }
}

impl std::error::Error for TileCoordError {}

fn tile_count_for_level(level: u8) -> Result<u32, TileCoordError> {
    if level >= u32::BITS as u8 {
        return Err(TileCoordError::LevelTooDeep { level });
    }

    Ok(1_u32 << level)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_bounds() -> SourceBounds {
        SourceBounds {
            west: -122.40130,
            south: 37.79245,
            east: -122.39975,
            north: 37.79375,
            min_height_m: 0.0,
            max_height_m: 100.0,
        }
    }

    fn assert_close(actual: f64, expected: f64) {
        let delta = (actual - expected).abs();
        assert!(
            delta < 1e-12,
            "actual {actual} differs from expected {expected} by {delta}"
        );
    }

    #[test]
    fn root_region_matches_source_bounds() {
        let bounds = fixture_bounds();
        let region = TileCoord::root()
            .geographic_region_degrees(&bounds)
            .expect("root region should be valid");

        assert_close(region.west, bounds.west);
        assert_close(region.south, bounds.south);
        assert_close(region.east, bounds.east);
        assert_close(region.north, bounds.north);
        assert_close(region.min_height_m, bounds.min_height_m);
        assert_close(region.max_height_m, bounds.max_height_m);
    }

    #[test]
    fn level_one_children_are_deterministic() {
        let bounds = fixture_bounds();
        let mid_lon = (bounds.west + bounds.east) / 2.0;
        let mid_lat = (bounds.south + bounds.north) / 2.0;

        let southwest = TileCoord::new(1, 0, 0)
            .expect("valid coord")
            .geographic_region_degrees(&bounds)
            .expect("valid region");
        assert_close(southwest.west, bounds.west);
        assert_close(southwest.south, bounds.south);
        assert_close(southwest.east, mid_lon);
        assert_close(southwest.north, mid_lat);

        let northeast = TileCoord::new(1, 1, 1)
            .expect("valid coord")
            .geographic_region_degrees(&bounds)
            .expect("valid region");
        assert_close(northeast.west, mid_lon);
        assert_close(northeast.south, mid_lat);
        assert_close(northeast.east, bounds.east);
        assert_close(northeast.north, bounds.north);
    }

    #[test]
    fn level_two_child_region_is_deterministic() {
        let bounds = fixture_bounds();
        let region = TileCoord::new(2, 2, 1)
            .expect("valid coord")
            .geographic_region_degrees(&bounds)
            .expect("valid region");

        assert_close(region.west, -122.400525);
        assert_close(region.south, 37.792775);
        assert_close(region.east, -122.4001375);
        assert_close(region.north, 37.79310);
    }

    #[test]
    fn tiles_region_uses_radians_and_meter_heights() {
        let bounds = fixture_bounds();
        let region = TileCoord::new(1, 0, 1)
            .expect("valid coord")
            .tiles_region(&bounds)
            .expect("valid tiles region")
            .as_array();

        assert_close(region[0], bounds.west.to_radians());
        assert_close(
            region[1],
            ((bounds.south + bounds.north) / 2.0).to_radians(),
        );
        assert_close(region[2], ((bounds.west + bounds.east) / 2.0).to_radians());
        assert_close(region[3], bounds.north.to_radians());
        assert_close(region[4], 0.0);
        assert_close(region[5], 100.0);
    }

    #[test]
    fn invalid_coordinates_are_rejected() {
        assert_eq!(
            TileCoord::new(1, 2, 0),
            Err(TileCoordError::OutOfRange {
                level: 1,
                x: 2,
                y: 0,
                tile_count: 2
            })
        );
        assert_eq!(
            TileCoord::new(32, 0, 0),
            Err(TileCoordError::LevelTooDeep { level: 32 })
        );
    }
}
