use lucy_core::source::SourceConfig;
use lucy_core::tile::TileCoord;

use crate::error::RouteError;

pub(crate) fn resolve_connection_string(connection: &str) -> Result<String, RouteError> {
    let trimmed = connection.trim();
    if trimmed == "${DATABASE_URL}" {
        std::env::var("DATABASE_URL").map_err(|error| {
            RouteError::config(format!(
                "DATABASE_URL is required by source connection: {error}"
            ))
        })
    } else {
        Ok(trimmed.to_string())
    }
}

pub(crate) fn ensure_configured_level(
    source: &SourceConfig,
    tile: TileCoord,
) -> Result<(), RouteError> {
    if tile.level < source.min_level || tile.level > source.max_level {
        return Err(RouteError::not_found(format!(
            "tile level {} is outside configured levels {}..={}",
            tile.level, source.min_level, source.max_level
        )));
    }

    Ok(())
}

pub(crate) fn ensure_subtree_root(
    source: &SourceConfig,
    tile: TileCoord,
) -> Result<(), RouteError> {
    if !tile.level.is_multiple_of(source.subtree_levels) {
        return Err(RouteError::bad_request(format!(
            "level {} is not a subtree root; expected a multiple of subtree_levels {}",
            tile.level, source.subtree_levels
        )));
    }

    Ok(())
}

pub(crate) fn parse_tile_path(
    level: &str,
    x: &str,
    y_file: &str,
    suffix: &'static str,
) -> Result<TileCoord, RouteError> {
    let y = y_file
        .strip_suffix(suffix)
        .ok_or_else(|| RouteError::bad_request(format!("tile path must end in {suffix}")))?;

    TileCoord::new(
        parse_u8(level, "level")?,
        parse_u32(x, "x")?,
        parse_u32(y, "y")?,
    )
    .map_err(RouteError::from)
}

fn parse_u8(value: &str, field: &str) -> Result<u8, RouteError> {
    value.parse::<u8>().map_err(|error| {
        RouteError::bad_request(format!("{field} must be an unsigned integer: {error}"))
    })
}

fn parse_u32(value: &str, field: &str) -> Result<u32, RouteError> {
    value.parse::<u32>().map_err(|error| {
        RouteError::bad_request(format!("{field} must be an unsigned integer: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use axum::response::IntoResponse;
    use lucy_core::source::SourceCatalog;

    use super::*;

    fn fixture_source() -> SourceConfig {
        let mut catalog =
            SourceCatalog::from_yaml_str(include_str!("../../../../config/poc-sources.yaml"))
                .expect("fixture config should load");
        catalog
            .sources
            .remove("poc_buildings")
            .expect("poc source should exist")
    }

    #[test]
    fn configured_level_guard_accepts_bounds_and_rejects_outside_them() {
        let source = fixture_source();
        ensure_configured_level(&source, TileCoord::root()).expect("min level should pass");
        ensure_configured_level(
            &source,
            TileCoord::new(source.max_level, 0, 0).expect("max-level coordinate"),
        )
        .expect("max level should pass");
        ensure_subtree_root(&source, TileCoord::root()).expect("root should pass");
        ensure_subtree_root(
            &source,
            TileCoord::new(source.subtree_levels, 0, 0).expect("subtree coordinate"),
        )
        .expect("configured subtree boundary should pass");

        let above = TileCoord::new(source.max_level + 1, 0, 0).expect("coordinate should parse");
        let error = ensure_configured_level(&source, above).expect_err("level should be rejected");
        assert_eq!(
            error.into_response().status(),
            axum::http::StatusCode::NOT_FOUND
        );

        let non_root_level = TileCoord::new(1, 0, 0).expect("coordinate should parse");
        let error =
            ensure_subtree_root(&source, non_root_level).expect_err("level should be rejected");
        assert_eq!(
            error.into_response().status(),
            axum::http::StatusCode::BAD_REQUEST
        );

        let mut nonzero_min = source;
        nonzero_min.min_level = 2;
        let below = TileCoord::new(1, 0, 0).expect("coordinate should parse");
        let error =
            ensure_configured_level(&nonzero_min, below).expect_err("level should be rejected");
        assert_eq!(
            error.into_response().status(),
            axum::http::StatusCode::NOT_FOUND
        );
    }
}
