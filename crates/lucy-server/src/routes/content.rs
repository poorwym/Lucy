use std::collections::BTreeMap;
use std::time::Instant;

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use tracing::{debug, error};

use lucy_core::geometry::NormalizedGeometry;
use lucy_core::glb::{
    ContentFeature, GlbEncodingOptions, encode_feature_content_tile_glb_with_options,
};
use lucy_core::mesh::{
    MeshFrame, SurfaceTileClip, TriangleMesh, footprint_fragment_to_extruded_mesh,
    surface_geometry_z_to_tile_mesh,
};
use lucy_core::source::{ConfigError, DEFAULT_BASE_HEIGHT_M, SourceConfig, SourceModel};
use lucy_core::tile::TileCoord;

use crate::error::RouteError;
use crate::postgis::{
    NormalizedFeature, TileQueryError, for_each_normalized_surface_feature_prepared,
    normalized_surface_geometry_query_sql, query_normalized_features, surface_tile_clip,
};
use crate::response::bytes_response;
use crate::state::AppState;

use super::util::{ensure_configured_level, parse_tile_path, resolve_connection_string};

pub(crate) async fn source_content(
    State(state): State<AppState>,
    AxumPath((source_id, level, x, y_file)): AxumPath<(String, String, String, String)>,
) -> Result<Response, RouteError> {
    let source = state.source(&source_id)?;
    content_tile_response(
        &state,
        &source_id,
        &source,
        parse_tile_path(&level, &x, &y_file, ".glb")?,
    )
    .await
}

pub(crate) async fn default_content(
    State(state): State<AppState>,
    AxumPath((level, x, y_file)): AxumPath<(String, String, String)>,
) -> Result<Response, RouteError> {
    let source = state.default_source()?;
    content_tile_response(
        &state,
        state.default_source_id(),
        &source,
        parse_tile_path(&level, &x, &y_file, ".glb")?,
    )
    .await
}

#[tracing::instrument(
    name = "content_tile",
    skip(state, source),
    fields(tile.level = tile.level, tile.x = tile.x, tile.y = tile.y)
)]
async fn content_tile_response(
    state: &AppState,
    source_id: &str,
    source: &SourceConfig,
    tile: TileCoord,
) -> Result<Response, RouteError> {
    ensure_configured_level(source, tile)?;
    if tile.level < source.tileset.content_start_level {
        return Err(RouteError::not_found(format!(
            "tile level={} is below content_start_level={}",
            tile.level, source.tileset.content_start_level
        )));
    }
    let connection = resolve_connection_string(&source.connection)?;
    let source_frame = MeshFrame::from_source_bounds(&source.bounds);
    let (frame, node_transform) = content_mesh_placement(source, tile)?;
    let surface_clip = surface_tile_clip(source, tile)?;
    let mut client = state.postgres_client(source_id, &connection).await?;
    let started = Instant::now();
    let mut candidate_count = 0_usize;
    let mut wkb_bytes = 0_usize;
    let mut pending_features = Vec::new();
    match source.source_model {
        SourceModel::ExtrudedFootprint => {
            client.discard();
            let features = query_normalized_features(client.client(), source, tile).await?;
            client.reuse();
            drop(client);
            candidate_count = features.len();
            wkb_bytes = features
                .iter()
                .map(|feature| feature.encoded_size_bytes)
                .sum();
            pending_features.reserve(features.len());
            for feature in features {
                append_pending_content_feature(
                    &mut pending_features,
                    source_id,
                    source,
                    feature,
                    source_frame,
                    frame,
                    surface_clip,
                )?;
            }
        }
        SourceModel::SurfaceGeometryZ => {
            let statement_sql = normalized_surface_geometry_query_sql(source)?;
            let statement = client.prepare_cached(&statement_sql).await?;
            client.discard();
            let stream_result = for_each_normalized_surface_feature_prepared(
                client.client(),
                &statement,
                source,
                tile,
                |feature| {
                    candidate_count += 1;
                    wkb_bytes += feature.encoded_size_bytes;
                    append_pending_content_feature(
                        &mut pending_features,
                        source_id,
                        source,
                        feature,
                        source_frame,
                        frame,
                        surface_clip,
                    )
                },
            )
            .await;
            if let Err(error) = stream_result {
                // Do not reuse a socket that may still be draining the
                // uncapped streaming query after an early exit.
                client.discard();
                return Err(error);
            }
            client.reuse();
            drop(client);
        }
    }
    debug!(
        duration_ms = started.elapsed().as_secs_f64() * 1_000.0,
        feature_count = candidate_count,
        wkb_bytes,
        "tile geometry streamed and meshed"
    );
    if candidate_count == 0 {
        return Err(RouteError::not_found(format!(
            "tile level={} x={} y={} has no features",
            tile.level, tile.x, tile.y
        )));
    }
    if pending_features.is_empty() {
        return Err(RouteError::not_found(format!(
            "tile level={} x={} y={} has no surface fragments after clipping",
            tile.level, tile.x, tile.y
        )));
    }
    if source.source_model == SourceModel::SurfaceGeometryZ {
        pending_features.sort_by(|left, right| left.id.cmp(&right.id));
    }
    let content_features = finalize_content_features(source, pending_features)?;
    let vertex_count = content_features
        .iter()
        .map(|feature| feature.mesh.vertices.len())
        .sum::<usize>();
    let triangle_count = content_features
        .iter()
        .map(|feature| feature.mesh.indices.len() / 3)
        .sum::<usize>();
    debug!(
        duration_ms = started.elapsed().as_secs_f64() * 1_000.0,
        mesh_count = content_features.len(),
        vertex_count,
        triangle_count,
        "feature meshes generated"
    );

    let started = Instant::now();
    let glb = encode_feature_content_tile_glb_with_options(
        &content_features,
        node_transform,
        GlbEncodingOptions {
            compression: source.compression,
            quantization: source.quantization,
        },
    )?;
    debug!(
        duration_ms = started.elapsed().as_secs_f64() * 1_000.0,
        glb_bytes = glb.len(),
        "GLB encoded"
    );
    Ok(bytes_response(StatusCode::OK, "model/gltf-binary", glb))
}

struct PendingContentFeature {
    id: String,
    attributes: BTreeMap<String, Option<String>>,
    mesh: TriangleMesh,
    double_sided: bool,
}

#[cfg(test)]
fn build_content_features(
    source_id: &str,
    source: &SourceConfig,
    features: Vec<NormalizedFeature>,
    source_frame: MeshFrame,
    tile_frame: MeshFrame,
    surface_clip: SurfaceTileClip,
) -> Result<Vec<ContentFeature>, RouteError> {
    let mut pending_features = Vec::with_capacity(features.len());
    for feature in features {
        append_pending_content_feature(
            &mut pending_features,
            source_id,
            source,
            feature,
            source_frame,
            tile_frame,
            surface_clip,
        )?;
    }
    finalize_content_features(source, pending_features)
}

fn append_pending_content_feature(
    pending_features: &mut Vec<PendingContentFeature>,
    source_id: &str,
    source: &SourceConfig,
    feature: NormalizedFeature,
    source_frame: MeshFrame,
    tile_frame: MeshFrame,
    surface_clip: SurfaceTileClip,
) -> Result<(), RouteError> {
    let double_sided = matches!(&feature.geometry, NormalizedGeometry::GeodeticSurface(_));
    let mesh_result = match &feature.geometry {
        NormalizedGeometry::GeographicFootprint(fragment) => {
            let (base_height_m, height_m) = feature_heights(source, &feature)?;
            footprint_fragment_to_extruded_mesh(
                fragment,
                tile_frame,
                f64::from(base_height_m),
                f64::from(height_m),
            )
            .map(Some)
        }
        NormalizedGeometry::GeodeticSurface(geometry) => {
            surface_geometry_z_to_tile_mesh(geometry, source_frame, tile_frame, surface_clip)
        }
    };
    let mesh = match mesh_result {
        Ok(mesh) => mesh,
        Err(error) => {
            error!(
                source.id = %source_id,
                feature.id = %feature.id,
                error = %error,
                "skipping feature that could not be converted to a mesh"
            );
            return Ok(());
        }
    };
    let Some(mesh) = mesh else {
        debug!(
            feature.id,
            "broad-phase surface candidate missed tile after clipping"
        );
        return Ok(());
    };
    if pending_features.len() >= source.max_features_per_tile as usize {
        return Err(TileQueryError::FeatureLimitExceeded {
            max_features_per_tile: source.max_features_per_tile,
        }
        .into());
    }
    pending_features.push(PendingContentFeature {
        id: feature.id,
        attributes: feature.attributes,
        mesh,
        double_sided,
    });
    Ok(())
}

fn finalize_content_features(
    source: &SourceConfig,
    pending_features: Vec<PendingContentFeature>,
) -> Result<Vec<ContentFeature>, RouteError> {
    let mut content_features = Vec::with_capacity(pending_features.len());
    for pending in pending_features {
        let base_color = feature_base_color(source, &pending.id, &pending.attributes)?;
        let properties = source
            .attributes
            .iter()
            .map(|attribute| {
                (
                    attribute.clone(),
                    pending.attributes.get(attribute).cloned().unwrap_or(None),
                )
            })
            .collect::<BTreeMap<_, _>>();
        content_features.push(ContentFeature {
            id: pending.id,
            mesh: pending.mesh,
            base_color,
            double_sided: pending.double_sided,
            properties,
        });
    }
    Ok(content_features)
}

fn content_mesh_placement(
    source: &SourceConfig,
    tile: TileCoord,
) -> Result<(MeshFrame, [f64; 16]), ConfigError> {
    let source_frame = MeshFrame::from_source_bounds(&source.bounds);
    let tile_region = tile.geographic_region_degrees(&source.bounds)?;
    let tile_frame = MeshFrame::from_tile_region(tile_region);
    let node_transform = source_frame.gltf_node_transform_for(tile_frame);
    Ok((tile_frame, node_transform))
}

fn feature_base_color(
    source: &SourceConfig,
    feature_id: &str,
    attributes: &BTreeMap<String, Option<String>>,
) -> Result<[f32; 4], RouteError> {
    let Some(color_column) = source.material.color_column.as_deref() else {
        return Ok(source.material.default_base_color);
    };
    let Some(value) = attributes.get(color_column).and_then(Option::as_deref) else {
        return Ok(source.material.default_base_color);
    };

    parse_hex_color(value).map_err(|message| {
        RouteError::config(format!(
            "feature {} material color {color_column}={value:?} is invalid: {message}",
            feature_id
        ))
    })
}

fn parse_hex_color(value: &str) -> Result<[f32; 4], &'static str> {
    let value = value.trim();
    let hex = value
        .strip_prefix('#')
        .ok_or("expected #RRGGBB or #RRGGBBAA")?;
    if hex.len() != 6 && hex.len() != 8 {
        return Err("expected #RRGGBB or #RRGGBBAA");
    }

    let parse_channel = |offset: usize| {
        u8::from_str_radix(&hex[offset..offset + 2], 16)
            .map(|channel| f32::from(channel) / 255.0)
            .map_err(|_| "color contains a non-hexadecimal channel")
    };

    Ok([
        parse_channel(0)?,
        parse_channel(2)?,
        parse_channel(4)?,
        if hex.len() == 8 {
            parse_channel(6)?
        } else {
            1.0
        },
    ])
}

fn feature_heights(
    source: &SourceConfig,
    feature: &NormalizedFeature,
) -> Result<(f32, f32), RouteError> {
    let base_height_m = match source.base_height_column_or_default() {
        Some(attribute) => {
            parse_optional_feature_f32(feature, attribute)?.unwrap_or(DEFAULT_BASE_HEIGHT_M)
        }
        None => DEFAULT_BASE_HEIGHT_M,
    };
    let height_column = source
        .extrusion_height_column()
        .ok_or_else(|| RouteError::config("extruded_footprint source is missing height_column"))?;
    let height_m = parse_required_feature_f32(feature, height_column)?;
    Ok((base_height_m, height_m))
}

fn parse_required_feature_f32(
    feature: &NormalizedFeature,
    attribute: &str,
) -> Result<f32, RouteError> {
    parse_optional_feature_f32(feature, attribute)?.ok_or_else(|| {
        RouteError::config(format!(
            "feature {} is missing required attribute {attribute}",
            feature.id
        ))
    })
}

fn parse_optional_feature_f32(
    feature: &NormalizedFeature,
    attribute: &str,
) -> Result<Option<f32>, RouteError> {
    let Some(value) = feature
        .attributes
        .get(attribute)
        .and_then(|value| value.as_deref())
    else {
        return Ok(None);
    };

    value.parse::<f32>().map(Some).map_err(|error| {
        RouteError::config(format!(
            "feature {} attribute {attribute}={value:?} is not a valid f32: {error}",
            feature.id
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use lucy_core::geometry::{
        FootprintFragment, FootprintGeometry, MultiLineString2D, Point2D, Point3D, Polygon2D,
        Polygon3D, Ring2D, Ring3D, SurfaceGeometryZ,
    };
    use lucy_core::mesh::footprint_to_extruded_mesh;
    use lucy_core::source::{SourceCatalog, SourceModel};

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

    fn empty_footprint_geometry() -> NormalizedGeometry {
        NormalizedGeometry::GeographicFootprint(FootprintFragment {
            geometry: FootprintGeometry::MultiPolygon(Vec::new()),
            source_boundary: MultiLineString2D { lines: Vec::new() },
        })
    }

    fn surface_polygon(west: f64, south: f64, size: f64) -> Polygon3D {
        let point = |x, y| Point3D { x, y, z: 20.0 };
        Polygon3D {
            exterior: Ring3D {
                points: vec![
                    point(west, south),
                    point(west + size, south),
                    point(west + size, south + size),
                    point(west, south + size),
                    point(west, south),
                ],
            },
            interiors: Vec::new(),
        }
    }

    fn surface_feature(id: &str, geometry: SurfaceGeometryZ, color: &str) -> NormalizedFeature {
        NormalizedFeature {
            id: id.to_string(),
            geometry: NormalizedGeometry::GeodeticSurface(geometry),
            encoded_size_bytes: 0,
            attributes: BTreeMap::from([("color".to_string(), Some(color.to_string()))]),
        }
    }

    #[test]
    fn feature_heights_use_configured_column_names() {
        let mut source = fixture_source();
        source.base_height_column = Some("bottom_m".to_string());
        source.height_column = Some("height_delta_m".to_string());

        let feature = NormalizedFeature {
            id: "42".to_string(),
            geometry: empty_footprint_geometry(),
            encoded_size_bytes: 0,
            attributes: BTreeMap::from([
                ("bottom_m".to_string(), Some("7.5".to_string())),
                ("height_delta_m".to_string(), Some("12.25".to_string())),
            ]),
        };

        assert_eq!(
            feature_heights(&source, &feature).expect("heights should parse"),
            (7.5, 12.25)
        );
    }

    #[test]
    fn feature_heights_default_missing_base_height_to_zero() {
        let mut source = fixture_source();
        source.base_height_column = None;
        source.height_column = Some("height_delta_m".to_string());

        let feature = NormalizedFeature {
            id: "42".to_string(),
            geometry: empty_footprint_geometry(),
            encoded_size_bytes: 0,
            attributes: BTreeMap::from([("height_delta_m".to_string(), Some("12.25".to_string()))]),
        };

        assert_eq!(
            feature_heights(&source, &feature).expect("heights should parse"),
            (DEFAULT_BASE_HEIGHT_M, 12.25)
        );
    }

    #[test]
    fn feature_color_uses_configured_hex_and_optional_alpha() {
        let source = fixture_source();
        let feature = NormalizedFeature {
            id: "42".to_string(),
            geometry: empty_footprint_geometry(),
            encoded_size_bytes: 0,
            attributes: BTreeMap::from([("color".to_string(), Some("#80402080".to_string()))]),
        };

        let color = feature_base_color(&source, &feature.id, &feature.attributes)
            .expect("color should parse");
        assert_eq!(
            color,
            [128.0 / 255.0, 64.0 / 255.0, 32.0 / 255.0, 128.0 / 255.0]
        );
    }

    #[test]
    fn feature_color_falls_back_to_configured_default() {
        let source = fixture_source();
        let feature = NormalizedFeature {
            id: "42".to_string(),
            geometry: empty_footprint_geometry(),
            encoded_size_bytes: 0,
            attributes: BTreeMap::from([("color".to_string(), None)]),
        };

        assert_eq!(
            feature_base_color(&source, &feature.id, &feature.attributes)
                .expect("default should apply"),
            source.material.default_base_color
        );
    }

    #[test]
    fn feature_color_rejects_malformed_source_values() {
        let source = fixture_source();
        let feature = NormalizedFeature {
            id: "42".to_string(),
            geometry: empty_footprint_geometry(),
            encoded_size_bytes: 0,
            attributes: BTreeMap::from([("color".to_string(), Some("orange".to_string()))]),
        };

        assert!(feature_base_color(&source, &feature.id, &feature.attributes).is_err());
        assert_eq!(
            parse_hex_color("orange"),
            Err("expected #RRGGBB or #RRGGBBAA")
        );
    }

    #[test]
    fn surface_content_filters_broad_phase_misses_before_metadata_and_limits() {
        let mut source = fixture_source();
        source.source_model = SourceModel::SurfaceGeometryZ;
        source.base_height_column = None;
        source.height_column = None;
        source.bounds.west = 5.0;
        source.bounds.south = 50.0;
        source.bounds.east = 5.01;
        source.bounds.north = 50.01;
        source.bounds.min_height_m = 0.0;
        source.bounds.max_height_m = 100.0;
        source.max_features_per_tile = 1;
        let source_frame = MeshFrame::from_source_bounds(&source.bounds);
        let tile_frame = MeshFrame::from_geodetic_origin(5.005, 50.005, 0.0);
        let clip = SurfaceTileClip {
            west_deg: 5.004,
            south_deg: 50.004,
            east_deg: 5.006,
            north_deg: 50.006,
            include_east: false,
            include_north: false,
        };
        let broad_phase_miss = surface_feature(
            "miss",
            SurfaceGeometryZ::MultiPolygon(vec![
                surface_polygon(5.001, 50.001, 0.001),
                surface_polygon(5.008, 50.008, 0.001),
            ]),
            "invalid-color-that-must-not-be-read",
        );
        let hit = surface_feature(
            "hit",
            SurfaceGeometryZ::Polygon(surface_polygon(5.0045, 50.0045, 0.001)),
            "#8aa1b1",
        );

        let features = build_content_features(
            "surfaces",
            &source,
            vec![broad_phase_miss, hit.clone()],
            source_frame,
            tile_frame,
            clip,
        )
        .expect("only the emitted surface should count");
        assert_eq!(features.len(), 1);
        assert_eq!(features[0].id, "hit");

        let overflow = surface_feature(
            "overflow",
            SurfaceGeometryZ::Polygon(surface_polygon(5.0046, 50.0046, 0.001)),
            "invalid-color-that-must-not-mask-overflow",
        );
        let error = build_content_features(
            "surfaces",
            &source,
            vec![hit.clone(), overflow.clone()],
            source_frame,
            tile_frame,
            clip,
        )
        .expect_err("two emitted surfaces should exceed the configured limit");
        assert!(format!("{error:?}").contains("tile contains more than 1 features"));

        let error = build_content_features(
            "surfaces",
            &source,
            vec![overflow, hit],
            source_frame,
            tile_frame,
            clip,
        )
        .expect_err("overflow must win even when invalid metadata arrives first");
        assert!(format!("{error:?}").contains("tile contains more than 1 features"));
    }

    #[test]
    fn surface_content_skips_geometry_errors_and_keeps_the_rest_of_the_tile() {
        let mut source = fixture_source();
        source.source_model = SourceModel::SurfaceGeometryZ;
        source.base_height_column = None;
        source.height_column = None;
        source.bounds.west = 5.0;
        source.bounds.south = 50.0;
        source.bounds.east = 5.01;
        source.bounds.north = 50.01;
        source.bounds.min_height_m = 0.0;
        source.bounds.max_height_m = 100.0;
        source.max_features_per_tile = 1;
        let source_frame = MeshFrame::from_source_bounds(&source.bounds);
        let tile_frame = MeshFrame::from_geodetic_origin(5.005, 50.005, 0.0);
        let clip = SurfaceTileClip {
            west_deg: 5.0,
            south_deg: 50.0,
            east_deg: 5.01,
            north_deg: 50.01,
            include_east: true,
            include_north: true,
        };
        let non_planar = surface_feature(
            "10216096",
            SurfaceGeometryZ::Polygon(Polygon3D {
                exterior: Ring3D {
                    points: vec![
                        Point3D {
                            x: 5.002,
                            y: 50.002,
                            z: 10.0,
                        },
                        Point3D {
                            x: 5.006,
                            y: 50.002,
                            z: 10.0,
                        },
                        Point3D {
                            x: 5.006,
                            y: 50.006,
                            z: 10.0,
                        },
                        Point3D {
                            x: 5.002,
                            y: 50.006,
                            z: 30.0,
                        },
                        Point3D {
                            x: 5.002,
                            y: 50.002,
                            z: 10.0,
                        },
                    ],
                },
                interiors: Vec::new(),
            }),
            "#ff0000",
        );
        let valid = surface_feature(
            "valid",
            SurfaceGeometryZ::Polygon(surface_polygon(5.004, 50.004, 0.001)),
            "#8aa1b1",
        );

        let features = build_content_features(
            "surfaces",
            &source,
            vec![non_planar, valid],
            source_frame,
            tile_frame,
            clip,
        )
        .expect("a geometry error in one feature must not fail the tile");

        assert_eq!(features.len(), 1);
        assert_eq!(features[0].id, "valid");
    }

    #[test]
    fn surface_tile_clip_uses_half_open_internal_boundaries() {
        let mut source = fixture_source();
        source.bounds.west = 5.0;
        source.bounds.south = 50.0;
        source.bounds.east = 5.01;
        source.bounds.north = 50.01;

        let southwest =
            surface_tile_clip(&source, TileCoord::new(1, 0, 0).expect("southwest tile"))
                .expect("southwest clip");
        assert!(!southwest.include_east);
        assert!(!southwest.include_north);

        let northeast =
            surface_tile_clip(&source, TileCoord::new(1, 1, 1).expect("northeast tile"))
                .expect("northeast clip");
        assert!(northeast.include_east);
        assert!(northeast.include_north);
    }

    #[test]
    fn root_content_placement_is_relative_identity() {
        let source = fixture_source();
        let (frame, node_transform) = content_mesh_placement(&source, TileCoord::root())
            .expect("root placement should build");
        let source_frame = MeshFrame::from_source_bounds(&source.bounds);
        assert!((frame.origin_longitude_deg - source_frame.origin_longitude_deg).abs() < 1.0e-12);
        assert!((frame.origin_latitude_deg - source_frame.origin_latitude_deg).abs() < 1.0e-12);

        let identity = [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ];
        for (component, (actual, expected)) in node_transform.into_iter().zip(identity).enumerate()
        {
            assert!(
                (actual - expected).abs() < 1.0e-8,
                "matrix component {component}: actual={actual} expected={expected}"
            );
        }
    }

    #[test]
    fn content_frames_keep_footprints_local_and_oriented_near_longitude_axes() {
        let cases = [
            ("longitude 0", -1.0, 1.0, 128),
            ("longitude 90", 89.0, 91.0, 128),
            ("longitude 180", 178.0, 180.0, 255),
        ];

        for (label, west, east, tile_x) in cases {
            let mut source = fixture_source();
            source.bounds.west = west;
            source.bounds.east = east;
            source.bounds.south = 9.0;
            source.bounds.north = 11.0;
            source.bounds.min_height_m = 0.0;
            source.bounds.max_height_m = 100.0;
            let tile = TileCoord::new(8, tile_x, 128).expect("valid tile");
            let tile_region = tile
                .geographic_region_degrees(&source.bounds)
                .expect("tile region");
            let (frame, _) =
                content_mesh_placement(&source, tile).expect("content placement should build");

            assert!(
                (frame.origin_longitude_deg - (tile_region.west + tile_region.east) / 2.0).abs()
                    < 1.0e-12,
                "{label}: frame must use the requested tile centre"
            );
            let lon = frame.origin_longitude_deg;
            let lat = frame.origin_latitude_deg;
            let delta = 0.000_1;
            let geometry = FootprintGeometry::Polygon(Polygon2D {
                exterior: Ring2D {
                    points: vec![
                        Point2D {
                            x: lon - delta,
                            y: lat - delta,
                        },
                        Point2D {
                            x: lon + delta,
                            y: lat - delta,
                        },
                        Point2D {
                            x: lon + delta,
                            y: lat + delta,
                        },
                        Point2D {
                            x: lon - delta,
                            y: lat + delta,
                        },
                        Point2D {
                            x: lon - delta,
                            y: lat - delta,
                        },
                    ],
                },
                interiors: Vec::new(),
            });
            let mesh = footprint_to_extruded_mesh(&geometry, frame, 0.0, 10.0)
                .expect("local footprint should mesh");

            assert!(
                mesh.vertices.iter().all(|vertex| {
                    vertex.position[0].abs() < 20.0 && vertex.position[1].abs() < 20.0
                }),
                "{label}: tile-local positions should remain near zero"
            );
            let southwest = mesh.vertices[0].position;
            let southeast = mesh.vertices[1].position;
            let northeast = mesh.vertices[2].position;
            assert!(
                southeast[0] - southwest[0] > 10.0,
                "{label}: increasing longitude must point east"
            );
            assert!(
                (southeast[1] - southwest[1]).abs() < 0.1,
                "{label}: an east edge must not rotate into north"
            );
            assert!(
                northeast[1] - southeast[1] > 10.0,
                "{label}: increasing latitude must point north"
            );
            assert!(
                mesh.vertices[0..4]
                    .iter()
                    .all(|vertex| vertex.normal[2] > 0.999),
                "{label}: top normals must remain tile-local up"
            );
            assert!(mesh.vertices.iter().all(|vertex| {
                let length = vertex
                    .normal
                    .iter()
                    .map(|component| component * component)
                    .sum::<f32>()
                    .sqrt();
                (length - 1.0).abs() < 1.0e-5
            }));
        }
    }
}
