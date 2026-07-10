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
