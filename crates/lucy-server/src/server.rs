use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;

use tokio_postgres::NoTls;

use lucy_core::glb::{GlbError, encode_content_tile_glb};
use lucy_core::mesh::{MeshError, MeshFrame, wkb_footprint_to_extruded_mesh};
use lucy_core::subtree::generate_root_subtree_bytes;
use lucy_core::tile::{TileCoord, TileCoordError};
use lucy_core::tileset::{TilesetOptions, generate_tileset_json};
use lucy_core::{ConfigError, SourceCatalog, SourceConfig};

use crate::ConfigLoadError;
use crate::postgis::{TileFeatureWkb, TileQueryError, query_tile_geometry_wkb};

pub const DEFAULT_POC_ADDR: &str = "127.0.0.1:8080";
const PHASE_0_REPORT: &str = include_str!("../../../docs/phase-0-report.md");

pub fn run_poc_server(
    config_path: impl AsRef<Path>,
    addr: SocketAddr,
) -> Result<(), PocServerError> {
    let config_path = config_path.as_ref().to_path_buf();
    let catalog = crate::load_source_catalog(&config_path)?;
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| PocServerError::Runtime(format!("failed to create runtime: {error}")))?;
    let listener = TcpListener::bind(addr)?;

    eprintln!("Lucy Phase 0 POC server listening on http://{addr}/tileset.json");
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(error) = handle_stream(&mut stream, &catalog, &runtime) {
                    eprintln!("request failed: {error}");
                }
            }
            Err(error) => eprintln!("failed to accept connection: {error}"),
        }
    }

    Ok(())
}

fn handle_stream(
    stream: &mut TcpStream,
    catalog: &SourceCatalog,
    runtime: &tokio::runtime::Runtime,
) -> Result<(), PocServerError> {
    let mut buffer = [0_u8; 4096];
    let bytes_read = stream.read(&mut buffer)?;
    if bytes_read == 0 {
        return Ok(());
    }

    let request = std::str::from_utf8(&buffer[..bytes_read])
        .map_err(|error| PocServerError::Request(format!("request is not UTF-8: {error}")))?;
    let response = handle_poc_http_request(request, catalog, runtime);
    stream.write_all(&response.to_http_bytes())?;
    stream.flush()?;
    Ok(())
}

fn handle_poc_http_request(
    request: &str,
    catalog: &SourceCatalog,
    runtime: &tokio::runtime::Runtime,
) -> PocHttpResponse {
    let Some(request_line) = request.lines().next() else {
        return PocHttpResponse::plain(400, "Bad Request", "missing request line");
    };
    let mut parts = request_line.split_whitespace();
    let Some(method) = parts.next() else {
        return PocHttpResponse::plain(400, "Bad Request", "missing method");
    };
    let Some(target) = parts.next() else {
        return PocHttpResponse::plain(400, "Bad Request", "missing path");
    };

    if method != "GET" && method != "HEAD" {
        return PocHttpResponse::plain(
            405,
            "Method Not Allowed",
            "only GET and HEAD are supported",
        );
    }

    let path = target.split('?').next().unwrap_or(target);
    let response = match route_poc_path(path, catalog, runtime) {
        Ok(response) => response,
        Err(error) => error.to_response(),
    };

    if method == "HEAD" {
        response.without_body()
    } else {
        response
    }
}

fn route_poc_path(
    path: &str,
    catalog: &SourceCatalog,
    runtime: &tokio::runtime::Runtime,
) -> Result<PocHttpResponse, PocRouteError> {
    let (_source_id, source) =
        catalog
            .sources
            .iter()
            .next()
            .ok_or(PocRouteError::Config(ConfigError::Validation(
                "at least one source must be configured".to_string(),
            )))?;

    match path {
        "/" => Ok(PocHttpResponse::plain(
            200,
            "OK",
            "Lucy tile server is running. Use /tileset.json, /subtrees/0/0/0.subtree, or /content/{level}/{x}/{y}.glb.",
        )),
        "/tileset.json" => {
            let json = generate_tileset_json(source, &TilesetOptions::default())?;
            Ok(PocHttpResponse::new(
                200,
                "OK",
                "application/json",
                json.into_bytes(),
            ))
        }
        "/subtrees/0/0/0.subtree" => Ok(PocHttpResponse::new(
            200,
            "OK",
            "application/octet-stream",
            generate_root_subtree_bytes(source)?,
        )),
        "/phase-0-report.md" => Ok(PocHttpResponse::new(
            200,
            "OK",
            "text/markdown; charset=utf-8",
            PHASE_0_REPORT.as_bytes().to_vec(),
        )),
        _ => {
            if let Some(tile) = parse_content_tile_path(path)? {
                return runtime.block_on(content_tile_response(source, tile));
            }

            if path.starts_with("/subtrees/") {
                return Err(PocRouteError::NotFound(
                    "Phase 0 only serves the root subtree at /subtrees/0/0/0.subtree".to_string(),
                ));
            }

            Err(PocRouteError::NotFound(format!("unknown route {path}")))
        }
    }
}

async fn content_tile_response(
    source: &SourceConfig,
    tile: TileCoord,
) -> Result<PocHttpResponse, PocRouteError> {
    let connection = resolve_connection_string(&source.connection)?;
    let (client, connection_task) = tokio_postgres::connect(&connection, NoTls).await?;
    tokio::spawn(async move {
        if let Err(error) = connection_task.await {
            eprintln!("PostGIS connection error: {error}");
        }
    });

    let features = query_tile_geometry_wkb(&client, source, tile).await?;
    if features.is_empty() {
        return Err(PocRouteError::NotFound(format!(
            "tile level={} x={} y={} has no fixture features",
            tile.level, tile.x, tile.y
        )));
    }

    let frame = MeshFrame::from_source_bounds(&source.bounds);
    let mut meshes = Vec::with_capacity(features.len());
    for feature in features {
        let (base_height_m, height_m) = feature_heights(&feature)?;
        meshes.push(wkb_footprint_to_extruded_mesh(
            &feature.geometry_wkb,
            frame,
            base_height_m,
            height_m,
        )?);
    }

    Ok(PocHttpResponse::new(
        200,
        "OK",
        "model/gltf-binary",
        encode_content_tile_glb(&meshes)?,
    ))
}

fn feature_heights(feature: &TileFeatureWkb) -> Result<(f32, f32), PocRouteError> {
    let base_height_m = parse_optional_feature_f32(feature, "base_height_m")?.unwrap_or(0.0);
    let height_m = parse_required_feature_f32(feature, "height_m")?;
    Ok((base_height_m, height_m))
}

fn parse_required_feature_f32(
    feature: &TileFeatureWkb,
    attribute: &str,
) -> Result<f32, PocRouteError> {
    parse_optional_feature_f32(feature, attribute)?.ok_or_else(|| {
        PocRouteError::Config(ConfigError::Validation(format!(
            "feature {} is missing required attribute {attribute}",
            feature.id
        )))
    })
}

fn parse_optional_feature_f32(
    feature: &TileFeatureWkb,
    attribute: &str,
) -> Result<Option<f32>, PocRouteError> {
    let Some(value) = feature
        .attributes
        .get(attribute)
        .and_then(|value| value.as_deref())
    else {
        return Ok(None);
    };

    value.parse::<f32>().map(Some).map_err(|error| {
        PocRouteError::Config(ConfigError::Validation(format!(
            "feature {} attribute {attribute}={value:?} is not a valid f32: {error}",
            feature.id
        )))
    })
}

fn resolve_connection_string(connection: &str) -> Result<String, PocRouteError> {
    let trimmed = connection.trim();
    if trimmed == "${DATABASE_URL}" {
        std::env::var("DATABASE_URL").map_err(|error| {
            PocRouteError::Config(ConfigError::Validation(format!(
                "DATABASE_URL is required by source connection: {error}"
            )))
        })
    } else {
        Ok(trimmed.to_string())
    }
}

fn parse_content_tile_path(path: &str) -> Result<Option<TileCoord>, PocRouteError> {
    let Some(rest) = path.strip_prefix("/content/") else {
        return Ok(None);
    };
    let parts = rest.split('/').collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(PocRouteError::BadRequest(
            "content route must be /content/{level}/{x}/{y}.glb".to_string(),
        ));
    }

    let level = parse_u8(parts[0], "level")?;
    let x = parse_u32(parts[1], "x")?;
    let y_text = parts[2]
        .strip_suffix(".glb")
        .ok_or_else(|| PocRouteError::BadRequest("content route must end in .glb".to_string()))?;
    let y = parse_u32(y_text, "y")?;

    TileCoord::new(level, x, y)
        .map(Some)
        .map_err(PocRouteError::TileCoord)
}

fn parse_u8(value: &str, field: &str) -> Result<u8, PocRouteError> {
    value.parse::<u8>().map_err(|error| {
        PocRouteError::BadRequest(format!("{field} must be an unsigned integer: {error}"))
    })
}

fn parse_u32(value: &str, field: &str) -> Result<u32, PocRouteError> {
    value.parse::<u32>().map_err(|error| {
        PocRouteError::BadRequest(format!("{field} must be an unsigned integer: {error}"))
    })
}

#[derive(Debug)]
pub enum PocServerError {
    Config(ConfigError),
    ConfigLoad(ConfigLoadError),
    Io(std::io::Error),
    Request(String),
    Runtime(String),
}

impl fmt::Display for PocServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PocServerError::Config(error) => write!(f, "{error}"),
            PocServerError::ConfigLoad(error) => write!(f, "{error}"),
            PocServerError::Io(error) => write!(f, "{error}"),
            PocServerError::Request(message) => write!(f, "invalid HTTP request: {message}"),
            PocServerError::Runtime(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for PocServerError {}

impl From<ConfigError> for PocServerError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<ConfigLoadError> for PocServerError {
    fn from(error: ConfigLoadError) -> Self {
        Self::ConfigLoad(error)
    }
}

impl From<std::io::Error> for PocServerError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug)]
enum PocRouteError {
    BadRequest(String),
    Config(ConfigError),
    Glb(GlbError),
    Mesh(MeshError),
    NotFound(String),
    Postgres(TileQueryError),
    TileCoord(TileCoordError),
}

impl PocRouteError {
    fn to_response(&self) -> PocHttpResponse {
        match self {
            PocRouteError::BadRequest(message) => {
                PocHttpResponse::plain(400, "Bad Request", message)
            }
            PocRouteError::NotFound(message) => PocHttpResponse::plain(404, "Not Found", message),
            PocRouteError::Config(error) => {
                PocHttpResponse::plain(500, "Internal Server Error", &error.to_string())
            }
            PocRouteError::Glb(error) => {
                PocHttpResponse::plain(500, "Internal Server Error", &error.to_string())
            }
            PocRouteError::Mesh(error) => {
                PocHttpResponse::plain(500, "Internal Server Error", &error.to_string())
            }
            PocRouteError::Postgres(error) => {
                PocHttpResponse::plain(500, "Internal Server Error", &error.to_string())
            }
            PocRouteError::TileCoord(error) => {
                PocHttpResponse::plain(400, "Bad Request", &error.to_string())
            }
        }
    }
}

impl From<ConfigError> for PocRouteError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<GlbError> for PocRouteError {
    fn from(error: GlbError) -> Self {
        Self::Glb(error)
    }
}

impl From<MeshError> for PocRouteError {
    fn from(error: MeshError) -> Self {
        Self::Mesh(error)
    }
}

impl From<TileQueryError> for PocRouteError {
    fn from(error: TileQueryError) -> Self {
        Self::Postgres(error)
    }
}

impl From<tokio_postgres::Error> for PocRouteError {
    fn from(error: tokio_postgres::Error) -> Self {
        Self::Postgres(TileQueryError::from(error))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PocHttpResponse {
    status_code: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    extra_headers: Vec<(&'static str, String)>,
}

impl PocHttpResponse {
    fn new(
        status_code: u16,
        reason: &'static str,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> Self {
        Self {
            status_code,
            reason,
            content_type,
            body,
            extra_headers: Vec::new(),
        }
    }

    fn plain(status_code: u16, reason: &'static str, body: &str) -> Self {
        Self::new(
            status_code,
            reason,
            "text/plain; charset=utf-8",
            body.as_bytes().to_vec(),
        )
    }

    fn without_body(mut self) -> Self {
        self.body.clear();
        self
    }

    fn to_http_bytes(&self) -> Vec<u8> {
        let mut headers = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n",
            self.status_code,
            self.reason,
            self.content_type,
            self.body.len()
        );
        for (name, value) in &self.extra_headers {
            headers.push_str(name);
            headers.push_str(": ");
            headers.push_str(value);
            headers.push_str("\r\n");
        }
        headers.push_str("\r\n");

        let mut bytes = headers.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn fixture_catalog() -> SourceCatalog {
        let config_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/poc-sources.yaml");
        crate::load_source_catalog(config_path).expect("fixture config should load")
    }

    #[test]
    fn routes_tileset_subtree_report_and_root_status() {
        let catalog = fixture_catalog();
        let runtime = tokio::runtime::Runtime::new().expect("runtime");

        let root = handle_poc_http_request("GET / HTTP/1.1\r\n\r\n", &catalog, &runtime);
        assert_eq!(root.status_code, 200);
        assert!(String::from_utf8_lossy(&root.body).contains("Lucy tile server"));

        let tileset =
            handle_poc_http_request("GET /tileset.json HTTP/1.1\r\n\r\n", &catalog, &runtime);
        assert_eq!(tileset.status_code, 200);
        assert_eq!(tileset.content_type, "application/json");
        assert!(String::from_utf8_lossy(&tileset.body).contains("\"implicitTiling\""));

        let subtree = handle_poc_http_request(
            "GET /subtrees/0/0/0.subtree HTTP/1.1\r\n\r\n",
            &catalog,
            &runtime,
        );
        assert_eq!(subtree.status_code, 200);
        assert_eq!(&subtree.body[0..4], b"subt");

        let report = handle_poc_http_request(
            "GET /phase-0-report.md HTTP/1.1\r\n\r\n",
            &catalog,
            &runtime,
        );
        assert_eq!(report.status_code, 200);
        assert!(String::from_utf8_lossy(&report.body).contains("Phase 0"));
    }

    #[test]
    fn route_rejects_bad_content_paths() {
        let catalog = fixture_catalog();
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let response = handle_poc_http_request(
            "GET /content/nope/0/0.glb HTTP/1.1\r\n\r\n",
            &catalog,
            &runtime,
        );

        assert_eq!(response.status_code, 400);
    }
}
