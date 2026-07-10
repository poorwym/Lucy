use std::collections::BTreeMap;
use std::fmt;

use tokio_postgres::GenericClient;

use lucy_core::source::{ConfigError, SourceConfig};
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

#[derive(Debug)]
pub enum TileQueryError {
    Config(ConfigError),
    FeatureLimitExceeded { max_features_per_tile: u32 },
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

    let sql = format!(
        "WITH tile_bbox AS (SELECT ST_MakeEnvelope($1, $2, $3, $4, $5) AS geom) \
         SELECT {} \
         FROM {schema}.{table} AS t \
         CROSS JOIN tile_bbox AS b \
         CROSS JOIN LATERAL ( \
           SELECT ST_Multi(ST_CollectionExtract(ST_Intersection(t.{geometry_column}, b.geom), 3)) AS geom \
         ) AS clipped \
         WHERE t.{geometry_column} && b.geom \
         AND ST_Intersects(t.{geometry_column}, b.geom) \
         AND NOT ST_IsEmpty(clipped.geom) \
         AND ST_Area(clipped.geom) > 0 \
         ORDER BY t.{id_column} \
         LIMIT $6",
        select_columns.join(", ")
    );

    Ok(TileWkbQueryPlan { sql, attributes })
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

    use tokio_postgres::NoTls;

    use super::*;
    use lucy_core::source::SourceCatalog;

    fn fixture_source() -> SourceConfig {
        let config_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/poc-sources.yaml");
        let raw = std::fs::read_to_string(config_path).expect("fixture config should read");
        let mut catalog = SourceCatalog::from_yaml_str(&raw).expect("fixture config should load");
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
            vec!["name", "building_type", "base_height_m", "height_m"]
        );
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
            vec!["name", "custom_base_m", "custom_height_m"]
        );
        assert!(plan.sql.contains("t.\"custom_base_m\"::text AS attr_1"));
        assert!(plan.sql.contains("t.\"custom_height_m\"::text AS attr_2"));
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
        let error = query_tile_geometry_wkb(&client, &source, TileCoord::root())
            .await
            .expect_err("overflow must not return a truncated tile");
        assert!(matches!(
            error,
            TileQueryError::FeatureLimitExceeded {
                max_features_per_tile: 2
            }
        ));
    }
}
