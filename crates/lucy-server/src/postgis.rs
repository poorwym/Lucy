use std::collections::BTreeMap;
use std::fmt;

use tokio_postgres::GenericClient;

use lucy_core::source::{ConfigError, SourceConfig};
use lucy_core::subtree::{SubtreeAvailabilityBits, subtree_layout};
use lucy_core::tile::{GeographicRegionDegrees, TileCoord};

/// One PostGIS feature clipped to a requested tile bbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileFeatureWkb {
    pub id: String,
    pub geometry_wkb: Vec<u8>,
    pub attributes: BTreeMap<String, Option<String>>,
}

/// Query a tile bbox from PostGIS and return positive-area clipped geometry as WKB.
pub async fn query_tile_geometry_wkb(
    client: &impl GenericClient,
    source: &SourceConfig,
    tile: TileCoord,
) -> Result<Vec<TileFeatureWkb>, TileQueryError> {
    let bbox = tile.geographic_region_degrees(&source.bounds)?;
    query_tile_geometry_wkb_for_bbox(client, source, bbox).await
}

/// Query an explicit geographic bbox from PostGIS and return clipped geometry as WKB.
pub async fn query_tile_geometry_wkb_for_bbox(
    client: &impl GenericClient,
    source: &SourceConfig,
    bbox: GeographicRegionDegrees,
) -> Result<Vec<TileFeatureWkb>, TileQueryError> {
    validate_query_bbox(bbox)?;

    let plan = build_tile_wkb_query(source)?;
    let query_limit = i64::from(source.max_features_per_tile) + 1;
    let rows = client
        .query(
            &plan.sql,
            &[
                &bbox.west,
                &bbox.south,
                &bbox.east,
                &bbox.north,
                &source.srid,
                &query_limit,
            ],
        )
        .await?;
    ensure_within_feature_limit(rows.len(), source.max_features_per_tile)?;

    let mut features = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row.try_get::<_, String>(0)?;
        let geometry_wkb = row.try_get::<_, Vec<u8>>(1)?;
        let mut attributes = BTreeMap::new();

        for (index, attribute) in plan.attributes.iter().enumerate() {
            attributes.insert(
                attribute.clone(),
                row.try_get::<_, Option<String>>(index + 2)?,
            );
        }

        features.push(TileFeatureWkb {
            id,
            geometry_wkb,
            attributes,
        });
    }

    Ok(features)
}

/// Derive all tile, content, and child-subtree availability for one subtree
/// with a single batched PostGIS query.
pub async fn query_subtree_availability(
    client: &impl GenericClient,
    source: &SourceConfig,
    subtree_root: TileCoord,
) -> Result<SubtreeAvailabilityBits, TileQueryError> {
    let layout = subtree_layout(source, subtree_root)?;
    let mut slots = Vec::new();
    for (index, tile) in layout.local_tiles.iter().copied().enumerate() {
        if let Some(tile) = tile {
            slots.push(SubtreeQuerySlot::Tile { index, tile });
        }
    }
    for (index, tile) in layout.child_roots.iter().copied().enumerate() {
        if let Some(tile) = tile {
            slots.push(SubtreeQuerySlot::ChildSubtree { index, tile });
        }
    }

    let mut west = Vec::with_capacity(slots.len());
    let mut south = Vec::with_capacity(slots.len());
    let mut east = Vec::with_capacity(slots.len());
    let mut north = Vec::with_capacity(slots.len());
    for slot in &slots {
        let bbox = slot.tile().geographic_region_degrees(&source.bounds)?;
        west.push(bbox.west);
        south.push(bbox.south);
        east.push(bbox.east);
        north.push(bbox.north);
    }

    let plan = build_subtree_occupancy_query(source)?;
    let query_limit = i64::from(source.max_features_per_tile) + 1;
    let rows = client
        .query(
            &plan.sql,
            &[&west, &south, &east, &north, &source.srid, &query_limit],
        )
        .await?;

    if rows.len() != slots.len() {
        return Err(TileQueryError::Config(ConfigError::Validation(format!(
            "PostGIS returned {} subtree occupancy rows for {} requested slots",
            rows.len(),
            slots.len()
        ))));
    }

    let mut availability = SubtreeAvailabilityBits {
        tile: vec![false; layout.local_tiles.len()],
        content: vec![false; layout.local_tiles.len()],
        child_subtree: vec![false; layout.child_roots.len()],
    };
    for row in rows {
        let slot_index = usize::try_from(row.try_get::<_, i64>(0)?).map_err(|_| {
            ConfigError::Validation("PostGIS returned a negative subtree slot".to_string())
        })?;
        let feature_count = u64::try_from(row.try_get::<_, i64>(1)?).map_err(|_| {
            ConfigError::Validation("PostGIS returned a negative feature count".to_string())
        })?;
        let slot = slots.get(slot_index).ok_or_else(|| {
            ConfigError::Validation(format!(
                "PostGIS returned out-of-range subtree slot {slot_index}"
            ))
        })?;
        let tile = slot.tile();
        let has_features = feature_count > 0;
        let overflow = feature_count > u64::from(source.max_features_per_tile);

        if overflow && tile.level == source.max_level {
            return Err(TileQueryError::TerminalFeatureLimitExceeded {
                level: tile.level,
                x: tile.x,
                y: tile.y,
                max_features_per_tile: source.max_features_per_tile,
            });
        }

        match *slot {
            SubtreeQuerySlot::Tile { index, .. } => {
                availability.tile[index] = has_features;
                availability.content[index] = has_features
                    && !overflow
                    && slot.tile().level >= source.tileset.content_start_level;
            }
            SubtreeQuerySlot::ChildSubtree { index, .. } => {
                availability.child_subtree[index] = has_features;
            }
        }
    }

    if subtree_root == TileCoord::root() {
        availability.tile[0] = true;
    }

    Ok(availability)
}

#[derive(Clone, Copy, Debug)]
enum SubtreeQuerySlot {
    Tile { index: usize, tile: TileCoord },
    ChildSubtree { index: usize, tile: TileCoord },
}

impl SubtreeQuerySlot {
    fn tile(self) -> TileCoord {
        match self {
            Self::Tile { tile, .. } | Self::ChildSubtree { tile, .. } => tile,
        }
    }
}

#[derive(Debug)]
pub enum TileQueryError {
    Config(ConfigError),
    FeatureLimitExceeded {
        max_features_per_tile: u32,
    },
    TerminalFeatureLimitExceeded {
        level: u8,
        x: u32,
        y: u32,
        max_features_per_tile: u32,
    },
    Postgres(tokio_postgres::Error),
}

impl fmt::Display for TileQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TileQueryError::Config(error) => write!(f, "{error}"),
            TileQueryError::FeatureLimitExceeded {
                max_features_per_tile,
            } => write!(
                f,
                "tile contains more than {max_features_per_tile} features; request a deeper tile or raise max_features_per_tile instead of serving truncated content"
            ),
            TileQueryError::TerminalFeatureLimitExceeded {
                level,
                x,
                y,
                max_features_per_tile,
            } => write!(
                f,
                "tile level={level} x={x} y={y} exceeds max_features_per_tile={max_features_per_tile} at max_level and cannot be subdivided"
            ),
            TileQueryError::Postgres(error) => write!(f, "PostGIS tile query failed: {error}"),
        }
    }
}

impl std::error::Error for TileQueryError {}

impl From<ConfigError> for TileQueryError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<tokio_postgres::Error> for TileQueryError {
    fn from(error: tokio_postgres::Error) -> Self {
        Self::Postgres(error)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct TileWkbQueryPlan {
    sql: String,
    attributes: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct SubtreeOccupancyQueryPlan {
    sql: String,
}

fn build_tile_wkb_query(source: &SourceConfig) -> Result<TileWkbQueryPlan, ConfigError> {
    if source.max_features_per_tile == 0 {
        return Err(ConfigError::Validation(
            "max_features_per_tile must be greater than zero".to_string(),
        ));
    }

    let schema = quote_identifier(&source.schema, "schema")?;
    let table = quote_identifier(&source.table, "table")?;
    let id_column = quote_identifier(&source.id_column, "id_column")?;
    let geometry_column = quote_identifier(&source.geometry_column, "geometry_column")?;

    let mut select_columns = vec![
        format!("t.{id_column}::text AS id"),
        "ST_AsBinary(clipped.geom, 'NDR') AS geometry_wkb".to_string(),
    ];

    let query_attributes = source.content_query_attributes();
    let mut attributes = Vec::with_capacity(query_attributes.len());
    for (index, attribute) in query_attributes.iter().enumerate() {
        let attribute_column = quote_identifier(attribute, "attribute")?;
        select_columns.push(format!("t.{attribute_column}::text AS attr_{index}"));
        attributes.push(attribute.clone());
    }

    let table_geometry = format!("t.{geometry_column}");
    let clipped_geometry = clipped_geometry_expression(&table_geometry, "b.geom");
    let intersection_predicate =
        positive_area_intersection_predicate(&table_geometry, "b.geom", "clipped.geom");

    let sql = format!(
        "WITH tile_bbox AS (SELECT ST_MakeEnvelope($1, $2, $3, $4, $5) AS geom) \
         SELECT {} \
         FROM {schema}.{table} AS t \
         CROSS JOIN tile_bbox AS b \
         CROSS JOIN LATERAL ( \
           SELECT {clipped_geometry} AS geom \
         ) AS clipped \
         WHERE {intersection_predicate} \
         ORDER BY t.{id_column} \
         LIMIT $6",
        select_columns.join(", ")
    );

    Ok(TileWkbQueryPlan { sql, attributes })
}

fn build_subtree_occupancy_query(
    source: &SourceConfig,
) -> Result<SubtreeOccupancyQueryPlan, ConfigError> {
    let schema = quote_identifier(&source.schema, "schema")?;
    let table = quote_identifier(&source.table, "table")?;
    let geometry_column = quote_identifier(&source.geometry_column, "geometry_column")?;
    let table_geometry = format!("t.{geometry_column}");
    let clipped_geometry = clipped_geometry_expression(&table_geometry, "q.geom");
    let intersection_predicate =
        positive_area_intersection_predicate(&table_geometry, "q.geom", "clipped.geom");

    let sql = format!(
        "WITH requested_tiles AS ( \
           SELECT (u.ordinality - 1)::bigint AS slot, \
                  ST_MakeEnvelope(u.west, u.south, u.east, u.north, $5) AS geom \
           FROM unnest($1::float8[], $2::float8[], $3::float8[], $4::float8[]) \
                WITH ORDINALITY AS u(west, south, east, north, ordinality) \
         ) \
         SELECT q.slot, ( \
           SELECT count(*)::bigint \
           FROM ( \
             SELECT 1 \
             FROM {schema}.{table} AS t \
             CROSS JOIN LATERAL (SELECT {clipped_geometry} AS geom) AS clipped \
             WHERE {intersection_predicate} \
             LIMIT $6 \
           ) AS capped \
         ) AS feature_count \
         FROM requested_tiles AS q \
         ORDER BY q.slot"
    );

    Ok(SubtreeOccupancyQueryPlan { sql })
}

fn clipped_geometry_expression(table_geometry: &str, bbox_geometry: &str) -> String {
    format!("ST_Multi(ST_CollectionExtract(ST_Intersection({table_geometry}, {bbox_geometry}), 3))")
}

fn positive_area_intersection_predicate(
    table_geometry: &str,
    bbox_geometry: &str,
    clipped_geometry: &str,
) -> String {
    format!(
        "{table_geometry} && {bbox_geometry} \
         AND ST_Intersects({table_geometry}, {bbox_geometry}) \
         AND NOT ST_IsEmpty({clipped_geometry}) \
         AND ST_Area({clipped_geometry}) > 0"
    )
}

fn ensure_within_feature_limit(
    row_count: usize,
    max_features_per_tile: u32,
) -> Result<(), TileQueryError> {
    if row_count > max_features_per_tile as usize {
        return Err(TileQueryError::FeatureLimitExceeded {
            max_features_per_tile,
        });
    }

    Ok(())
}

fn validate_query_bbox(bbox: GeographicRegionDegrees) -> Result<(), ConfigError> {
    for (field, value) in [
        ("west", bbox.west),
        ("south", bbox.south),
        ("east", bbox.east),
        ("north", bbox.north),
    ] {
        if !value.is_finite() {
            return Err(ConfigError::Validation(format!(
                "tile bbox {field} must be finite"
            )));
        }
    }

    if bbox.west >= bbox.east {
        return Err(ConfigError::Validation(
            "tile bbox west must be less than east".to_string(),
        ));
    }

    if bbox.south >= bbox.north {
        return Err(ConfigError::Validation(
            "tile bbox south must be less than north".to_string(),
        ));
    }

    Ok(())
}

fn quote_identifier(value: &str, field: &str) -> Result<String, ConfigError> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(ConfigError::Validation(format!(
            "{field} must not be empty"
        )));
    };

    if !(first == '_' || first.is_ascii_alphabetic()) {
        return Err(ConfigError::Validation(format!(
            "{field} must start with an ASCII letter or underscore"
        )));
    }

    if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        return Err(ConfigError::Validation(format!(
            "{field} may only contain ASCII letters, numbers, and underscores"
        )));
    }

    Ok(format!("\"{value}\""))
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::path::Path;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tokio_postgres::NoTls;
    use tower::ServiceExt;

    use super::*;
    use lucy_core::source::SourceCatalog;
    use lucy_core::subtree::{
        generate_subtree_bytes_with_availability, pack_availability_bits, subtree_layout,
    };

    fn fixture_catalog() -> SourceCatalog {
        let config_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/poc-sources.yaml");
        let raw = std::fs::read_to_string(config_path).expect("fixture config should read");
        SourceCatalog::from_yaml_str(&raw).expect("fixture config should load")
    }

    fn fixture_source() -> SourceConfig {
        let mut catalog = fixture_catalog();
        catalog
            .sources
            .remove("poc_buildings")
            .expect("poc source should exist")
    }

    #[test]
    fn tile_wkb_query_uses_bound_bbox_values_clipping_and_limit() {
        let source = fixture_source();
        let plan = build_tile_wkb_query(&source).expect("query should build");

        assert!(plan.sql.contains("ST_MakeEnvelope($1, $2, $3, $4, $5)"));
        assert!(plan.sql.contains("t.\"geom\" && b.geom"));
        assert!(plan.sql.contains("ST_Intersects(t.\"geom\", b.geom)"));
        assert!(plan.sql.contains("ST_Intersection(t.\"geom\", b.geom)"));
        assert!(plan.sql.contains("ST_CollectionExtract"));
        assert!(plan.sql.contains("ST_Area(clipped.geom) > 0"));
        assert!(plan.sql.contains("ST_AsBinary(clipped.geom, 'NDR')"));
        assert!(plan.sql.contains("LIMIT $6"));
        assert!(!plan.sql.contains("-122.40130"));
        assert!(!plan.sql.contains("37.79245"));
        assert_eq!(
            plan.attributes,
            vec![
                "name",
                "building_type",
                "base_height_m",
                "height_m",
                "color"
            ]
        );
    }

    #[test]
    fn subtree_occupancy_query_batches_all_boxes_with_shared_clipping_semantics() {
        let source = fixture_source();
        let plan = build_subtree_occupancy_query(&source).expect("query should build");

        assert!(
            plan.sql
                .contains("unnest($1::float8[], $2::float8[], $3::float8[], $4::float8[])")
        );
        assert!(plan.sql.contains("WITH ORDINALITY"));
        assert!(plan.sql.contains("ST_Intersection(t.\"geom\", q.geom)"));
        assert!(plan.sql.contains("t.\"geom\" && q.geom"));
        assert!(plan.sql.contains("ST_Area(clipped.geom) > 0"));
        assert!(plan.sql.contains("LIMIT $6"));
        assert!(plan.sql.contains("ORDER BY q.slot"));
    }

    #[test]
    fn tile_wkb_query_rejects_unsafe_attribute_identifiers() {
        let mut source = fixture_source();
        source.attributes.push("name; DROP TABLE x".to_string());

        let error = build_tile_wkb_query(&source).expect_err("unsafe attribute should fail");
        assert!(
            error.to_string().contains("attribute"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn tile_wkb_query_includes_configured_height_columns() {
        let mut source = fixture_source();
        source.attributes = vec!["name".to_string()];
        source.base_height_column = Some("custom_base_m".to_string());
        source.height_column = "custom_height_m".to_string();

        let plan = build_tile_wkb_query(&source).expect("query should build");

        assert_eq!(
            plan.attributes,
            vec!["name", "color", "custom_base_m", "custom_height_m"]
        );
        assert!(plan.sql.contains("t.\"custom_base_m\"::text AS attr_2"));
        assert!(plan.sql.contains("t.\"custom_height_m\"::text AS attr_3"));
    }

    #[test]
    fn feature_limit_reports_overflow_instead_of_truncating() {
        ensure_within_feature_limit(2, 2).expect("limit itself should be accepted");

        let error = ensure_within_feature_limit(3, 2).expect_err("overflow should fail");
        assert!(matches!(
            error,
            TileQueryError::FeatureLimitExceeded {
                max_features_per_tile: 2
            }
        ));
        assert!(error.to_string().contains("instead of serving truncated"));
    }

    #[tokio::test]
    async fn fixture_tile_query_clips_cross_boundary_features_and_rejects_overflow() {
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

        let mut source = fixture_source();
        let root_features = query_tile_geometry_wkb(&client, &source, TileCoord::root())
            .await
            .expect("root tile should query");
        assert_eq!(root_features.len(), 6);
        assert!(
            root_features
                .iter()
                .all(|feature| !feature.geometry_wkb.is_empty())
        );
        assert_eq!(
            root_features[0]
                .attributes
                .get("name")
                .and_then(|value| value.as_deref()),
            Some("Sansome Office")
        );
        assert_eq!(
            root_features[0]
                .attributes
                .get("color")
                .and_then(|value| value.as_deref()),
            Some("#8aa1b1")
        );

        let availability = query_subtree_availability(&client, &source, TileCoord::root())
            .await
            .expect("root availability should query");
        assert_eq!(
            availability
                .tile
                .iter()
                .filter(|available| **available)
                .count(),
            60
        );
        assert_eq!(
            availability
                .content
                .iter()
                .filter(|available| **available)
                .count(),
            60
        );
        assert_eq!(
            availability
                .child_subtree
                .iter()
                .filter(|available| **available)
                .count(),
            121
        );
        assert_eq!(
            pack_availability_bits(&availability.tile),
            vec![
                0xff, 0x7f, 0xe6, 0xff, 0xff, 0xbf, 0xf9, 0x1f, 0xe0, 0x07, 0x00,
            ]
        );
        let first =
            generate_subtree_bytes_with_availability(&source, TileCoord::root(), &availability)
                .expect("sparse subtree should encode");
        let second =
            generate_subtree_bytes_with_availability(&source, TileCoord::root(), &availability)
                .expect("sparse subtree should encode deterministically");
        assert_eq!(first, second);

        let app = crate::server::build_app(fixture_catalog()).expect("fixture app should build");
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/sources/poc_buildings/subtrees/0/0/0.subtree")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("subtree request should route");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/octet-stream")
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("subtree body should read");
        assert_eq!(&body[0..4], b"subt");
        let json_length = u64::from_le_bytes(body[8..16].try_into().expect("JSON length")) as usize;
        let document: serde_json::Value =
            serde_json::from_slice(&body[24..24 + json_length]).expect("subtree JSON should parse");
        assert_eq!(document["tileAvailability"]["availableCount"], 60);
        assert_eq!(document["contentAvailability"][0]["availableCount"], 60);
        assert_eq!(document["childSubtreeAvailability"]["availableCount"], 121);

        let layout = subtree_layout(&source, TileCoord::root()).expect("root layout should build");
        let occupied_child_index = availability
            .child_subtree
            .iter()
            .position(|available| *available)
            .expect("fixture should have an occupied child subtree");
        let occupied_child = layout.child_roots[occupied_child_index]
            .expect("occupied child slot should have a coordinate");
        let scoped_path = format!(
            "/sources/poc_buildings/subtrees/{}/{}/{}.subtree",
            occupied_child.level, occupied_child.x, occupied_child.y
        );
        let scoped = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&scoped_path)
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("non-root subtree request should route");
        assert_eq!(scoped.status(), StatusCode::OK);
        assert_eq!(
            scoped
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/octet-stream")
        );
        let scoped_body = to_bytes(scoped.into_body(), usize::MAX)
            .await
            .expect("scoped subtree body should read");
        assert_eq!(&scoped_body[0..4], b"subt");

        let legacy_path = format!(
            "/subtrees/{}/{}/{}.subtree",
            occupied_child.level, occupied_child.x, occupied_child.y
        );
        let legacy = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&legacy_path)
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("legacy non-root subtree request should route");
        assert_eq!(legacy.status(), StatusCode::OK);
        let legacy_body = to_bytes(legacy.into_body(), usize::MAX)
            .await
            .expect("legacy subtree body should read");
        assert_eq!(legacy_body, scoped_body);

        let content = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/sources/poc_buildings/content/0/0/0.glb")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("content request should route");
        assert_eq!(content.status(), StatusCode::OK);
        assert_eq!(
            content
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("model/gltf-binary")
        );
        let content_body = to_bytes(content.into_body(), usize::MAX)
            .await
            .expect("content body should read");
        assert_eq!(&content_body[0..4], b"glTF");
        let content_json_length =
            u32::from_le_bytes(content_body[12..16].try_into().expect("JSON length")) as usize;
        let content_document: serde_json::Value =
            serde_json::from_slice(&content_body[20..20 + content_json_length])
                .expect("content glTF JSON should parse");
        assert_eq!(
            content_document["extensionsUsed"],
            serde_json::json!(["EXT_mesh_features", "EXT_structural_metadata"])
        );
        assert_eq!(
            content_document["meshes"][0]["primitives"][0]["attributes"]["COLOR_0"],
            2
        );
        assert_eq!(
            content_document["extensions"]["EXT_structural_metadata"]["propertyTables"][0]["count"],
            6
        );
        let color_accessor_index =
            content_document["meshes"][0]["primitives"][0]["attributes"]["COLOR_0"]
                .as_u64()
                .expect("color accessor") as usize;
        let color_accessor = &content_document["accessors"][color_accessor_index];
        let color_view_index = color_accessor["bufferView"].as_u64().expect("color view") as usize;
        let color_view = &content_document["bufferViews"][color_view_index];
        let binary_start = 20 + content_json_length + 8;
        let color_start =
            binary_start + color_view["byteOffset"].as_u64().expect("color offset") as usize;
        let first_color = content_body[color_start..color_start + 16]
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("f32 color")))
            .collect::<Vec<_>>();
        assert_eq!(
            first_color,
            vec![
                f32::from(0x8a_u8) / 255.0,
                f32::from(0xa1_u8) / 255.0,
                f32::from(0xb1_u8) / 255.0,
                1.0
            ]
        );

        let empty_child_index = availability
            .child_subtree
            .iter()
            .position(|available| !*available)
            .expect("fixture should have an empty child subtree");
        let empty_child = layout.child_roots[empty_child_index]
            .expect("empty child slot should have a coordinate");
        let empty_path = format!(
            "/sources/poc_buildings/subtrees/{}/{}/{}.subtree",
            empty_child.level, empty_child.x, empty_child.y
        );
        let empty = app
            .oneshot(
                Request::builder()
                    .uri(&empty_path)
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("empty subtree request should route");
        assert_eq!(empty.status(), StatusCode::NOT_FOUND);

        let empty_tile = TileCoord::new(2, 0, 3).expect("valid empty fixture tile");
        let empty_features = query_tile_geometry_wkb(&client, &source, empty_tile)
            .await
            .expect("empty tile should query");
        assert!(empty_features.is_empty());

        let southwest = query_tile_geometry_wkb(
            &client,
            &source,
            TileCoord::new(1, 0, 0).expect("southwest tile"),
        )
        .await
        .expect("southwest tile should query");
        let southeast = query_tile_geometry_wkb(
            &client,
            &source,
            TileCoord::new(1, 1, 0).expect("southeast tile"),
        )
        .await
        .expect("southeast tile should query");
        let west_fragment = southwest
            .iter()
            .find(|feature| feature.id == "2")
            .expect("cross-boundary feature should have a west fragment");
        let east_fragment = southeast
            .iter()
            .find(|feature| feature.id == "2")
            .expect("cross-boundary feature should have an east fragment");
        assert_ne!(west_fragment.geometry_wkb, east_fragment.geometry_wkb);

        source.max_features_per_tile = 2;
        let limited_availability = query_subtree_availability(&client, &source, TileCoord::root())
            .await
            .expect("non-terminal overflow should require subdivision");
        assert_eq!(
            limited_availability
                .tile
                .iter()
                .filter(|available| **available)
                .count(),
            60
        );
        assert_eq!(
            limited_availability
                .content
                .iter()
                .filter(|available| **available)
                .count(),
            56
        );
        assert!(!limited_availability.content[0]);
        assert_eq!(
            pack_availability_bits(&limited_availability.content),
            vec![
                0xf8, 0x7e, 0xe6, 0xff, 0xff, 0xbf, 0xf9, 0x1f, 0xe0, 0x07, 0x00,
            ]
        );

        let error = query_tile_geometry_wkb(&client, &source, TileCoord::root())
            .await
            .expect_err("overflow must not return a truncated tile");
        assert!(matches!(
            error,
            TileQueryError::FeatureLimitExceeded {
                max_features_per_tile: 2
            }
        ));

        source.max_level = 0;
        let error = query_subtree_availability(&client, &source, TileCoord::root())
            .await
            .expect_err("overflow at max_level must be terminal");
        assert!(matches!(
            error,
            TileQueryError::TerminalFeatureLimitExceeded {
                level: 0,
                x: 0,
                y: 0,
                max_features_per_tile: 2
            }
        ));
    }
}
