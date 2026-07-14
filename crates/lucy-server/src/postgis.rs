use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use tokio_postgres::types::ToSql;
use tokio_postgres::{GenericClient, Row, Statement};

use lucy_core::geometry::{
    FootprintFragment, NormalizedGeometry, decode_footprint_wkb, decode_multi_line_string_wkb,
    decode_surface_geometry_z_wkb,
};
use lucy_core::mesh::{MeshFrame, SurfaceTileClip, prepare_surface_geometry_z};
use lucy_core::source::{
    ConfigError, CoordinateOperation, GeometryType, SourceBounds, SourceConfig, SourceModel,
};
use lucy_core::subtree::{SubtreeAvailabilityBits, SubtreeLayout, subtree_layout};
use lucy_core::tile::{GeographicRegionDegrees, TileCoord};

const TARGET_GEOGRAPHIC_2D_SRID: i32 = 4326;
const TARGET_GEODETIC_3D_SRID: i32 = 4979;
const POSTGIS_AUTOMATIC_TRANSFORM_OPERATION: &str = "postgis_st_transform";
const RDNAPTRANS2018_EPSG_1149_OPERATION: &str = "rdnaptrans2018_epsg_1149";
const RDNAP_CONTENT_TRANSFORM_BATCH_SIZE: usize = 64;
const RDNAPTRANS2018_EPSG_1149_PIPELINE: &str = "+proj=pipeline \
  +step +inv +proj=sterea +lat_0=52.1561605555556 +lon_0=5.38763888888889 \
        +k=0.9999079 +x_0=155000 +y_0=463000 +ellps=bessel \
  +step +proj=hgridshift +grids=nl_nsgi_rdtrans2018.tif \
  +step +proj=vgridshift +grids=nl_nsgi_nlgeo2018.tif +multiplier=1 \
  +step +proj=cart +ellps=GRS80 \
  +step +proj=helmert +x=0 +y=0 +z=0 \
  +step +inv +proj=cart +ellps=WGS84 \
  +step +proj=unitconvert +xy_in=rad +xy_out=deg";

/// One PostGIS feature selected for a requested tile bbox.
///
/// Extruded footprints contain an EPSG:4326 clipped fragment plus the portions
/// of the original feature boundary that remain inside the tile. The boundary
/// mask prevents clip edges from becoming artificial extrusion walls. Native
/// surface features contain the whole source geometry normalized to Lucy's
/// explicit EPSG:4979 target contract. Content generation triangulates them in
/// a stable source frame and clips the triangles per tile, because PostGIS XY
/// overlay would collapse vertical PolygonZ faces.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedFeature {
    pub id: String,
    pub geometry: NormalizedGeometry,
    pub encoded_size_bytes: usize,
    pub attributes: BTreeMap<String, Option<String>>,
}

/// Query a tile bbox from PostGIS using the configured geometry strategy.
pub async fn query_normalized_features(
    client: &impl GenericClient,
    source: &SourceConfig,
    tile: TileCoord,
) -> Result<Vec<NormalizedFeature>, TileQueryError> {
    if tile.level < source.min_level || tile.level > source.max_level {
        return Err(ConfigError::Validation(format!(
            "tile level={} is outside configured levels {}..={}",
            tile.level, source.min_level, source.max_level
        ))
        .into());
    }
    let bbox = tile.geographic_region_degrees(&source.bounds)?;
    query_normalized_features_for_bbox(client, source, bbox).await
}

/// Query an explicit target-geodetic bbox and return decoded, normalized
/// geometry. Native surfaces are returned as whole-feature broad-phase
/// candidates and are clipped safely after 3D triangulation. WKB and
/// source-CRS details do not cross this adapter boundary.
#[tracing::instrument(
    name = "postgis.tile_geometry",
    skip(client, source, bbox),
    fields(
        db.operation = "select",
        db.query_kind = "tile_geometry",
        db.schema = %source.schema,
        db.table = %source.table,
        bbox.west = bbox.west,
        bbox.south = bbox.south,
        bbox.east = bbox.east,
        bbox.north = bbox.north,
        max_features_per_tile = source.max_features_per_tile,
    )
)]
async fn query_normalized_features_for_bbox(
    client: &impl GenericClient,
    source: &SourceConfig,
    bbox: GeographicRegionDegrees,
) -> Result<Vec<NormalizedFeature>, TileQueryError> {
    validate_query_bbox(bbox)?;

    if source.source_model == SourceModel::SurfaceGeometryZ {
        let mut features = Vec::new();
        for_each_normalized_surface_feature_for_bbox(client, None, source, bbox, |feature| {
            features.push(feature);
            Ok::<(), TileQueryError>(())
        })
        .await?;
        return Ok(features);
    }

    let plan = build_normalized_geometry_query(source)?;
    let query_limit = i64::from(source.max_features_per_tile) + 1;
    let started = Instant::now();
    let rows = match plan.bindings {
        QueryBindings::Standard => {
            client
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
                .await
        }
        QueryBindings::Rdnaptrans2018Epsg1149 => {
            client
                .query(
                    &plan.sql,
                    &[
                        &bbox.west,
                        &bbox.south,
                        &bbox.east,
                        &bbox.north,
                        &source.srid,
                        &RDNAPTRANS2018_EPSG_1149_PIPELINE,
                        &query_limit,
                    ],
                )
                .await
        }
    }
    .map_err(|source_error| plan.map_query_error(source, source_error))?;
    tracing::debug!(
        duration_ms = started.elapsed().as_secs_f64() * 1_000.0,
        row_count = rows.len(),
        "PostGIS query completed"
    );
    ensure_within_feature_limit(rows.len(), source.max_features_per_tile)?;

    let mut features = Vec::with_capacity(rows.len());
    for row in rows {
        features.push(normalized_feature_from_row(
            row,
            source.source_model,
            &plan.attributes,
        )?);
    }

    Ok(features)
}

/// Stream whole native-surface candidates for one tile and let the caller
/// apply exact 3D clipping before deciding feature limits.
pub async fn for_each_normalized_surface_feature<E, F>(
    client: &impl GenericClient,
    source: &SourceConfig,
    tile: TileCoord,
    consumer: F,
) -> Result<(), E>
where
    E: From<TileQueryError>,
    F: FnMut(NormalizedFeature) -> Result<(), E>,
{
    for_each_normalized_surface_feature_with_statement(client, None, source, tile, consumer).await
}

pub(crate) async fn for_each_normalized_surface_feature_prepared<E, F>(
    client: &impl GenericClient,
    statement: &Statement,
    source: &SourceConfig,
    tile: TileCoord,
    consumer: F,
) -> Result<(), E>
where
    E: From<TileQueryError>,
    F: FnMut(NormalizedFeature) -> Result<(), E>,
{
    for_each_normalized_surface_feature_with_statement(
        client,
        Some(statement),
        source,
        tile,
        consumer,
    )
    .await
}

async fn for_each_normalized_surface_feature_with_statement<E, F>(
    client: &impl GenericClient,
    statement: Option<&Statement>,
    source: &SourceConfig,
    tile: TileCoord,
    consumer: F,
) -> Result<(), E>
where
    E: From<TileQueryError>,
    F: FnMut(NormalizedFeature) -> Result<(), E>,
{
    if source.source_model != SourceModel::SurfaceGeometryZ {
        return Err(E::from(TileQueryError::Config(ConfigError::Validation(
            "streaming surface candidates require source_model = surface_geometry_z".to_string(),
        ))));
    }
    if tile.level < source.min_level || tile.level > source.max_level {
        return Err(E::from(TileQueryError::Config(ConfigError::Validation(
            format!(
                "tile level={} is outside configured levels {}..={}",
                tile.level, source.min_level, source.max_level
            ),
        ))));
    }
    let bbox = tile
        .geographic_region_degrees(&source.bounds)
        .map_err(TileQueryError::from)
        .map_err(E::from)?;
    for_each_normalized_surface_feature_for_bbox(client, statement, source, bbox, consumer).await
}

async fn for_each_normalized_surface_feature_for_bbox<E, F>(
    client: &impl GenericClient,
    statement: Option<&Statement>,
    source: &SourceConfig,
    bbox: GeographicRegionDegrees,
    mut consumer: F,
) -> Result<(), E>
where
    E: From<TileQueryError>,
    F: FnMut(NormalizedFeature) -> Result<(), E>,
{
    validate_query_bbox(bbox)
        .map_err(TileQueryError::from)
        .map_err(E::from)?;
    let plan = build_normalized_geometry_query(source)
        .map_err(TileQueryError::from)
        .map_err(E::from)?;
    let started = Instant::now();
    let rows = match (plan.bindings, statement) {
        (QueryBindings::Standard, None) => {
            let params: [&(dyn ToSql + Sync); 5] = [
                &bbox.west,
                &bbox.south,
                &bbox.east,
                &bbox.north,
                &source.srid,
            ];
            client.query_raw(&plan.sql, params).await
        }
        (QueryBindings::Standard, Some(statement)) => {
            let params: [&(dyn ToSql + Sync); 5] = [
                &bbox.west,
                &bbox.south,
                &bbox.east,
                &bbox.north,
                &source.srid,
            ];
            client.query_raw(statement, params).await
        }
        (QueryBindings::Rdnaptrans2018Epsg1149, None) => {
            let params: [&(dyn ToSql + Sync); 6] = [
                &bbox.west,
                &bbox.south,
                &bbox.east,
                &bbox.north,
                &source.srid,
                &RDNAPTRANS2018_EPSG_1149_PIPELINE,
            ];
            client.query_raw(&plan.sql, params).await
        }
        (QueryBindings::Rdnaptrans2018Epsg1149, Some(statement)) => {
            let params: [&(dyn ToSql + Sync); 6] = [
                &bbox.west,
                &bbox.south,
                &bbox.east,
                &bbox.north,
                &source.srid,
                &RDNAPTRANS2018_EPSG_1149_PIPELINE,
            ];
            client.query_raw(statement, params).await
        }
    }
    .map_err(|error| E::from(plan.map_query_error(source, error)))?;
    let query_submit_duration = started.elapsed();
    tokio::pin!(rows);
    let mut row_count = 0_usize;
    let mut row_wait_duration = Duration::ZERO;
    let mut processing_duration = Duration::ZERO;
    loop {
        let row_wait_started = Instant::now();
        let next_row = rows
            .try_next()
            .await
            .map_err(|error| E::from(plan.map_query_error(source, error)))?;
        row_wait_duration += row_wait_started.elapsed();
        let Some(row) = next_row else {
            break;
        };

        let processing_started = Instant::now();
        row_count += 1;
        let feature = normalized_feature_from_row(row, source.source_model, &plan.attributes)
            .map_err(E::from)?;
        consumer(feature)?;
        processing_duration += processing_started.elapsed();
    }
    tracing::debug!(
        duration_ms = started.elapsed().as_secs_f64() * 1_000.0,
        query_submit_ms = query_submit_duration.as_secs_f64() * 1_000.0,
        row_wait_ms = row_wait_duration.as_secs_f64() * 1_000.0,
        processing_ms = processing_duration.as_secs_f64() * 1_000.0,
        row_count,
        "PostGIS native-surface candidates streamed"
    );
    Ok(())
}

pub(crate) fn normalized_surface_geometry_query_sql(
    source: &SourceConfig,
) -> Result<String, ConfigError> {
    if source.source_model != SourceModel::SurfaceGeometryZ {
        return Err(ConfigError::Validation(
            "prepared surface query requires source_model = surface_geometry_z".to_string(),
        ));
    }
    Ok(build_normalized_geometry_query(source)?.sql)
}

fn normalized_feature_from_row(
    row: Row,
    source_model: SourceModel,
    attributes: &[String],
) -> Result<NormalizedFeature, TileQueryError> {
    let id = row.try_get::<_, String>(0)?;
    let geometry_wkb = row.try_get::<_, Vec<u8>>(1)?;
    let source_boundary_wkb = row.try_get::<_, Option<Vec<u8>>>(2)?;
    let geometry =
        decode_normalized_geometry(source_model, &geometry_wkb, source_boundary_wkb.as_deref())
            .map_err(|error| {
                TileQueryError::SourceContract(format!(
                    "feature {id} did not match the adapter's normalized geometry contract: {error}"
                ))
            })?;
    let mut decoded_attributes = BTreeMap::new();
    for (index, attribute) in attributes.iter().enumerate() {
        decoded_attributes.insert(
            attribute.clone(),
            row.try_get::<_, Option<String>>(index + 3)?,
        );
    }
    Ok(NormalizedFeature {
        id,
        geometry,
        encoded_size_bytes: geometry_wkb.len() + source_boundary_wkb.as_ref().map_or(0, Vec::len),
        attributes: decoded_attributes,
    })
}

fn decode_normalized_geometry(
    source_model: SourceModel,
    geometry_wkb: &[u8],
    source_boundary_wkb: Option<&[u8]>,
) -> Result<NormalizedGeometry, String> {
    match source_model {
        SourceModel::ExtrudedFootprint => {
            let source_boundary_wkb = source_boundary_wkb.ok_or_else(|| {
                "extruded footprint is missing its original-boundary mask".to_string()
            })?;
            let geometry = decode_footprint_wkb(geometry_wkb).map_err(|error| error.to_string())?;
            let source_boundary = decode_multi_line_string_wkb(source_boundary_wkb)
                .map_err(|error| format!("source boundary: {error}"))?;
            Ok(NormalizedGeometry::GeographicFootprint(FootprintFragment {
                geometry,
                source_boundary,
            }))
        }
        SourceModel::SurfaceGeometryZ => {
            if source_boundary_wkb.is_some() {
                return Err(
                    "native surface unexpectedly included a footprint boundary mask".to_string(),
                );
            }
            decode_surface_geometry_z_wkb(geometry_wkb)
                .map(NormalizedGeometry::GeodeticSurface)
                .map_err(|error| error.to_string())
        }
    }
}

/// Derive all tile, content, and child-subtree availability for one subtree
/// with a single batched PostGIS query.
#[tracing::instrument(
    name = "postgis.subtree_availability",
    skip(client, source),
    fields(
        db.operation = "select",
        db.query_kind = "subtree_availability",
        db.schema = %source.schema,
        db.table = %source.table,
        tile.level = subtree_root.level,
        tile.x = subtree_root.x,
        tile.y = subtree_root.y,
        max_features_per_tile = source.max_features_per_tile,
    )
)]
pub async fn query_subtree_availability(
    client: &impl GenericClient,
    source: &SourceConfig,
    subtree_root: TileCoord,
) -> Result<SubtreeAvailabilityBits, TileQueryError> {
    let layout = subtree_layout(source, subtree_root)?;
    let slots = subtree_query_slots(source, &layout);

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

    if source.source_model == SourceModel::SurfaceGeometryZ {
        return query_surface_subtree_availability(
            client,
            source,
            subtree_root,
            &layout,
            &slots,
            &west,
            &south,
            &east,
            &north,
        )
        .await;
    }

    let plan = build_subtree_occupancy_query(source)?;
    let query_limit = slots
        .iter()
        .map(|slot| subtree_slot_feature_count_target(source, *slot))
        .max()
        .and_then(|target| i64::try_from(target).ok())
        .ok_or_else(|| {
            ConfigError::Validation("subtree query must contain at least one slot".to_string())
        })?;
    let started = Instant::now();
    let rows = match plan.bindings {
        QueryBindings::Standard => {
            client
                .query(
                    &plan.sql,
                    &[&west, &south, &east, &north, &source.srid, &query_limit],
                )
                .await
        }
        QueryBindings::Rdnaptrans2018Epsg1149 => {
            client
                .query(
                    &plan.sql,
                    &[
                        &west,
                        &south,
                        &east,
                        &north,
                        &source.srid,
                        &RDNAPTRANS2018_EPSG_1149_PIPELINE,
                        &query_limit,
                    ],
                )
                .await
        }
    }
    .map_err(|source_error| plan.map_query_error(source, source_error))?;
    tracing::debug!(
        duration_ms = started.elapsed().as_secs_f64() * 1_000.0,
        query_slot_count = slots.len(),
        row_count = rows.len(),
        "PostGIS query completed"
    );

    if rows.len() != slots.len() {
        return Err(TileQueryError::Config(ConfigError::Validation(format!(
            "PostGIS returned {} subtree occupancy rows for {} requested slots",
            rows.len(),
            slots.len()
        ))));
    }

    let mut feature_counts = vec![0; slots.len()];
    for row in rows {
        let slot_index = usize::try_from(row.try_get::<_, i64>(0)?).map_err(|_| {
            ConfigError::Validation("PostGIS returned a negative subtree slot".to_string())
        })?;
        let feature_count = u64::try_from(row.try_get::<_, i64>(1)?).map_err(|_| {
            ConfigError::Validation("PostGIS returned a negative feature count".to_string())
        })?;
        slots.get(slot_index).ok_or_else(|| {
            ConfigError::Validation(format!(
                "PostGIS returned out-of-range subtree slot {slot_index}"
            ))
        })?;
        feature_counts[slot_index] = feature_count;
    }

    availability_from_feature_counts(source, subtree_root, &layout, &slots, &feature_counts)
}

fn subtree_query_slots(source: &SourceConfig, layout: &SubtreeLayout) -> Vec<SubtreeQuerySlot> {
    let mut slots = Vec::new();
    for (index, tile) in layout.local_tiles.iter().copied().enumerate() {
        if let Some(tile) = tile
            && tile.level >= source.tileset.content_start_level
        {
            slots.push(SubtreeQuerySlot::Tile { index, tile });
        }
    }
    for (index, tile) in layout.child_roots.iter().copied().enumerate() {
        if let Some(tile) = tile {
            slots.push(SubtreeQuerySlot::ChildSubtree { index, tile });
        }
    }
    slots
}

fn availability_from_feature_counts(
    source: &SourceConfig,
    subtree_root: TileCoord,
    layout: &SubtreeLayout,
    slots: &[SubtreeQuerySlot],
    feature_counts: &[u64],
) -> Result<SubtreeAvailabilityBits, TileQueryError> {
    if feature_counts.len() != slots.len() {
        return Err(ConfigError::Validation(format!(
            "received {} feature counts for {} subtree slots",
            feature_counts.len(),
            slots.len()
        ))
        .into());
    }
    let mut availability = SubtreeAvailabilityBits {
        tile: vec![false; layout.local_tiles.len()],
        content: vec![false; layout.local_tiles.len()],
        child_subtree: vec![false; layout.child_roots.len()],
    };
    for (slot, &feature_count) in slots.iter().zip(feature_counts) {
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
                availability.content[index] =
                    has_features && !overflow && tile.level >= source.tileset.content_start_level;
            }
            SubtreeQuerySlot::ChildSubtree { index, .. } => {
                availability.child_subtree[index] = has_features;
            }
        }
    }
    if subtree_root == TileCoord::root() {
        availability.tile[0] = true;
    }
    close_tile_availability_over_ancestors(subtree_root, layout, &mut availability);

    Ok(availability)
}

fn close_tile_availability_over_ancestors(
    subtree_root: TileCoord,
    layout: &SubtreeLayout,
    availability: &mut SubtreeAvailabilityBits,
) {
    let local_indices = layout
        .local_tiles
        .iter()
        .enumerate()
        .filter_map(|(index, tile)| tile.map(|tile| ((tile.level, tile.x, tile.y), index)))
        .collect::<HashMap<_, _>>();
    let mut occupied_descendants = layout
        .local_tiles
        .iter()
        .zip(&availability.tile)
        .filter_map(|(tile, &available)| available.then_some(*tile).flatten())
        .collect::<Vec<_>>();
    occupied_descendants.extend(
        layout
            .child_roots
            .iter()
            .zip(&availability.child_subtree)
            .filter_map(|(tile, &available)| available.then_some(*tile).flatten()),
    );

    for mut tile in occupied_descendants {
        loop {
            if let Some(&index) = local_indices.get(&(tile.level, tile.x, tile.y)) {
                availability.tile[index] = true;
            }
            if tile.level == subtree_root.level {
                break;
            }
            tile = TileCoord {
                level: tile.level - 1,
                x: tile.x / 2,
                y: tile.y / 2,
            };
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn query_surface_subtree_availability(
    client: &impl GenericClient,
    source: &SourceConfig,
    subtree_root: TileCoord,
    layout: &SubtreeLayout,
    slots: &[SubtreeQuerySlot],
    west: &[f64],
    south: &[f64],
    east: &[f64],
    north: &[f64],
) -> Result<SubtreeAvailabilityBits, TileQueryError> {
    let mut feature_count_targets = slots
        .iter()
        .map(|slot| subtree_slot_feature_count_target(source, *slot))
        .collect::<Vec<_>>();
    let mut feature_counts = vec![0_u64; slots.len()];

    let exact_slots = if source.surface_subtree_envelope_shortcut {
        populate_surface_subtree_count_bounds(
            client,
            source,
            west,
            south,
            east,
            north,
            &mut feature_count_targets,
            &mut feature_counts,
        )
        .await?
    } else {
        // Without the audited shortcut none of the envelope lower bounds may
        // replace request-time mesh validation. Go straight to the exact path
        // instead of paying for an unused count-bound query first.
        (0..slots.len()).collect()
    };

    let boolean_slots = exact_slots
        .iter()
        .copied()
        .filter(|&index| feature_count_targets[index] == 1)
        .collect::<Vec<_>>();
    let counted_slots = exact_slots
        .into_iter()
        .filter(|&index| feature_count_targets[index] > 1)
        .collect::<Vec<_>>();

    populate_surface_subtree_slot_counts(
        client,
        source,
        slots,
        west,
        south,
        east,
        north,
        &feature_count_targets,
        &boolean_slots,
        1,
        &mut feature_counts,
    )
    .await?;
    populate_surface_subtree_slot_counts(
        client,
        source,
        slots,
        west,
        south,
        east,
        north,
        &feature_count_targets,
        &counted_slots,
        128,
        &mut feature_counts,
    )
    .await?;

    availability_from_feature_counts(source, subtree_root, layout, slots, &feature_counts)
}

#[allow(clippy::too_many_arguments)]
async fn populate_surface_subtree_count_bounds(
    client: &impl GenericClient,
    source: &SourceConfig,
    west: &[f64],
    south: &[f64],
    east: &[f64],
    north: &[f64],
    feature_count_targets: &mut [u64],
    feature_counts: &mut [u64],
) -> Result<Vec<usize>, TileQueryError> {
    if feature_count_targets.is_empty() {
        return Ok(Vec::new());
    }
    let plan = build_surface_subtree_count_query(source)?;
    let query_limits = feature_count_targets
        .iter()
        .copied()
        .map(i64::try_from)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ConfigError::Validation("subtree count target overflowed i64".to_string()))?;
    let started = Instant::now();
    let rows = match plan.bindings {
        QueryBindings::Standard => {
            let params: [&(dyn ToSql + Sync); 6] =
                [&west, &south, &east, &north, &query_limits, &source.srid];
            client.query(&plan.sql, &params).await
        }
        QueryBindings::Rdnaptrans2018Epsg1149 => {
            let params: [&(dyn ToSql + Sync); 7] = [
                &west,
                &south,
                &east,
                &north,
                &query_limits,
                &source.srid,
                &RDNAPTRANS2018_EPSG_1149_PIPELINE,
            ];
            client.query(&plan.sql, &params).await
        }
    }
    .map_err(|source_error| plan.map_query_error(source, source_error))?;

    if rows.len() != feature_count_targets.len() {
        return Err(ConfigError::Validation(format!(
            "PostGIS returned {} native-surface bound rows for {} slots",
            rows.len(),
            feature_count_targets.len()
        ))
        .into());
    }
    let mut exact_slots = Vec::new();
    for row in rows {
        let slot_index = usize::try_from(row.try_get::<_, i64>(0)?).map_err(|_| {
            ConfigError::Validation(
                "PostGIS returned a negative native-surface bound slot".to_string(),
            )
        })?;
        let &target = feature_count_targets.get(slot_index).ok_or_else(|| {
            ConfigError::Validation(format!(
                "PostGIS returned out-of-range native-surface bound slot {slot_index}"
            ))
        })?;
        let bbox_count = u64::try_from(row.try_get::<_, i64>(1)?).map_err(|_| {
            ConfigError::Validation(
                "PostGIS returned a negative native-surface bbox count".to_string(),
            )
        })?;
        let contained_count = u64::try_from(row.try_get::<_, i64>(2)?).map_err(|_| {
            ConfigError::Validation(
                "PostGIS returned a negative native-surface contained count".to_string(),
            )
        })?;
        if contained_count > bbox_count || bbox_count > target {
            return Err(ConfigError::Validation(format!(
                "PostGIS returned invalid native-surface bounds for slot {slot_index}: bbox={bbox_count} contained={contained_count} target={target}"
            ))
            .into());
        }

        if classify_surface_subtree_count_bound(
            &mut feature_count_targets[slot_index],
            &mut feature_counts[slot_index],
            bbox_count,
            contained_count,
            source.surface_subtree_envelope_shortcut,
        ) {
            exact_slots.push(slot_index);
        }
    }
    tracing::debug!(
        duration_ms = started.elapsed().as_secs_f64() * 1_000.0,
        query_slot_count = feature_count_targets.len(),
        exact_fallback_slot_count = exact_slots.len(),
        "PostGIS native-surface subtree count bounds queried"
    );
    Ok(exact_slots)
}

fn classify_surface_subtree_count_bound(
    target: &mut u64,
    feature_count: &mut u64,
    bbox_count: u64,
    contained_count: u64,
    envelope_shortcut: bool,
) -> bool {
    if bbox_count == 0 {
        return false;
    }
    if !envelope_shortcut {
        // Preserve request-time decoding and mesh-contract validation for
        // sources that have not opted into the audited envelope shortcut.
        *feature_count = 0;
        return true;
    }
    if contained_count == *target {
        *feature_count = contained_count;
        return false;
    }
    if bbox_count < *target {
        // A capped broad-phase count below the overflow threshold proves that
        // this slot cannot overflow. Any fully contained feature is also an
        // exact positive fragment, so its precise count is not needed.
        if contained_count > 0 {
            *feature_count = contained_count;
            return false;
        }
        *target = 1;
        return true;
    }

    // Fully contained feature bboxes are an exact lower bound. Keep that work
    // and exact-clip only non-contained fringe candidates.
    *feature_count = contained_count;
    true
}

#[allow(clippy::too_many_arguments)]
async fn populate_surface_subtree_slot_counts(
    client: &impl GenericClient,
    source: &SourceConfig,
    slots: &[SubtreeQuerySlot],
    west: &[f64],
    south: &[f64],
    east: &[f64],
    north: &[f64],
    feature_count_targets: &[u64],
    slot_indices: &[usize],
    candidate_batch_size: usize,
    feature_counts: &mut [u64],
) -> Result<(), TileQueryError> {
    if slot_indices.is_empty() {
        return Ok(());
    }
    let plan = build_surface_subtree_exact_candidates_query(source)?;
    let statement = client
        .prepare(&plan.sql)
        .await
        .map_err(|source_error| plan.map_query_error(source, source_error))?;
    let source_frame = MeshFrame::from_source_bounds(&source.bounds);
    let mut unresolved_slots = slot_indices.to_vec();
    let mut candidate_offsets = vec![0_i64; slots.len()];
    let mut candidate_feature_count = 0_usize;
    let mut query_batch_count = 0_usize;
    let started = Instant::now();

    while !unresolved_slots.is_empty() {
        let batch_west = unresolved_slots
            .iter()
            .map(|&slot_index| west[slot_index])
            .collect::<Vec<_>>();
        let batch_south = unresolved_slots
            .iter()
            .map(|&slot_index| south[slot_index])
            .collect::<Vec<_>>();
        let batch_east = unresolved_slots
            .iter()
            .map(|&slot_index| east[slot_index])
            .collect::<Vec<_>>();
        let batch_north = unresolved_slots
            .iter()
            .map(|&slot_index| north[slot_index])
            .collect::<Vec<_>>();
        let batch_limits = unresolved_slots
            .iter()
            .map(|&slot_index| {
                let remaining =
                    feature_count_targets[slot_index].saturating_sub(feature_counts[slot_index]);
                // Counted slots may contain broad-phase misses. Modest
                // overfetch avoids another sorted OFFSET scan when the number
                // of fringe candidates is only slightly above the remaining
                // exact-count target. Boolean slots still fetch one at a time.
                let candidate_limit = if feature_count_targets[slot_index] == 1 {
                    1
                } else {
                    remaining.saturating_mul(2).min(candidate_batch_size as u64)
                };
                i64::try_from(candidate_limit).expect("candidate batch size fits i64")
            })
            .collect::<Vec<_>>();
        let batch_offsets = unresolved_slots
            .iter()
            .map(|&slot_index| candidate_offsets[slot_index])
            .collect::<Vec<_>>();
        let rows = match plan.bindings {
            QueryBindings::Standard => {
                client
                    .query(
                        &statement,
                        &[
                            &batch_west,
                            &batch_south,
                            &batch_east,
                            &batch_north,
                            &batch_limits,
                            &batch_offsets,
                            &source.srid,
                        ],
                    )
                    .await
            }
            QueryBindings::Rdnaptrans2018Epsg1149 => {
                client
                    .query(
                        &statement,
                        &[
                            &batch_west,
                            &batch_south,
                            &batch_east,
                            &batch_north,
                            &batch_limits,
                            &batch_offsets,
                            &source.srid,
                            &RDNAPTRANS2018_EPSG_1149_PIPELINE,
                        ],
                    )
                    .await
            }
        }
        .map_err(|source_error| plan.map_query_error(source, source_error))?;
        query_batch_count += 1;

        let mut returned_counts = vec![0_usize; unresolved_slots.len()];
        for row in rows {
            candidate_feature_count += 1;
            let batch_slot_index = usize::try_from(row.try_get::<_, i64>(0)?).map_err(|_| {
                ConfigError::Validation(
                    "PostGIS returned a negative boolean subtree slot".to_string(),
                )
            })?;
            let original_slot_index = *unresolved_slots.get(batch_slot_index).ok_or_else(|| {
                ConfigError::Validation(format!(
                    "PostGIS returned out-of-range boolean subtree slot {batch_slot_index}"
                ))
            })?;
            returned_counts[batch_slot_index] += 1;
            if feature_counts[original_slot_index] >= feature_count_targets[original_slot_index] {
                continue;
            }

            let feature_id = row.try_get::<_, String>(1)?;
            let geometry_wkb = row.try_get::<_, Vec<u8>>(2)?;
            let geometry = decode_surface_geometry_z_wkb(&geometry_wkb).map_err(|error| {
                TileQueryError::SourceContract(format!(
                    "feature {feature_id} did not match the adapter's normalized geometry contract: {error}"
                ))
            })?;
            let prepared = prepare_surface_geometry_z(&geometry, source_frame).map_err(|error| {
                TileQueryError::SourceContract(format!(
                    "feature {feature_id} could not be prepared for subtree availability: {error}"
                ))
            })?;
            let clip = surface_tile_clip(source, slots[original_slot_index].tile())?;
            if prepared.has_tile_content(clip).map_err(|error| {
                TileQueryError::SourceContract(format!(
                    "feature {feature_id} could not be clipped for subtree availability: {error}"
                ))
            })? {
                feature_counts[original_slot_index] += 1;
            }
        }

        let mut next_unresolved_slots = Vec::new();
        for (batch_slot_index, original_slot_index) in unresolved_slots.into_iter().enumerate() {
            let requested_count = usize::try_from(batch_limits[batch_slot_index])
                .expect("positive candidate limit fits usize");
            if feature_counts[original_slot_index] < feature_count_targets[original_slot_index]
                && returned_counts[batch_slot_index] == requested_count
            {
                candidate_offsets[original_slot_index] = candidate_offsets[original_slot_index]
                    .checked_add(batch_limits[batch_slot_index])
                    .ok_or_else(|| {
                        ConfigError::Validation("candidate offset overflowed".to_string())
                    })?;
                next_unresolved_slots.push(original_slot_index);
            }
        }
        unresolved_slots = next_unresolved_slots;
    }

    tracing::debug!(
        duration_ms = started.elapsed().as_secs_f64() * 1_000.0,
        query_slot_count = slots.len(),
        requested_slot_count = slot_indices.len(),
        candidate_batch_size,
        query_batch_count,
        candidate_feature_count,
        "PostGIS native-surface paged subtree candidates queried"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum SubtreeQuerySlot {
    Tile { index: usize, tile: TileCoord },
    ChildSubtree { index: usize, tile: TileCoord },
}

fn subtree_slot_feature_count_target(source: &SourceConfig, slot: SubtreeQuerySlot) -> u64 {
    match slot {
        SubtreeQuerySlot::Tile { tile, .. } if tile.level >= source.tileset.content_start_level => {
            u64::from(source.max_features_per_tile) + 1
        }
        SubtreeQuerySlot::Tile { .. } | SubtreeQuerySlot::ChildSubtree { .. } => 1,
    }
}

impl SubtreeQuerySlot {
    fn tile(self) -> TileCoord {
        match self {
            Self::Tile { tile, .. } | Self::ChildSubtree { tile, .. } => tile,
        }
    }
}

pub(crate) fn surface_tile_clip(
    source: &SourceConfig,
    tile: TileCoord,
) -> Result<SurfaceTileClip, ConfigError> {
    let region = tile.geographic_region_degrees(&source.bounds)?;
    let tile_count = 1_u32
        .checked_shl(u32::from(tile.level))
        .ok_or_else(|| ConfigError::Validation(format!("tile level {} is too deep", tile.level)))?;
    let outer_index = tile_count - 1;
    Ok(SurfaceTileClip {
        west_deg: region.west,
        south_deg: region.south,
        east_deg: region.east,
        north_deg: region.north,
        include_east: tile.x == outer_index,
        include_north: tile.y == outer_index,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceGeometryProfile {
    pub row_count: u64,
    pub srids: Vec<i32>,
    pub geometry_types: Vec<String>,
    pub zm_flags: Vec<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMetadataProfile {
    pub declared_geometry_type: Option<String>,
    pub declared_srid: Option<i32>,
    pub declared_dimensions: Option<i32>,
    pub id_not_null: bool,
    pub geometry_not_null: bool,
    pub id_unique: bool,
}

#[derive(Debug)]
pub enum SourceValidationError {
    ConnectionConfig {
        source_id: String,
        message: String,
    },
    Database {
        source_id: String,
        stage: &'static str,
        source: tokio_postgres::Error,
    },
    RelationNotFound {
        source_id: String,
        schema: String,
        table: String,
    },
    MissingColumns {
        source_id: String,
        columns: Vec<String>,
    },
    GeometryColumnType {
        source_id: String,
        column: String,
        actual_type: String,
    },
    DeclaredGeometryContract {
        source_id: String,
        message: String,
    },
    NullFeatureId {
        source_id: String,
        count: u64,
    },
    EmptyFeatureId {
        source_id: String,
        count: u64,
    },
    DuplicateFeatureId {
        source_id: String,
    },
    NullGeometry {
        source_id: String,
        count: u64,
    },
    EmptyGeometry {
        source_id: String,
        count: u64,
    },
    SridProfile {
        source_id: String,
        expected: i32,
        found: Vec<i32>,
    },
    GeometryTypeProfile {
        source_id: String,
        allowed: Vec<String>,
        found: Vec<String>,
    },
    CoordinateDimensionProfile {
        source_id: String,
        expected_zm_flag: i32,
        found: Vec<i32>,
    },
    NonFiniteCoordinate {
        source_id: String,
        feature_id: String,
    },
    TransformUnavailable {
        source_id: String,
        source_srid: i32,
        target_srid: i32,
        operation: &'static str,
        source: tokio_postgres::Error,
    },
    TransformContract {
        source_id: String,
        message: String,
    },
    TransformedExtentOutsideBounds {
        source_id: String,
        configured: [f64; 6],
        actual: [f64; 6],
    },
    Config {
        source_id: String,
        source: ConfigError,
    },
}

impl SourceValidationError {
    fn database(source_id: &str, stage: &'static str, source: tokio_postgres::Error) -> Self {
        Self::Database {
            source_id: source_id.to_string(),
            stage,
            source,
        }
    }
}

impl fmt::Display for SourceValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionConfig { source_id, message } => {
                write!(
                    f,
                    "source {source_id} connection is not configured: {message}"
                )
            }
            Self::Database {
                source_id,
                stage,
                source,
            } => write!(
                f,
                "source {source_id} PostGIS validation failed during {stage}: {source}"
            ),
            Self::RelationNotFound {
                source_id,
                schema,
                table,
            } => write!(
                f,
                "source {source_id} relation {schema}.{table} does not exist"
            ),
            Self::MissingColumns { source_id, columns } => write!(
                f,
                "source {source_id} is missing required column(s): {}",
                columns.join(", ")
            ),
            Self::GeometryColumnType {
                source_id,
                column,
                actual_type,
            } => write!(
                f,
                "source {source_id} geometry column {column} has type {actual_type}, expected PostGIS geometry"
            ),
            Self::DeclaredGeometryContract { source_id, message } => write!(
                f,
                "source {source_id} declared geometry contract does not match its configuration: {message}"
            ),
            Self::NullFeatureId { source_id, count } => write!(
                f,
                "source {source_id} contains {count} row(s) with a NULL feature id"
            ),
            Self::EmptyFeatureId { source_id, count } => write!(
                f,
                "source {source_id} contains {count} row(s) whose feature id is an empty string"
            ),
            Self::DuplicateFeatureId { source_id } => {
                write!(f, "source {source_id} feature ids are not unique")
            }
            Self::NullGeometry { source_id, count } => write!(
                f,
                "source {source_id} contains {count} row(s) with NULL geometry"
            ),
            Self::EmptyGeometry { source_id, count } => write!(
                f,
                "source {source_id} contains {count} row(s) with empty geometry"
            ),
            Self::SridProfile {
                source_id,
                expected,
                found,
            } => write!(
                f,
                "source {source_id} geometry SRID profile {found:?} does not match EPSG:{expected}"
            ),
            Self::GeometryTypeProfile {
                source_id,
                allowed,
                found,
            } => write!(
                f,
                "source {source_id} geometry type profile {found:?} is not contained in {allowed:?}"
            ),
            Self::CoordinateDimensionProfile {
                source_id,
                expected_zm_flag,
                found,
            } => write!(
                f,
                "source {source_id} coordinate Z/M profile {found:?} does not match required ST_Zmflag={expected_zm_flag}"
            ),
            Self::NonFiniteCoordinate {
                source_id,
                feature_id,
            } => write!(
                f,
                "source {source_id} feature {feature_id} contains a missing or non-finite coordinate"
            ),
            Self::TransformUnavailable {
                source_id,
                source_srid,
                target_srid,
                operation,
                source,
            } => write!(
                f,
                "source {source_id} cannot transform EPSG:{source_srid} -> EPSG:{target_srid} with {operation}: {source}"
            ),
            Self::TransformContract { source_id, message } => {
                write!(f, "source {source_id} transform contract failed: {message}")
            }
            Self::TransformedExtentOutsideBounds {
                source_id,
                configured,
                actual,
            } => write!(
                f,
                "source {source_id} transformed extent {actual:?} is outside configured EPSG:4979 bounds {configured:?}"
            ),
            Self::Config { source_id, source } => {
                write!(
                    f,
                    "source {source_id} query configuration is invalid: {source}"
                )
            }
        }
    }
}

impl std::error::Error for SourceValidationError {}

/// Validate source metadata and transformation capabilities without scanning
/// source rows. Generic geometry typmods and absent database constraints are
/// reported in the returned profile as unknown rather than inferred from data.
pub async fn validate_source_metadata(
    client: &impl GenericClient,
    source_id: &str,
    source: &SourceConfig,
) -> Result<SourceMetadataProfile, SourceValidationError> {
    validate_source_columns(client, source_id, source).await?;
    validate_source_select_permissions(client, source_id, source).await?;
    let profile = query_source_metadata_profile(client, source_id, source).await?;
    validate_declared_geometry_contract(source_id, source, &profile)?;
    match source.source_model {
        SourceModel::ExtrudedFootprint => {
            validate_footprint_transform(client, source_id, source).await?;
        }
        SourceModel::SurfaceGeometryZ => {
            validate_surface_transform(client, source_id, source).await?;
        }
    }
    Ok(profile)
}

/// Validate the configured relation and its strategy-specific geometry contract.
///
/// Surface validation deliberately does not call `ST_IsValid`: PostGIS/GEOS
/// validates PolygonZ topology in XY, where legitimate vertical faces collapse.
pub async fn validate_source(
    client: &impl GenericClient,
    source_id: &str,
    source: &SourceConfig,
) -> Result<SourceGeometryProfile, SourceValidationError> {
    validate_source_columns(client, source_id, source).await?;
    let profile = query_source_geometry_profile(client, source_id, source).await?;
    validate_source_geometry_profile(source_id, source, &profile)?;
    validate_finite_coordinates(client, source_id, source).await?;
    match source.source_model {
        SourceModel::ExtrudedFootprint => {
            validate_footprint_transform(client, source_id, source).await?;
        }
        SourceModel::SurfaceGeometryZ => {
            validate_surface_transform(client, source_id, source).await?;
            if profile.row_count > 0 {
                validate_surface_extent(client, source_id, source).await?;
            }
        }
    }
    Ok(profile)
}

async fn validate_footprint_transform(
    client: &impl GenericClient,
    source_id: &str,
    source: &SourceConfig,
) -> Result<(), SourceValidationError> {
    let longitude = (source.bounds.west + source.bounds.east) / 2.0;
    let latitude = (source.bounds.south + source.bounds.north) / 2.0;
    let row = client
        .query_one(
            &format!(
                "WITH target_point AS ( \
                   SELECT ST_SetSRID(ST_MakePoint($1, $2), {TARGET_GEOGRAPHIC_2D_SRID}) AS geom \
                 ), source_point AS ( \
                   SELECT ST_Transform(geom, $3::integer) AS geom FROM target_point \
                 ), roundtrip AS ( \
                   SELECT ST_Transform(geom, {TARGET_GEOGRAPHIC_2D_SRID}) AS geom FROM source_point \
                 ) \
                 SELECT ST_X(geom), ST_Y(geom), ST_SRID(geom) FROM roundtrip"
            ),
            &[&longitude, &latitude, &source.srid],
        )
        .await
        .map_err(|source_error| {
            let is_transform_failure = source_error.as_db_error().is_some_and(|database_error| {
                is_coordinate_transform_failure(
                    database_error.code().code(),
                    database_error.message(),
                )
            });
            if is_transform_failure {
                SourceValidationError::TransformUnavailable {
                    source_id: source_id.to_string(),
                    source_srid: source.srid,
                    target_srid: TARGET_GEOGRAPHIC_2D_SRID,
                    operation: POSTGIS_AUTOMATIC_TRANSFORM_OPERATION,
                    source: source_error,
                }
            } else {
                SourceValidationError::database(source_id, "transform probe", source_error)
            }
        })?;

    let transformed_longitude = row.get::<_, f64>(0);
    let transformed_latitude = row.get::<_, f64>(1);
    let transformed_srid = row.get::<_, i32>(2);
    const ROUNDTRIP_TOLERANCE_DEG: f64 = 1.0e-7;
    if !transformed_longitude.is_finite()
        || !transformed_latitude.is_finite()
        || transformed_srid != TARGET_GEOGRAPHIC_2D_SRID
        || (transformed_longitude - longitude).abs() > ROUNDTRIP_TOLERANCE_DEG
        || (transformed_latitude - latitude).abs() > ROUNDTRIP_TOLERANCE_DEG
    {
        return Err(SourceValidationError::TransformContract {
            source_id: source_id.to_string(),
            message: format!(
                "footprint transform probe returned ({transformed_longitude}, {transformed_latitude}) EPSG:{transformed_srid}; expected ({longitude}, {latitude}) EPSG:{TARGET_GEOGRAPHIC_2D_SRID}"
            ),
        });
    }
    Ok(())
}

async fn validate_source_columns(
    client: &impl GenericClient,
    source_id: &str,
    source: &SourceConfig,
) -> Result<(), SourceValidationError> {
    let relation = client
        .query_opt(
            "SELECT c.oid::bigint \
             FROM pg_catalog.pg_class AS c \
             JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 \
               AND c.relkind IN ('r', 'p', 'v', 'm', 'f')",
            &[&source.schema, &source.table],
        )
        .await
        .map_err(|error| SourceValidationError::database(source_id, "relation lookup", error))?;
    let Some(relation) = relation else {
        return Err(SourceValidationError::RelationNotFound {
            source_id: source_id.to_string(),
            schema: source.schema.clone(),
            table: source.table.clone(),
        });
    };
    let relation_oid = relation.get::<_, i64>(0);
    let rows = client
        .query(
            "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod) \
             FROM pg_catalog.pg_attribute AS a \
             WHERE a.attrelid = $1::bigint::oid AND a.attnum > 0 AND NOT a.attisdropped",
            &[&relation_oid],
        )
        .await
        .map_err(|error| SourceValidationError::database(source_id, "column lookup", error))?;

    let columns = rows
        .iter()
        .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
        .collect::<BTreeMap<_, _>>();
    let mut required = BTreeSet::from([source.geometry_column.clone(), source.id_column.clone()]);
    required.extend(source.content_query_attributes());
    let missing = required
        .into_iter()
        .filter(|column| !columns.contains_key(column))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(SourceValidationError::MissingColumns {
            source_id: source_id.to_string(),
            columns: missing,
        });
    }

    let geometry_type = &columns[&source.geometry_column];
    if geometry_type != "geometry" && !geometry_type.starts_with("geometry(") {
        return Err(SourceValidationError::GeometryColumnType {
            source_id: source_id.to_string(),
            column: source.geometry_column.clone(),
            actual_type: geometry_type.clone(),
        });
    }
    Ok(())
}

async fn validate_source_select_permissions(
    client: &impl GenericClient,
    source_id: &str,
    source: &SourceConfig,
) -> Result<(), SourceValidationError> {
    let schema = quote_identifier(&source.schema, "schema").map_err(|source| {
        SourceValidationError::Config {
            source_id: source_id.to_string(),
            source,
        }
    })?;
    let table = quote_identifier(&source.table, "table").map_err(|source| {
        SourceValidationError::Config {
            source_id: source_id.to_string(),
            source,
        }
    })?;
    let mut columns = vec![source.id_column.as_str(), source.geometry_column.as_str()];
    let attributes = source.content_query_attributes();
    columns.extend(attributes.iter().map(String::as_str));
    let columns = columns
        .into_iter()
        .map(|column| quote_identifier(column, "source column"))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| SourceValidationError::Config {
            source_id: source_id.to_string(),
            source,
        })?
        .join(", ");
    client
        .query(
            &format!("SELECT {columns} FROM {schema}.{table} LIMIT 0"),
            &[],
        )
        .await
        .map_err(|error| {
            SourceValidationError::database(source_id, "select permission probe", error)
        })?;
    Ok(())
}

async fn query_source_metadata_profile(
    client: &impl GenericClient,
    source_id: &str,
    source: &SourceConfig,
) -> Result<SourceMetadataProfile, SourceValidationError> {
    let row = client
        .query_one(
            "SELECT postgis_typmod_type(geometry_attribute.atttypmod), \
                    postgis_typmod_srid(geometry_attribute.atttypmod), \
                    postgis_typmod_dims(geometry_attribute.atttypmod), \
                    id_attribute.attnotnull, geometry_attribute.attnotnull, \
                    EXISTS ( \
                      SELECT 1 FROM pg_catalog.pg_index AS index \
                      WHERE index.indrelid = relation.oid \
                        AND index.indisunique AND index.indisvalid \
                        AND index.indpred IS NULL AND index.indexprs IS NULL \
                        AND index.indnkeyatts = 1 \
                        AND index.indkey[0] = id_attribute.attnum \
                    ) \
             FROM pg_catalog.pg_class AS relation \
             JOIN pg_catalog.pg_namespace AS namespace ON namespace.oid = relation.relnamespace \
             JOIN pg_catalog.pg_attribute AS geometry_attribute \
               ON geometry_attribute.attrelid = relation.oid AND geometry_attribute.attname = $3 \
             JOIN pg_catalog.pg_attribute AS id_attribute \
               ON id_attribute.attrelid = relation.oid AND id_attribute.attname = $4 \
             WHERE namespace.nspname = $1 AND relation.relname = $2",
            &[
                &source.schema,
                &source.table,
                &source.geometry_column,
                &source.id_column,
            ],
        )
        .await
        .map_err(|error| SourceValidationError::database(source_id, "metadata profile", error))?;

    let typmod_type = row.get::<_, String>(0);
    let typmod_srid = row.get::<_, i32>(1);
    let typmod_dimensions = row.get::<_, Option<i32>>(2);
    Ok(SourceMetadataProfile {
        declared_geometry_type: (typmod_type != "Geometry").then_some(typmod_type),
        declared_srid: (typmod_srid > 0).then_some(typmod_srid),
        declared_dimensions: typmod_dimensions,
        id_not_null: row.get(3),
        geometry_not_null: row.get(4),
        id_unique: row.get(5),
    })
}

fn validate_declared_geometry_contract(
    source_id: &str,
    source: &SourceConfig,
    profile: &SourceMetadataProfile,
) -> Result<(), SourceValidationError> {
    if let Some(declared_type) = &profile.declared_geometry_type {
        if declared_type.starts_with("Geometry") {
            let expected_generic_type = match source.source_model {
                SourceModel::ExtrudedFootprint => "Geometry",
                SourceModel::SurfaceGeometryZ => "GeometryZ",
            };
            if declared_type != expected_generic_type {
                return Err(SourceValidationError::DeclaredGeometryContract {
                    source_id: source_id.to_string(),
                    message: format!(
                        "column typmod is {declared_type}, expected {expected_generic_type}"
                    ),
                });
            }
        } else {
            let allowed = source
                .geometry_types
                .iter()
                .copied()
                .map(postgis_typmod_geometry_type)
                .collect::<Vec<_>>();
            if !allowed
                .iter()
                .any(|allowed_type| allowed_type.eq_ignore_ascii_case(declared_type))
            {
                return Err(SourceValidationError::DeclaredGeometryContract {
                    source_id: source_id.to_string(),
                    message: format!(
                        "column typmod is {declared_type}, expected one of {allowed:?}"
                    ),
                });
            }
        }
    }
    if let Some(declared_srid) = profile.declared_srid
        && declared_srid != source.srid
    {
        return Err(SourceValidationError::DeclaredGeometryContract {
            source_id: source_id.to_string(),
            message: format!(
                "column typmod uses EPSG:{declared_srid}, expected EPSG:{}",
                source.srid
            ),
        });
    }
    let expected_dimensions = match source.source_model {
        SourceModel::ExtrudedFootprint => 2,
        SourceModel::SurfaceGeometryZ => 3,
    };
    if let Some(declared_dimensions) = profile.declared_dimensions
        && declared_dimensions != expected_dimensions
    {
        return Err(SourceValidationError::DeclaredGeometryContract {
            source_id: source_id.to_string(),
            message: format!(
                "column typmod has {declared_dimensions} dimensions, expected {expected_dimensions}"
            ),
        });
    }
    Ok(())
}

fn postgis_typmod_geometry_type(geometry_type: GeometryType) -> &'static str {
    match geometry_type {
        GeometryType::Polygon => "Polygon",
        GeometryType::MultiPolygon => "MultiPolygon",
        GeometryType::PolygonZ => "PolygonZ",
        GeometryType::MultiPolygonZ => "MultiPolygonZ",
    }
}

async fn query_source_geometry_profile(
    client: &impl GenericClient,
    source_id: &str,
    source: &SourceConfig,
) -> Result<SourceGeometryProfile, SourceValidationError> {
    let schema = quote_identifier(&source.schema, "schema").map_err(|error| {
        SourceValidationError::Config {
            source_id: source_id.to_string(),
            source: error,
        }
    })?;
    let table = quote_identifier(&source.table, "table").map_err(|error| {
        SourceValidationError::Config {
            source_id: source_id.to_string(),
            source: error,
        }
    })?;
    let geometry =
        quote_identifier(&source.geometry_column, "geometry_column").map_err(|error| {
            SourceValidationError::Config {
                source_id: source_id.to_string(),
                source: error,
            }
        })?;
    let id = quote_identifier(&source.id_column, "id_column").map_err(|error| {
        SourceValidationError::Config {
            source_id: source_id.to_string(),
            source: error,
        }
    })?;
    let sql = format!(
        "SELECT count(*)::bigint, \
                count(*) FILTER (WHERE {geometry} IS NULL)::bigint, \
                count(*) FILTER (WHERE {geometry} IS NOT NULL AND ST_IsEmpty({geometry}))::bigint, \
                count(*) FILTER (WHERE {id} IS NULL)::bigint, \
                count(*) FILTER (WHERE {id} IS NOT NULL AND {id}::text = '')::bigint, \
                count(DISTINCT {id})::bigint, \
                COALESCE(array_agg(DISTINCT ST_SRID({geometry})) \
                  FILTER (WHERE {geometry} IS NOT NULL AND NOT ST_IsEmpty({geometry})), ARRAY[]::integer[]), \
                COALESCE(array_agg(DISTINCT GeometryType({geometry})) \
                  FILTER (WHERE {geometry} IS NOT NULL AND NOT ST_IsEmpty({geometry})), ARRAY[]::text[]), \
                COALESCE(array_agg(DISTINCT ST_Zmflag({geometry})::integer) \
                  FILTER (WHERE {geometry} IS NOT NULL AND NOT ST_IsEmpty({geometry})), ARRAY[]::integer[]) \
         FROM {schema}.{table}"
    );
    let row = client
        .query_one(&sql, &[])
        .await
        .map_err(|error| SourceValidationError::database(source_id, "geometry profile", error))?;
    let row_count = u64::try_from(row.get::<_, i64>(0)).unwrap_or(0);
    let null_geometry_count = u64::try_from(row.get::<_, i64>(1)).unwrap_or(0);
    let empty_geometry_count = u64::try_from(row.get::<_, i64>(2)).unwrap_or(0);
    let null_id_count = u64::try_from(row.get::<_, i64>(3)).unwrap_or(0);
    let empty_id_count = u64::try_from(row.get::<_, i64>(4)).unwrap_or(0);
    let distinct_id_count = u64::try_from(row.get::<_, i64>(5)).unwrap_or(0);
    if null_id_count > 0 {
        return Err(SourceValidationError::NullFeatureId {
            source_id: source_id.to_string(),
            count: null_id_count,
        });
    }
    if empty_id_count > 0 {
        return Err(SourceValidationError::EmptyFeatureId {
            source_id: source_id.to_string(),
            count: empty_id_count,
        });
    }
    if distinct_id_count != row_count {
        return Err(SourceValidationError::DuplicateFeatureId {
            source_id: source_id.to_string(),
        });
    }
    if null_geometry_count > 0 {
        return Err(SourceValidationError::NullGeometry {
            source_id: source_id.to_string(),
            count: null_geometry_count,
        });
    }
    if empty_geometry_count > 0 {
        return Err(SourceValidationError::EmptyGeometry {
            source_id: source_id.to_string(),
            count: empty_geometry_count,
        });
    }

    let mut srids = row.get::<_, Vec<i32>>(6);
    let mut geometry_types = row.get::<_, Vec<String>>(7);
    let mut zm_flags = row.get::<_, Vec<i32>>(8);
    srids.sort_unstable();
    geometry_types.sort();
    zm_flags.sort_unstable();
    Ok(SourceGeometryProfile {
        row_count,
        srids,
        geometry_types,
        zm_flags,
    })
}

fn validate_source_geometry_profile(
    source_id: &str,
    source: &SourceConfig,
    profile: &SourceGeometryProfile,
) -> Result<(), SourceValidationError> {
    if profile.row_count == 0 {
        return Ok(());
    }
    if profile.srids != [source.srid] {
        return Err(SourceValidationError::SridProfile {
            source_id: source_id.to_string(),
            expected: source.srid,
            found: profile.srids.clone(),
        });
    }
    let allowed = source
        .geometry_types
        .iter()
        .copied()
        .map(postgis_geometry_type)
        .collect::<BTreeSet<_>>();
    if profile
        .geometry_types
        .iter()
        .any(|geometry_type| !allowed.contains(geometry_type.as_str()))
    {
        return Err(SourceValidationError::GeometryTypeProfile {
            source_id: source_id.to_string(),
            allowed: allowed.into_iter().map(str::to_string).collect(),
            found: profile.geometry_types.clone(),
        });
    }
    let expected_zm_flag = match source.source_model {
        SourceModel::ExtrudedFootprint => 0,
        SourceModel::SurfaceGeometryZ => 2,
    };
    if profile.zm_flags != [expected_zm_flag] {
        return Err(SourceValidationError::CoordinateDimensionProfile {
            source_id: source_id.to_string(),
            expected_zm_flag,
            found: profile.zm_flags.clone(),
        });
    }
    Ok(())
}

fn postgis_geometry_type(geometry_type: GeometryType) -> &'static str {
    match geometry_type {
        GeometryType::Polygon | GeometryType::PolygonZ => "POLYGON",
        GeometryType::MultiPolygon | GeometryType::MultiPolygonZ => "MULTIPOLYGON",
    }
}

async fn validate_finite_coordinates(
    client: &impl GenericClient,
    source_id: &str,
    source: &SourceConfig,
) -> Result<(), SourceValidationError> {
    let schema = quote_identifier(&source.schema, "schema").map_err(|source| {
        SourceValidationError::Config {
            source_id: source_id.to_string(),
            source,
        }
    })?;
    let table = quote_identifier(&source.table, "table").map_err(|source| {
        SourceValidationError::Config {
            source_id: source_id.to_string(),
            source,
        }
    })?;
    let geometry =
        quote_identifier(&source.geometry_column, "geometry_column").map_err(|source| {
            SourceValidationError::Config {
                source_id: source_id.to_string(),
                source,
            }
        })?;
    let id = quote_identifier(&source.id_column, "id_column").map_err(|source| {
        SourceValidationError::Config {
            source_id: source_id.to_string(),
            source,
        }
    })?;
    let z_check = if source.source_model == SourceModel::SurfaceGeometryZ {
        "ST_Z(p.geom) IS NULL OR NOT (abs(ST_Z(p.geom)) < 'Infinity'::float8) OR"
    } else {
        ""
    };
    let sql = format!(
        "SELECT t.{id}::text \
         FROM {schema}.{table} AS t \
         CROSS JOIN LATERAL ST_DumpPoints(t.{geometry}) AS p \
         WHERE t.{geometry} IS NOT NULL AND NOT ST_IsEmpty(t.{geometry}) \
           AND ({z_check} \
                NOT (abs(ST_X(p.geom)) < 'Infinity'::float8) OR \
                NOT (abs(ST_Y(p.geom)) < 'Infinity'::float8)) \
         LIMIT 1"
    );
    if let Some(row) = client
        .query_opt(&sql, &[])
        .await
        .map_err(|error| SourceValidationError::database(source_id, "coordinate scan", error))?
    {
        return Err(SourceValidationError::NonFiniteCoordinate {
            source_id: source_id.to_string(),
            feature_id: row.get(0),
        });
    }
    Ok(())
}

async fn validate_surface_transform(
    client: &impl GenericClient,
    source_id: &str,
    source: &SourceConfig,
) -> Result<(), SourceValidationError> {
    let transform =
        surface_transform(source).map_err(|source_error| SourceValidationError::Config {
            source_id: source_id.to_string(),
            source: source_error,
        })?;
    let longitude = (source.bounds.west + source.bounds.east) / 2.0;
    let latitude = (source.bounds.south + source.bounds.north) / 2.0;
    let result = match transform {
        SurfaceTransform::Rdnaptrans2018Epsg1149 => {
            client
                .query_one(
                    &format!(
                        "WITH target_point AS ( \
                           SELECT ST_SetSRID(ST_MakePoint($1, $2, 0.0), {TARGET_GEODETIC_3D_SRID}) AS geom \
                         ), source_point AS ( \
                           SELECT ST_InverseTransformPipeline(geom, $3, $4::integer) AS geom FROM target_point \
                         ), roundtrip AS ( \
                           SELECT ST_TransformPipeline(geom, $3, {TARGET_GEODETIC_3D_SRID}) AS geom FROM source_point \
                         ) \
                         SELECT ST_X(geom), ST_Y(geom), ST_Z(geom), ST_SRID(geom), ST_Zmflag(geom)::integer \
                         FROM roundtrip"
                    ),
                    &[
                        &longitude,
                        &latitude,
                        &RDNAPTRANS2018_EPSG_1149_PIPELINE,
                        &source.srid,
                    ],
                )
                .await
        }
        SurfaceTransform::Identity => {
            client
                .query_one(
                    &format!(
                        "WITH roundtrip AS ( \
                           SELECT ST_SetSRID(ST_MakePoint($1, $2, 0.0), {TARGET_GEODETIC_3D_SRID}) AS geom \
                         ) \
                         SELECT ST_X(geom), ST_Y(geom), ST_Z(geom), ST_SRID(geom), ST_Zmflag(geom)::integer \
                         FROM roundtrip"
                    ),
                    &[&longitude, &latitude],
                )
                .await
        }
    }
    .map_err(|source_error| {
        map_surface_validation_error(
            source_id,
            source,
            transform,
            "transform probe",
            source_error,
        )
    })?;
    validate_transform_probe(source_id, &result)
}

async fn validate_surface_extent(
    client: &impl GenericClient,
    source_id: &str,
    source: &SourceConfig,
) -> Result<(), SourceValidationError> {
    let schema = quote_identifier(&source.schema, "schema").map_err(|source| {
        SourceValidationError::Config {
            source_id: source_id.to_string(),
            source,
        }
    })?;
    let table = quote_identifier(&source.table, "table").map_err(|source| {
        SourceValidationError::Config {
            source_id: source_id.to_string(),
            source,
        }
    })?;
    let geometry =
        quote_identifier(&source.geometry_column, "geometry_column").map_err(|source| {
            SourceValidationError::Config {
                source_id: source_id.to_string(),
                source,
            }
        })?;
    let transform =
        surface_transform(source).map_err(|source_error| SourceValidationError::Config {
            source_id: source_id.to_string(),
            source: source_error,
        })?;
    let aggregate = "SELECT min(ST_X(p.geom)), min(ST_Y(p.geom)), \
                            max(ST_X(p.geom)), max(ST_Y(p.geom)), \
                            min(ST_Z(p.geom)), max(ST_Z(p.geom))";
    let row = match transform {
        SurfaceTransform::Identity => {
            client
                .query_one(
                    &format!(
                        "{aggregate} \
                         FROM {schema}.{table} AS t \
                         CROSS JOIN LATERAL ST_DumpPoints(t.{geometry}) AS p"
                    ),
                    &[],
                )
                .await
        }
        SurfaceTransform::Rdnaptrans2018Epsg1149 => {
            client
                .query_one(
                    &format!(
                        "{aggregate} \
                         FROM {schema}.{table} AS t \
                         CROSS JOIN LATERAL ( \
                           SELECT ST_TransformPipeline(t.{geometry}, $1, {TARGET_GEODETIC_3D_SRID}) AS geom \
                         ) AS transformed \
                         CROSS JOIN LATERAL ST_DumpPoints(transformed.geom) AS p"
                    ),
                    &[&RDNAPTRANS2018_EPSG_1149_PIPELINE],
                )
                .await
        }
    }
    .map_err(|source_error| {
        map_surface_validation_error(
            source_id,
            source,
            transform,
            "transformed extent",
            source_error,
        )
    })?;

    let mut actual = [0.0; 6];
    for (index, value) in actual.iter_mut().enumerate() {
        *value = row.get::<_, Option<f64>>(index).ok_or_else(|| {
            SourceValidationError::TransformContract {
                source_id: source_id.to_string(),
                message: "non-empty source produced an empty transformed extent".to_string(),
            }
        })?;
    }
    validate_surface_extent_contract(source_id, &source.bounds, actual)
}

fn validate_surface_extent_contract(
    source_id: &str,
    bounds: &SourceBounds,
    actual: [f64; 6],
) -> Result<(), SourceValidationError> {
    const ANGULAR_TOLERANCE_DEG: f64 = 1.0e-9;
    const HEIGHT_TOLERANCE_M: f64 = 1.0e-6;

    let configured = [
        bounds.west,
        bounds.south,
        bounds.east,
        bounds.north,
        bounds.min_height_m,
        bounds.max_height_m,
    ];
    let outside = !actual.iter().all(|coordinate| coordinate.is_finite())
        || actual[0] < bounds.west - ANGULAR_TOLERANCE_DEG
        || actual[1] < bounds.south - ANGULAR_TOLERANCE_DEG
        || actual[2] > bounds.east + ANGULAR_TOLERANCE_DEG
        || actual[3] > bounds.north + ANGULAR_TOLERANCE_DEG
        || actual[4] < bounds.min_height_m - HEIGHT_TOLERANCE_M
        || actual[5] > bounds.max_height_m + HEIGHT_TOLERANCE_M;
    if outside {
        Err(SourceValidationError::TransformedExtentOutsideBounds {
            source_id: source_id.to_string(),
            configured,
            actual,
        })
    } else {
        Ok(())
    }
}

fn map_surface_validation_error(
    source_id: &str,
    source: &SourceConfig,
    transform: SurfaceTransform,
    stage: &'static str,
    source_error: tokio_postgres::Error,
) -> SourceValidationError {
    let is_transform_failure = source_error.as_db_error().is_some_and(|database_error| {
        is_coordinate_transform_failure(database_error.code().code(), database_error.message())
    });
    if is_transform_failure {
        SourceValidationError::TransformUnavailable {
            source_id: source_id.to_string(),
            source_srid: source.srid,
            target_srid: TARGET_GEODETIC_3D_SRID,
            operation: transform.operation_name(),
            source: source_error,
        }
    } else {
        SourceValidationError::Database {
            source_id: source_id.to_string(),
            stage,
            source: source_error,
        }
    }
}

fn validate_transform_probe(source_id: &str, row: &Row) -> Result<(), SourceValidationError> {
    let point = [
        row.get::<_, f64>(0),
        row.get::<_, f64>(1),
        row.get::<_, f64>(2),
    ];
    let srid = row.get::<_, i32>(3);
    let zm_flag = row.get::<_, i32>(4);
    if !point.iter().all(|coordinate| coordinate.is_finite())
        || srid != TARGET_GEODETIC_3D_SRID
        || zm_flag != 2
    {
        return Err(SourceValidationError::TransformContract {
            source_id: source_id.to_string(),
            message: format!(
                "probe returned coordinates {point:?}, SRID {srid}, ST_Zmflag {zm_flag}; expected finite EPSG:{TARGET_GEODETIC_3D_SRID} XYZ"
            ),
        });
    }
    Ok(())
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
    CoordinateTransform {
        source_srid: i32,
        target_srid: i32,
        operation: &'static str,
        source: tokio_postgres::Error,
    },
    SourceContract(String),
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
            TileQueryError::CoordinateTransform {
                source_srid,
                target_srid,
                operation,
                source,
            } => write!(
                f,
                "coordinate transform EPSG:{source_srid} -> EPSG:{target_srid} using {operation} failed: {source}"
            ),
            TileQueryError::SourceContract(message) => {
                write!(f, "source geometry contract violation: {message}")
            }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueryBindings {
    Standard,
    Rdnaptrans2018Epsg1149,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfaceTransform {
    Identity,
    Rdnaptrans2018Epsg1149,
}

impl SurfaceTransform {
    fn operation_name(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Rdnaptrans2018Epsg1149 => RDNAPTRANS2018_EPSG_1149_OPERATION,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct NormalizedGeometryQueryPlan {
    sql: String,
    attributes: Vec<String>,
    bindings: QueryBindings,
    source_model: SourceModel,
}

#[derive(Debug, PartialEq, Eq)]
struct SubtreeOccupancyQueryPlan {
    sql: String,
    bindings: QueryBindings,
    source_model: SourceModel,
}

#[derive(Debug, PartialEq, Eq)]
struct SurfaceSubtreeCandidatesQueryPlan {
    sql: String,
    bindings: QueryBindings,
}

impl NormalizedGeometryQueryPlan {
    fn map_query_error(
        &self,
        source: &SourceConfig,
        source_error: tokio_postgres::Error,
    ) -> TileQueryError {
        map_plan_query_error(self.source_model, self.bindings, source, source_error)
    }
}

impl SubtreeOccupancyQueryPlan {
    fn map_query_error(
        &self,
        source: &SourceConfig,
        source_error: tokio_postgres::Error,
    ) -> TileQueryError {
        map_plan_query_error(self.source_model, self.bindings, source, source_error)
    }
}

impl SurfaceSubtreeCandidatesQueryPlan {
    fn map_query_error(
        &self,
        source: &SourceConfig,
        source_error: tokio_postgres::Error,
    ) -> TileQueryError {
        map_plan_query_error(
            SourceModel::SurfaceGeometryZ,
            self.bindings,
            source,
            source_error,
        )
    }
}

fn map_plan_query_error(
    source_model: SourceModel,
    bindings: QueryBindings,
    source: &SourceConfig,
    source_error: tokio_postgres::Error,
) -> TileQueryError {
    let is_transform_failure = source_error.as_db_error().is_some_and(|database_error| {
        is_coordinate_transform_failure(database_error.code().code(), database_error.message())
    });
    if is_transform_failure {
        let (target_srid, operation) = match (source_model, bindings) {
            (SourceModel::ExtrudedFootprint, _) => (
                TARGET_GEOGRAPHIC_2D_SRID,
                POSTGIS_AUTOMATIC_TRANSFORM_OPERATION,
            ),
            (SourceModel::SurfaceGeometryZ, QueryBindings::Standard) => {
                (TARGET_GEODETIC_3D_SRID, "identity")
            }
            (SourceModel::SurfaceGeometryZ, QueryBindings::Rdnaptrans2018Epsg1149) => {
                (TARGET_GEODETIC_3D_SRID, RDNAPTRANS2018_EPSG_1149_OPERATION)
            }
        };
        TileQueryError::CoordinateTransform {
            source_srid: source.srid,
            target_srid,
            operation,
            source: source_error,
        }
    } else {
        TileQueryError::Postgres(source_error)
    }
}

fn is_coordinate_transform_failure(sqlstate: &str, message: &str) -> bool {
    if sqlstate != "XX000" {
        return false;
    }

    let message = message.to_ascii_lowercase();
    message.starts_with("transform:")
        || message.starts_with("could not parse coordinate operation")
        || message.starts_with("could not form projection")
}

fn build_normalized_geometry_query(
    source: &SourceConfig,
) -> Result<NormalizedGeometryQueryPlan, ConfigError> {
    if source.max_features_per_tile == 0 {
        return Err(ConfigError::Validation(
            "max_features_per_tile must be greater than zero".to_string(),
        ));
    }

    let schema = quote_identifier(&source.schema, "schema")?;
    let table = quote_identifier(&source.table, "table")?;
    let id_column = quote_identifier(&source.id_column, "id_column")?;
    let geometry_column = quote_identifier(&source.geometry_column, "geometry_column")?;

    let (
        geometry_select,
        source_boundary_select,
        tile_bbox,
        joins,
        predicate,
        limit_parameter,
        bindings,
    ) = match source.source_model {
        SourceModel::ExtrudedFootprint => {
            let table_geometry = format!("t.{geometry_column}");
            let clipped_geometry = clipped_geometry_expression(&table_geometry, "b.geom");
            let source_boundary = source_boundary_expression(&table_geometry, "b.geom");
            let predicate =
                positive_area_intersection_predicate(&table_geometry, "b.geom", "clipped.geom");
            (
                "ST_AsBinary(ST_Transform(clipped.geom, 4326), 'NDR') AS geometry_wkb".to_string(),
                "ST_AsBinary(ST_Transform(source_boundary.geom, 4326), 'NDR') AS source_boundary_wkb"
                    .to_string(),
                "ST_Transform(ST_MakeEnvelope($1, $2, $3, $4, 4326), $5::integer) AS geom"
                    .to_string(),
                format!(
                    "CROSS JOIN LATERAL (SELECT {clipped_geometry} AS geom) AS clipped \
                     CROSS JOIN LATERAL (SELECT {source_boundary} AS geom) AS source_boundary"
                ),
                predicate,
                Some("$6"),
                QueryBindings::Standard,
            )
        }
        SourceModel::SurfaceGeometryZ => match surface_transform(source)? {
            SurfaceTransform::Identity => (
                format!("ST_AsBinary(t.{geometry_column}, 'NDR') AS geometry_wkb"),
                "NULL::bytea AS source_boundary_wkb".to_string(),
                "ST_MakeEnvelope($1, $2, $3, $4, $5::integer) AS geom".to_string(),
                String::new(),
                format!("t.{geometry_column} && b.geom"),
                None,
                QueryBindings::Standard,
            ),
            SurfaceTransform::Rdnaptrans2018Epsg1149 => (
                format!(
                    "ST_AsBinary(ST_TransformPipeline(t.{geometry_column}, $6, {TARGET_GEODETIC_3D_SRID}), 'NDR') AS geometry_wkb"
                ),
                "NULL::bytea AS source_boundary_wkb".to_string(),
                format!(
                    "ST_Envelope(ST_Force2D(ST_InverseTransformPipeline(\
                         ST_Force3D(ST_MakeEnvelope($1, $2, $3, $4, {TARGET_GEODETIC_3D_SRID}), 0.0), \
                         $6, $5::integer))) AS geom"
                ),
                String::new(),
                format!("t.{geometry_column} && b.geom"),
                None,
                QueryBindings::Rdnaptrans2018Epsg1149,
            ),
        },
    };

    let mut select_columns = vec![
        format!("t.{id_column}::text AS id"),
        geometry_select,
        source_boundary_select,
    ];

    let query_attributes = source.content_query_attributes();
    let mut attributes = Vec::with_capacity(query_attributes.len());
    let mut quoted_attribute_columns = Vec::with_capacity(query_attributes.len());
    for (index, attribute) in query_attributes.iter().enumerate() {
        let attribute_column = quote_identifier(attribute, "attribute")?;
        select_columns.push(format!("t.{attribute_column}::text AS attr_{index}"));
        quoted_attribute_columns.push(attribute_column);
        attributes.push(attribute.clone());
    }

    let sql =
        if source.source_model == SourceModel::SurfaceGeometryZ
            && bindings == QueryBindings::Rdnaptrans2018Epsg1149
        {
            // Initializing the pinned PROJ pipeline once per source row
            // dominates dense content requests. Transform bounded collections
            // instead, then split them back into the exact per-feature rows
            // expected by the adapter. Keeping the batch CTE non-materialized
            // lets query_raw cancel remaining transforms after an exact
            // post-clip feature-limit failure.
            let candidate_attributes = comma_prefixed_sql(
                quoted_attribute_columns
                    .iter()
                    .enumerate()
                    .map(|(index, column)| format!("t.{column}::text AS attr_{index}")),
            );
            let batch_attributes =
                comma_prefixed_sql(quoted_attribute_columns.iter().enumerate().map(
                    |(index, _)| format!("array_agg(attr_{index} ORDER BY id) AS attr_{index}"),
                ));
            let output_attributes =
                comma_prefixed_sql(quoted_attribute_columns.iter().enumerate().map(
                    |(index, _)| format!("batch.attr_{index}[feature_index] AS attr_{index}"),
                ));

            format!(
                "WITH tile_bbox AS MATERIALIZED (SELECT {tile_bbox}), \
             candidate_rows AS MATERIALIZED ( \
               SELECT t.{id_column}::text AS id, \
                      t.{geometry_column} AS source_geom \
                      {candidate_attributes}, \
                      ((row_number() OVER (ORDER BY t.{id_column}::text) - 1) \
                        / {RDNAP_CONTENT_TRANSFORM_BATCH_SIZE})::bigint AS transform_batch \
               FROM {schema}.{table} AS t \
               CROSS JOIN tile_bbox AS b \
               WHERE {predicate} \
             ), candidate_batches AS NOT MATERIALIZED ( \
               SELECT transform_batch, \
                      array_agg(id ORDER BY id) AS feature_ids, \
                      ST_TransformPipeline( \
                        ST_Collect(source_geom ORDER BY id), \
                        $6, \
                        {TARGET_GEODETIC_3D_SRID} \
                      ) AS transformed_geom \
                      {batch_attributes} \
               FROM candidate_rows \
               GROUP BY transform_batch \
               ORDER BY transform_batch \
             ) \
             SELECT batch.feature_ids[feature_index] AS id, \
                    ST_AsBinary( \
                      ST_GeometryN(batch.transformed_geom, feature_index), \
                      'NDR' \
                    ) AS geometry_wkb, \
                    NULL::bytea AS source_boundary_wkb \
                    {output_attributes} \
             FROM candidate_batches AS batch \
             CROSS JOIN LATERAL \
                  generate_subscripts(batch.feature_ids, 1) AS feature(feature_index)"
            )
        } else {
            let order_and_limit = limit_parameter.map_or_else(String::new, |parameter| {
                format!("ORDER BY t.{id_column} LIMIT {parameter}")
            });
            format!(
                "WITH tile_bbox AS (SELECT {tile_bbox}) \
             SELECT {} \
             FROM {schema}.{table} AS t \
             CROSS JOIN tile_bbox AS b \
             {joins} \
             WHERE {predicate} \
             {order_and_limit}",
                select_columns.join(", ")
            )
        };

    Ok(NormalizedGeometryQueryPlan {
        sql,
        attributes,
        bindings,
        source_model: source.source_model,
    })
}

fn comma_prefixed_sql(parts: impl IntoIterator<Item = String>) -> String {
    let joined = parts.into_iter().collect::<Vec<_>>().join(", ");
    if joined.is_empty() {
        joined
    } else {
        format!(", {joined}")
    }
}

fn build_subtree_occupancy_query(
    source: &SourceConfig,
) -> Result<SubtreeOccupancyQueryPlan, ConfigError> {
    if source.source_model != SourceModel::ExtrudedFootprint {
        return Err(ConfigError::Validation(
            "bbox-only subtree occupancy requires source_model = extruded_footprint".to_string(),
        ));
    }
    let schema = quote_identifier(&source.schema, "schema")?;
    let table = quote_identifier(&source.table, "table")?;
    let geometry_column = quote_identifier(&source.geometry_column, "geometry_column")?;
    let table_geometry = format!("t.{geometry_column}");
    let clipped_geometry = clipped_geometry_expression(&table_geometry, "q.geom");
    let bbox_expression =
        "ST_Transform(ST_MakeEnvelope(u.west, u.south, u.east, u.north, 4326), $5::integer)";
    let joins = format!("CROSS JOIN LATERAL (SELECT {clipped_geometry} AS geom) AS clipped");
    let predicate = positive_area_intersection_predicate(&table_geometry, "q.geom", "clipped.geom");

    let sql = format!(
        "WITH requested_tiles AS ( \
           SELECT (u.ordinality - 1)::bigint AS slot, \
                  {bbox_expression} AS geom \
           FROM unnest($1::float8[], $2::float8[], $3::float8[], $4::float8[]) \
                WITH ORDINALITY AS u(west, south, east, north, ordinality) \
         ) \
         SELECT q.slot, ( \
           SELECT count(*)::bigint \
           FROM ( \
             SELECT 1 \
             FROM {schema}.{table} AS t \
             {joins} \
             WHERE {predicate} \
             LIMIT $6 \
           ) AS capped \
         ) AS feature_count \
         FROM requested_tiles AS q \
         ORDER BY q.slot"
    );

    Ok(SubtreeOccupancyQueryPlan {
        sql,
        bindings: QueryBindings::Standard,
        source_model: SourceModel::ExtrudedFootprint,
    })
}

fn build_surface_subtree_count_query(
    source: &SourceConfig,
) -> Result<SurfaceSubtreeCandidatesQueryPlan, ConfigError> {
    if source.source_model != SourceModel::SurfaceGeometryZ {
        return Err(ConfigError::Validation(
            "surface subtree counts require source_model = surface_geometry_z".to_string(),
        ));
    }
    let schema = quote_identifier(&source.schema, "schema")?;
    let table = quote_identifier(&source.table, "table")?;
    let geometry_column = quote_identifier(&source.geometry_column, "geometry_column")?;
    let (bbox_expression, bindings) = match surface_transform(source)? {
        SurfaceTransform::Identity => (
            "ST_MakeEnvelope(u.west, u.south, u.east, u.north, $6::integer)".to_string(),
            QueryBindings::Standard,
        ),
        SurfaceTransform::Rdnaptrans2018Epsg1149 => (
            format!(
                "ST_Force2D(ST_InverseTransformPipeline(\
                 ST_Force3D(ST_Segmentize(ST_MakeEnvelope(u.west, u.south, u.east, u.north, {TARGET_GEODETIC_3D_SRID}), 0.001), 0.0), \
                 $7, $6::integer))"
            ),
            QueryBindings::Rdnaptrans2018Epsg1149,
        ),
    };
    let sql = format!(
        "WITH source_tiles AS MATERIALIZED ( \
           SELECT (u.ordinality - 1)::bigint AS slot, \
                  u.target, \
                  {bbox_expression} AS geom \
           FROM unnest($1::float8[], $2::float8[], $3::float8[], $4::float8[], $5::bigint[]) \
                WITH ORDINALITY AS u(west, south, east, north, target, ordinality) \
         ), requested_tiles AS MATERIALIZED ( \
           SELECT slot, target, ST_Envelope(geom) AS candidate_geom, \
                  ST_Buffer(\
                    geom, \
                    -ST_Distance(ST_PointOnSurface(geom), ST_Boundary(geom)) * 0.001\
                  ) AS contained_geom \
           FROM source_tiles \
         ) \
         SELECT q.slot, \
                CASE WHEN contained.feature_count = q.target THEN q.target ELSE ( \
                  SELECT count(*)::bigint \
                  FROM ( \
                    SELECT 1 \
                    FROM {schema}.{table} AS t \
                    WHERE t.{geometry_column} && q.candidate_geom \
                    LIMIT q.target \
                  ) AS bbox_features \
                ) END AS bbox_count, \
                contained.feature_count \
         FROM requested_tiles AS q \
         CROSS JOIN LATERAL ( \
           SELECT count(*)::bigint AS feature_count \
           FROM ( \
             SELECT 1 \
             FROM {schema}.{table} AS t \
             WHERE t.{geometry_column} && q.contained_geom \
               AND ST_CoveredBy(ST_Envelope(t.{geometry_column}), q.contained_geom) \
             LIMIT q.target \
           ) AS matching_features \
         ) AS contained \
         ORDER BY q.slot"
    );
    Ok(SurfaceSubtreeCandidatesQueryPlan { sql, bindings })
}

fn build_surface_subtree_exact_candidates_query(
    source: &SourceConfig,
) -> Result<SurfaceSubtreeCandidatesQueryPlan, ConfigError> {
    if source.source_model != SourceModel::SurfaceGeometryZ {
        return Err(ConfigError::Validation(
            "surface subtree candidates require source_model = surface_geometry_z".to_string(),
        ));
    }
    let schema = quote_identifier(&source.schema, "schema")?;
    let table = quote_identifier(&source.table, "table")?;
    let id_column = quote_identifier(&source.id_column, "id_column")?;
    let geometry_column = quote_identifier(&source.geometry_column, "geometry_column")?;
    let fringe_predicate = if source.surface_subtree_envelope_shortcut {
        format!("AND NOT ST_CoveredBy(ST_Envelope(t.{geometry_column}), q.contained_geom)")
    } else {
        String::new()
    };
    let contained_geom_projection = if source.surface_subtree_envelope_shortcut {
        ", ST_Buffer(\
           geom, \
           -ST_Distance(ST_PointOnSurface(geom), ST_Boundary(geom)) * 0.001\
         ) AS contained_geom"
    } else {
        ""
    };
    let transform = surface_transform(source)?;
    let (bbox_expression, bindings) = match transform {
        SurfaceTransform::Identity => (
            "ST_MakeEnvelope(u.west, u.south, u.east, u.north, $7::integer)".to_string(),
            QueryBindings::Standard,
        ),
        SurfaceTransform::Rdnaptrans2018Epsg1149 => (
            format!(
                "ST_Force2D(ST_InverseTransformPipeline(\
                     ST_Force3D(ST_Segmentize(ST_MakeEnvelope(u.west, u.south, u.east, u.north, {TARGET_GEODETIC_3D_SRID}), 0.001), 0.0), \
                     $8, $7::integer))"
            ),
            QueryBindings::Rdnaptrans2018Epsg1149,
        ),
    };
    let requested_tiles_ctes = format!(
        "WITH source_tiles AS MATERIALIZED ( \
           SELECT (u.ordinality - 1)::bigint AS slot, \
                  u.candidate_limit, u.candidate_offset, \
                  {bbox_expression} AS geom \
           FROM unnest($1::float8[], $2::float8[], $3::float8[], $4::float8[], $5::bigint[], $6::bigint[]) \
                WITH ORDINALITY AS u(west, south, east, north, candidate_limit, candidate_offset, ordinality) \
         ), requested_tiles AS MATERIALIZED ( \
           SELECT slot, candidate_limit, candidate_offset, \
                  ST_Envelope(geom) AS candidate_geom \
                  {contained_geom_projection} \
           FROM source_tiles \
         )"
    );
    let candidate_rows = format!(
        "SELECT q.slot, candidate.id, candidate.source_geom \
         FROM requested_tiles AS q \
         CROSS JOIN LATERAL ( \
           SELECT t.{id_column}::text AS id, t.{geometry_column} AS source_geom \
           FROM {schema}.{table} AS t \
           WHERE t.{geometry_column} && q.candidate_geom \
             {fringe_predicate} \
           ORDER BY t.{id_column} \
           LIMIT q.candidate_limit OFFSET q.candidate_offset \
         ) AS candidate"
    );
    let sql = match transform {
        SurfaceTransform::Identity => format!(
            "{requested_tiles_ctes} \
             SELECT q.slot, candidate.id, \
                    ST_AsBinary(candidate.source_geom, 'NDR') AS geometry_wkb \
             FROM requested_tiles AS q \
             CROSS JOIN LATERAL ( \
               SELECT t.{id_column}::text AS id, t.{geometry_column} AS source_geom \
               FROM {schema}.{table} AS t \
               WHERE t.{geometry_column} && q.candidate_geom \
                 {fringe_predicate} \
               ORDER BY t.{id_column} \
               LIMIT q.candidate_limit OFFSET q.candidate_offset \
             ) AS candidate \
             ORDER BY q.slot, candidate.id"
        ),
        SurfaceTransform::Rdnaptrans2018Epsg1149 => format!(
            "{requested_tiles_ctes}, \
             candidate_rows AS MATERIALIZED ( \
               {candidate_rows} \
             ), candidate_batches AS MATERIALIZED ( \
               SELECT slot, \
                      array_agg(id ORDER BY id) AS feature_ids, \
                      ST_TransformPipeline( \
                        ST_Collect(source_geom ORDER BY id), \
                        $8, \
                        {TARGET_GEODETIC_3D_SRID} \
                      ) AS transformed_geom \
               FROM candidate_rows \
               GROUP BY slot \
             ) \
             SELECT batch.slot, batch.feature_ids[feature_index], \
                    ST_AsBinary( \
                      ST_GeometryN(batch.transformed_geom, feature_index), \
                      'NDR' \
                    ) AS geometry_wkb \
             FROM candidate_batches AS batch \
             CROSS JOIN LATERAL \
                  generate_subscripts(batch.feature_ids, 1) AS feature(feature_index) \
             ORDER BY batch.slot, batch.feature_ids[feature_index]"
        ),
    };
    Ok(SurfaceSubtreeCandidatesQueryPlan { sql, bindings })
}

fn surface_transform(source: &SourceConfig) -> Result<SurfaceTransform, ConfigError> {
    Ok(match source.coordinate_operation {
        None => SurfaceTransform::Identity,
        Some(CoordinateOperation::Rdnaptrans2018Epsg1149) => {
            SurfaceTransform::Rdnaptrans2018Epsg1149
        }
    })
}

fn clipped_geometry_expression(table_geometry: &str, bbox_geometry: &str) -> String {
    format!("ST_Multi(ST_CollectionExtract(ST_Intersection({table_geometry}, {bbox_geometry}), 3))")
}

fn source_boundary_expression(table_geometry: &str, bbox_geometry: &str) -> String {
    format!(
        "ST_Multi(ST_CollectionExtract(ST_Intersection(ST_Boundary({table_geometry}), {bbox_geometry}), 2))"
    )
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
    use lucy_core::geometry::NormalizedGeometry;
    use lucy_core::mesh::{MeshFrame, TriangleMesh, footprint_fragment_to_extruded_mesh};
    use lucy_core::source::SourceCatalog;
    use lucy_core::subtree::{
        generate_subtree_bytes_with_availability, pack_availability_bits, subtree_layout,
    };

    fn fixture_catalog() -> SourceCatalog {
        let config_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/fixture-sources.yaml");
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

    fn surface_source() -> SourceConfig {
        let mut source = fixture_source();
        source.table = "surface_buildings_7415".to_string();
        source.srid = 7415;
        source.source_model = SourceModel::SurfaceGeometryZ;
        source.coordinate_operation = Some(CoordinateOperation::Rdnaptrans2018Epsg1149);
        source.base_height_column = None;
        source.height_column = None;
        source.geometry_types = vec![GeometryType::PolygonZ, GeometryType::MultiPolygonZ];
        source.max_level = 2;
        source.attributes = vec!["name".to_string()];
        source.bounds.west = 5.86;
        source.bounds.south = 50.97;
        source.bounds.east = 5.90;
        source.bounds.north = 51.00;
        source.bounds.min_height_m = 35.0;
        source.bounds.max_height_m = 100.0;
        source
    }

    fn footprint_feature_mesh(feature: &NormalizedFeature, frame: MeshFrame) -> TriangleMesh {
        let NormalizedGeometry::GeographicFootprint(fragment) = &feature.geometry else {
            panic!("expected normalized footprint feature")
        };
        footprint_fragment_to_extruded_mesh(fragment, frame, 0.0, 10.0)
            .expect("normalized footprint fragment should mesh")
    }

    #[test]
    fn normalized_geometry_query_returns_clipped_caps_and_original_boundary_mask() {
        let source = fixture_source();
        let plan = build_normalized_geometry_query(&source).expect("query should build");

        assert!(
            plan.sql
                .contains("ST_Transform(ST_MakeEnvelope($1, $2, $3, $4, 4326), $5::integer)")
        );
        assert!(plan.sql.contains("t.\"geom\" && b.geom"));
        assert!(plan.sql.contains("ST_Intersects(t.\"geom\", b.geom)"));
        assert!(plan.sql.contains("ST_Intersection(t.\"geom\", b.geom)"));
        assert!(plan.sql.contains("ST_CollectionExtract"));
        assert!(plan.sql.contains("ST_Area(clipped.geom) > 0"));
        assert!(
            plan.sql
                .contains("ST_AsBinary(ST_Transform(clipped.geom, 4326), 'NDR')")
        );
        assert!(
            plan.sql
                .contains("ST_Intersection(ST_Boundary(t.\"geom\"), b.geom)")
        );
        assert!(plan.sql.contains("ST_CollectionExtract("));
        assert!(plan.sql.contains(", 2)) AS geom) AS source_boundary"));
        assert!(plan.sql.contains(
            "ST_AsBinary(ST_Transform(source_boundary.geom, 4326), 'NDR') AS source_boundary_wkb"
        ));
        assert!(!plan.sql.contains("ST_Boundary(clipped.geom)"));
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
    fn projected_footprint_query_normalizes_adapter_output_to_epsg_4326() {
        let mut source = fixture_source();
        source.srid = 28992;

        let plan = build_normalized_geometry_query(&source).expect("query should build");

        assert!(
            plan.sql
                .contains("ST_Transform(ST_MakeEnvelope($1, $2, $3, $4, 4326), $5::integer)")
        );
        assert!(
            plan.sql
                .contains("ST_AsBinary(ST_Transform(clipped.geom, 4326), 'NDR')")
        );
        assert!(
            plan.sql
                .contains("ST_AsBinary(ST_Transform(source_boundary.geom, 4326), 'NDR')")
        );
        assert!(
            plan.sql
                .contains("ST_Intersection(ST_Boundary(t.\"geom\"), b.geom)")
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
    fn subtree_query_omits_ancestor_only_slots_and_uses_boolean_child_targets() {
        let mut source = surface_source();
        source.max_level = 7;
        source.subtree_levels = 4;
        source.tileset.content_start_level = 6;
        source.max_features_per_tile = 1_000;

        let root = TileCoord::root();
        let layout = subtree_layout(&source, root).expect("root subtree layout");
        let slots = subtree_query_slots(&source, &layout);
        assert_eq!(slots.len(), 256);
        assert!(slots.iter().all(|slot| matches!(
            slot,
            SubtreeQuerySlot::ChildSubtree { tile, .. } if tile.level == 4
        )));
        assert!(
            slots
                .iter()
                .all(|slot| subtree_slot_feature_count_target(&source, *slot) == 1)
        );

        let availability =
            availability_from_feature_counts(&source, root, &layout, &slots, &vec![1; slots.len()])
                .expect("occupied child roots should close over local ancestors");
        assert!(availability.tile.iter().all(|available| *available));
        assert!(availability.content.iter().all(|available| !*available));
        assert!(
            availability
                .child_subtree
                .iter()
                .all(|available| *available)
        );

        let level_four = TileCoord::new(4, 0, 0).expect("level-four subtree root");
        let layout = subtree_layout(&source, level_four).expect("final subtree layout");
        let slots = subtree_query_slots(&source, &layout);
        assert_eq!(slots.len(), 80);
        assert!(slots.iter().all(|slot| matches!(
            slot,
            SubtreeQuerySlot::Tile { tile, .. } if tile.level >= 6
        )));
        assert!(slots.iter().all(|slot| {
            subtree_slot_feature_count_target(&source, *slot)
                == u64::from(source.max_features_per_tile) + 1
        }));
    }

    #[test]
    fn surface_count_bounds_skip_proven_slots_and_preserve_exact_lower_bounds() {
        for (bbox, contained, expected_target, expected_count, expected_exact) in [
            (0, 0, 1_001, 0, false),
            (1_001, 1_001, 1_001, 1_001, false),
            (825, 385, 1_001, 385, false),
            (825, 0, 1, 0, true),
            (1_001, 674, 1_001, 674, true),
        ] {
            let mut target = 1_001;
            let mut count = 0;
            let exact = classify_surface_subtree_count_bound(
                &mut target,
                &mut count,
                bbox,
                contained,
                true,
            );
            assert_eq!(target, expected_target);
            assert_eq!(count, expected_count);
            assert_eq!(exact, expected_exact);
        }

        let mut target = 1;
        let mut count = 0;
        assert!(classify_surface_subtree_count_bound(
            &mut target,
            &mut count,
            1,
            0,
            true,
        ));
        assert_eq!((target, count), (1, 0));

        assert!(!classify_surface_subtree_count_bound(
            &mut target,
            &mut count,
            1,
            1,
            true,
        ));
        assert_eq!((target, count), (1, 1));

        let mut target = 1_001;
        let mut count = 0;
        assert!(classify_surface_subtree_count_bound(
            &mut target,
            &mut count,
            825,
            385,
            false,
        ));
        assert_eq!((target, count), (1_001, 0));

        let mut target = 1_001;
        assert!(classify_surface_subtree_count_bound(
            &mut target,
            &mut count,
            1_001,
            1_001,
            false,
        ));
        assert_eq!((target, count), (1_001, 0));

        let mut target = 1;
        assert!(classify_surface_subtree_count_bound(
            &mut target,
            &mut count,
            1,
            1,
            false,
        ));
        assert_eq!((target, count), (1, 0));
    }

    #[test]
    fn surface_query_batches_candidates_through_the_explicit_3d_transform() {
        let source = surface_source();
        let plan = build_normalized_geometry_query(&source).expect("surface query should build");

        assert_eq!(plan.bindings, QueryBindings::Rdnaptrans2018Epsg1149);
        assert!(RDNAPTRANS2018_EPSG_1149_PIPELINE.contains("+proj=cart +ellps=GRS80"));
        assert!(RDNAPTRANS2018_EPSG_1149_PIPELINE.contains("+proj=helmert +x=0 +y=0 +z=0"));
        assert!(RDNAPTRANS2018_EPSG_1149_PIPELINE.contains("+inv +proj=cart +ellps=WGS84"));
        assert!(plan.sql.contains("ST_InverseTransformPipeline"));
        assert!(plan.sql.contains("/ 64)::bigint AS transform_batch"));
        assert!(plan.sql.contains("ST_Collect(source_geom ORDER BY id)"));
        assert!(plan.sql.contains("ST_TransformPipeline("));
        assert!(plan.sql.contains("$6, 4979"));
        assert!(
            plan.sql
                .contains("ST_GeometryN(batch.transformed_geom, feature_index)")
        );
        assert!(
            plan.sql
                .contains("generate_subscripts(batch.feature_ids, 1)")
        );
        assert!(plan.sql.contains("array_agg(attr_0 ORDER BY id) AS attr_0"));
        assert!(plan.sql.contains("batch.attr_0[feature_index] AS attr_0"));
        assert!(
            !plan
                .sql
                .contains("ST_TransformPipeline(t.\"geom\", $6, 4979)")
        );
        assert!(plan.sql.contains("t.\"geom\" && b.geom"));
        assert!(!plan.sql.contains("LIMIT"));
        assert!(
            plan.sql
                .contains("GROUP BY transform_batch ORDER BY transform_batch")
        );
        assert!(!plan.sql.contains("ST_Intersection"));
        assert!(!plan.sql.contains("ST_Intersects"));
        assert!(!plan.sql.contains("ST_Area"));
        assert!(!plan.sql.contains("ST_IsValid"));
        assert!(plan.sql.contains("NULL::bytea AS source_boundary_wkb"));
        assert!(!plan.sql.contains("ST_Boundary"));
        assert_eq!(plan.attributes, vec!["name", "color"]);
    }

    #[test]
    fn already_normalized_surface_query_omits_database_transform() {
        let mut source = surface_source();
        source.srid = 4979;
        source.coordinate_operation = None;

        let plan = build_normalized_geometry_query(&source).expect("identity query should build");

        assert_eq!(plan.bindings, QueryBindings::Standard);
        assert!(plan.sql.contains("ST_AsBinary(t.\"geom\", 'NDR')"));
        assert!(plan.sql.contains("NULL::bytea AS source_boundary_wkb"));
        assert!(
            plan.sql
                .contains("ST_MakeEnvelope($1, $2, $3, $4, $5::integer)")
        );
        assert!(!plan.sql.contains("ST_Transform("));
        assert!(!plan.sql.contains("ST_TransformPipeline"));
    }

    #[test]
    fn only_known_postgis_proj_failures_map_to_crs_errors() {
        assert!(is_coordinate_transform_failure(
            "XX000",
            "could not parse coordinate operation '+proj=pipeline ...'"
        ));
        assert!(is_coordinate_transform_failure(
            "XX000",
            "transform: Coordinate to transform falls outside grid (2052)"
        ));
        assert!(!is_coordinate_transform_failure(
            "XX000",
            "cache lookup failed for relation 42"
        ));
        assert!(!is_coordinate_transform_failure(
            "57014",
            "canceling statement due to statement timeout"
        ));
    }

    #[test]
    fn metadata_contract_accepts_specific_and_generic_surface_typmods() {
        let source = surface_source();
        for declared_geometry_type in [Some("MultiPolygonZ"), Some("GeometryZ"), None] {
            let profile = SourceMetadataProfile {
                declared_geometry_type: declared_geometry_type.map(str::to_string),
                declared_srid: Some(7415),
                declared_dimensions: Some(3),
                id_not_null: true,
                geometry_not_null: false,
                id_unique: true,
            };

            validate_declared_geometry_contract("surface", &source, &profile)
                .expect("compatible metadata should satisfy the declared contract");
        }
    }

    #[test]
    fn metadata_contract_rejects_declared_srid_type_and_dimension_mismatches() {
        let source = surface_source();
        for profile in [
            SourceMetadataProfile {
                declared_geometry_type: Some("PointZ".to_string()),
                declared_srid: Some(7415),
                declared_dimensions: Some(3),
                id_not_null: true,
                geometry_not_null: true,
                id_unique: true,
            },
            SourceMetadataProfile {
                declared_geometry_type: Some("MultiPolygonZ".to_string()),
                declared_srid: Some(4979),
                declared_dimensions: Some(3),
                id_not_null: true,
                geometry_not_null: true,
                id_unique: true,
            },
            SourceMetadataProfile {
                declared_geometry_type: Some("MultiPolygonZ".to_string()),
                declared_srid: Some(7415),
                declared_dimensions: Some(2),
                id_not_null: true,
                geometry_not_null: true,
                id_unique: true,
            },
        ] {
            assert!(matches!(
                validate_declared_geometry_contract("surface", &source, &profile),
                Err(SourceValidationError::DeclaredGeometryContract { .. })
            ));
        }
    }

    #[test]
    fn bbox_only_subtree_query_rejects_native_surfaces() {
        let source = surface_source();
        let error = build_subtree_occupancy_query(&source)
            .expect_err("surface availability must use exact core clipping");
        assert!(error.to_string().contains("extruded_footprint"));
    }

    #[test]
    fn surface_subtree_counts_use_capped_bbox_and_containment_bounds_without_geometry_output() {
        let source = surface_source();
        let plan =
            build_surface_subtree_count_query(&source).expect("surface count query should build");

        assert_eq!(plan.bindings, QueryBindings::Rdnaptrans2018Epsg1149);
        assert!(plan.sql.contains("AS MATERIALIZED"));
        assert!(plan.sql.contains("$5::bigint[]"));
        assert!(plan.sql.contains("t.\"geom\" && q.candidate_geom"));
        assert!(plan.sql.contains("ST_PointOnSurface"));
        assert!(plan.sql.contains("ST_Distance"));
        assert!(plan.sql.contains("ST_Buffer"));
        assert!(plan.sql.contains("* 0.001"));
        assert!(
            plan.sql
                .contains("ST_CoveredBy(ST_Envelope(t.\"geom\"), q.contained_geom)")
        );
        assert!(plan.sql.contains("ST_Segmentize"));
        assert!(plan.sql.contains("LIMIT q.target"));
        assert!(plan.sql.contains("count(*)::bigint"));
        assert!(!plan.sql.contains("ST_AsBinary"));
        assert!(!plan.sql.contains("ST_Relate"));
        assert!(!plan.sql.contains("ST_TransformPipeline(t.\"geom\""));
    }

    #[test]
    fn exact_surface_subtree_candidates_exclude_contained_lower_bound() {
        let mut source = surface_source();
        source.surface_subtree_envelope_shortcut = true;
        let plan = build_surface_subtree_exact_candidates_query(&source)
            .expect("exact surface candidate query should build");

        assert_eq!(plan.bindings, QueryBindings::Rdnaptrans2018Epsg1149);
        assert!(plan.sql.contains("AS MATERIALIZED"));
        assert!(plan.sql.contains("t.\"geom\" && q.candidate_geom"));
        assert!(
            plan.sql
                .contains("NOT ST_CoveredBy(ST_Envelope(t.\"geom\"), q.contained_geom)")
        );
        assert!(plan.sql.contains("ORDER BY t.\"id\""));
        assert!(plan.sql.contains("$5::bigint[], $6::bigint[]"));
        assert!(
            plan.sql
                .contains("LIMIT q.candidate_limit OFFSET q.candidate_offset")
        );
        assert!(
            plan.sql
                .contains("array_agg(id ORDER BY id) AS feature_ids")
        );
        assert!(plan.sql.contains("ST_Collect(source_geom ORDER BY id)"));
        assert!(plan.sql.contains("ST_TransformPipeline("));
        assert!(
            plan.sql
                .contains("ST_GeometryN(batch.transformed_geom, feature_index)")
        );
        assert!(
            plan.sql
                .contains("generate_subscripts(batch.feature_ids, 1)")
        );
        assert!(
            !plan
                .sql
                .contains("ST_TransformPipeline(candidate.source_geom")
        );
        assert!(!plan.sql.contains("requested_extent"));
    }

    #[test]
    fn identity_surface_subtree_candidates_keep_direct_per_row_wkb() {
        let mut source = surface_source();
        source.srid = TARGET_GEODETIC_3D_SRID;
        source.coordinate_operation = None;
        let plan = build_surface_subtree_exact_candidates_query(&source)
            .expect("identity surface candidate query should build");

        assert_eq!(plan.bindings, QueryBindings::Standard);
        assert!(
            plan.sql
                .contains("ST_AsBinary(candidate.source_geom, 'NDR')")
        );
        assert!(!plan.sql.contains("ST_Collect"));
        assert!(!plan.sql.contains("array_agg"));
        assert!(!plan.sql.contains("ST_TransformPipeline"));
        assert!(!plan.sql.contains("ST_GeometryN"));
    }

    #[test]
    fn exact_surface_subtree_candidates_include_contained_for_unaudited_sources() {
        let source = surface_source();
        let plan = build_surface_subtree_exact_candidates_query(&source)
            .expect("exact surface candidate query should build");

        assert!(plan.sql.contains("t.\"geom\" && q.candidate_geom"));
        assert!(
            !plan
                .sql
                .contains("NOT ST_CoveredBy(ST_Envelope(t.\"geom\"), q.contained_geom)")
        );
        assert!(!plan.sql.contains("AS contained_geom"));
        assert!(!plan.sql.contains("ST_Buffer"));
    }

    #[test]
    fn availability_closes_child_subtree_occupancy_over_local_ancestors() {
        let mut source = fixture_source();
        source.subtree_levels = 2;
        source.max_level = 4;
        let subtree_root = TileCoord::root();
        let layout = subtree_layout(&source, subtree_root).expect("subtree layout");
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
        let mut feature_counts = vec![0_u64; slots.len()];
        let child_slot = slots
            .iter()
            .position(|slot| matches!(slot, SubtreeQuerySlot::ChildSubtree { index: 0, .. }))
            .expect("southwest child subtree slot");
        feature_counts[child_slot] = 1;

        let availability = availability_from_feature_counts(
            &source,
            subtree_root,
            &layout,
            &slots,
            &feature_counts,
        )
        .expect("availability should build");

        assert!(availability.child_subtree[0]);
        let southwest_parent = TileCoord::new(1, 0, 0).expect("southwest parent");
        let parent_index = layout
            .local_tiles
            .iter()
            .position(|tile| *tile == Some(southwest_parent))
            .expect("parent availability index");
        assert!(availability.tile[parent_index]);
        assert!(!availability.content[parent_index]);
    }

    #[test]
    fn surface_profile_requires_xyz_but_allows_polygon_and_multipolygon() {
        let source = surface_source();
        let valid = SourceGeometryProfile {
            row_count: 2,
            srids: vec![7415],
            geometry_types: vec!["MULTIPOLYGON".to_string(), "POLYGON".to_string()],
            zm_flags: vec![2],
        };
        validate_source_geometry_profile("surface", &source, &valid)
            .expect("configured PolygonZ and MultiPolygonZ should pass");

        let no_z = SourceGeometryProfile {
            zm_flags: vec![0],
            ..valid
        };
        assert!(matches!(
            validate_source_geometry_profile("surface", &source, &no_z),
            Err(SourceValidationError::CoordinateDimensionProfile { .. })
        ));
    }

    #[test]
    fn transformed_surface_extent_must_fit_the_root_region() {
        let source = surface_source();
        validate_surface_extent_contract(
            "surface",
            &source.bounds,
            [5.861, 50.971, 5.899, 50.999, 40.0, 90.0],
        )
        .expect("contained transformed extent should pass");

        for actual in [
            [5.85, 50.971, 5.899, 50.999, 40.0, 90.0],
            [5.861, 50.971, 5.899, 50.999, 34.0, 90.0],
            [5.861, 50.971, 5.91, 50.999, 40.0, 90.0],
            [5.861, 50.971, 5.899, 50.999, 40.0, 101.0],
        ] {
            assert!(matches!(
                validate_surface_extent_contract("surface", &source.bounds, actual),
                Err(SourceValidationError::TransformedExtentOutsideBounds { .. })
            ));
        }
    }

    #[test]
    fn normalized_geometry_query_rejects_unsafe_attribute_identifiers() {
        let mut source = fixture_source();
        source.attributes.push("name; DROP TABLE x".to_string());

        let error =
            build_normalized_geometry_query(&source).expect_err("unsafe attribute should fail");
        assert!(
            error.to_string().contains("attribute"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn normalized_geometry_query_includes_configured_height_columns() {
        let mut source = fixture_source();
        source.attributes = vec!["name".to_string()];
        source.base_height_column = Some("custom_base_m".to_string());
        source.height_column = Some("custom_height_m".to_string());

        let plan = build_normalized_geometry_query(&source).expect("query should build");

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
    #[ignore = "requires DATABASE_URL and the PostGIS fixtures"]
    async fn fixture_tile_query_omits_clip_walls_and_rejects_overflow() {
        let database_url = env::var("DATABASE_URL")
            .expect("DATABASE_URL is required for the ignored PostGIS integration test");

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
        // This integration test exercises deep footprint clipping and child
        // subtrees independently of the shallower demo catalog setting.
        source.max_level = 16;
        let profile = validate_source(&client, "poc_buildings", &source)
            .await
            .expect("extruded fixture source should satisfy introspection");
        assert_eq!(profile.row_count, 6);
        assert_eq!(profile.srids, vec![4326]);
        assert_eq!(profile.geometry_types, vec!["MULTIPOLYGON"]);
        assert_eq!(profile.zm_flags, vec![0]);
        let root_features = query_normalized_features(&client, &source, TileCoord::root())
            .await
            .expect("root tile should query");
        assert_eq!(root_features.len(), 6);
        assert!(root_features.iter().all(|feature| {
            feature.encoded_size_bytes > 0
                && matches!(
                    &feature.geometry,
                    NormalizedGeometry::GeographicFootprint(fragment)
                        if !fragment.source_boundary.lines.is_empty()
                )
        }));
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

        let mut catalog = fixture_catalog();
        catalog
            .sources
            .get_mut("poc_buildings")
            .expect("fixture catalog source")
            .max_level = 16;
        let app = crate::server::build_app(catalog).expect("fixture app should build");
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
            content_document["meshes"][0]["primitives"][0]["attributes"]["NORMAL"],
            2
        );
        assert_eq!(
            content_document["meshes"][0]["primitives"][0]["attributes"]["COLOR_0"],
            3
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
            .clone()
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
        let empty_features = query_normalized_features(&client, &source, empty_tile)
            .await
            .expect("empty tile should query");
        assert!(empty_features.is_empty());

        let southwest_tile = TileCoord::new(1, 0, 0).expect("southwest tile");
        let southeast_tile = TileCoord::new(1, 1, 0).expect("southeast tile");
        let southwest = query_normalized_features(&client, &source, southwest_tile)
            .await
            .expect("southwest tile should query");
        let southeast = query_normalized_features(&client, &source, southeast_tile)
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
        assert_ne!(west_fragment.geometry, east_fragment.geometry);

        let root_feature = root_features
            .iter()
            .find(|feature| feature.id == "2")
            .expect("cross-boundary feature should exist in the root tile");
        let root_frame = MeshFrame::from_tile_region(
            TileCoord::root()
                .geographic_region_degrees(&source.bounds)
                .expect("root region"),
        );
        let root_mesh = footprint_feature_mesh(root_feature, root_frame);
        assert_eq!(root_mesh.vertices.len(), 24);
        assert_eq!(root_mesh.indices.len(), 36);

        for (fragment, tile) in [
            (west_fragment, southwest_tile),
            (east_fragment, southeast_tile),
        ] {
            let frame = MeshFrame::from_tile_region(
                tile.geographic_region_degrees(&source.bounds)
                    .expect("level-one tile region"),
            );
            let mesh = footprint_feature_mesh(fragment, frame);
            assert_eq!(mesh.vertices.len(), 20);
            assert_eq!(mesh.indices.len(), 30);
        }

        let interior_tile = TileCoord::new(4, 7, 2).expect("feature-interior tile");
        let interior_features = query_normalized_features(&client, &source, interior_tile)
            .await
            .expect("feature-interior tile should query");
        assert_eq!(interior_features.len(), 1);
        assert_eq!(interior_features[0].id, "2");
        assert!(
            matches!(
                &interior_features[0].geometry,
                NormalizedGeometry::GeographicFootprint(fragment)
                    if fragment.source_boundary.lines.is_empty()
            ),
            "a tile wholly inside the feature must not gain clip-edge walls"
        );
        let interior_frame = MeshFrame::from_tile_region(
            interior_tile
                .geographic_region_degrees(&source.bounds)
                .expect("feature-interior tile region"),
        );
        let interior_mesh = footprint_feature_mesh(&interior_features[0], interior_frame);
        assert_eq!(interior_mesh.vertices.len(), 8);
        assert_eq!(interior_mesh.indices.len(), 12);

        let interior_content = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/sources/poc_buildings/content/4/7/2.glb")
                    .body(Body::empty())
                    .expect("interior content request should build"),
            )
            .await
            .expect("interior content request should route");
        assert_eq!(interior_content.status(), StatusCode::OK);
        let interior_body = to_bytes(interior_content.into_body(), usize::MAX)
            .await
            .expect("interior content body should read");
        let interior_json_length =
            u32::from_le_bytes(interior_body[12..16].try_into().expect("JSON length")) as usize;
        let interior_document: serde_json::Value =
            serde_json::from_slice(&interior_body[20..20 + interior_json_length])
                .expect("interior content glTF JSON should parse");
        let primitive = &interior_document["meshes"][0]["primitives"][0];
        let position_accessor_index = primitive["attributes"]["POSITION"]
            .as_u64()
            .expect("position accessor") as usize;
        let index_accessor_index = primitive["indices"].as_u64().expect("index accessor") as usize;
        assert_eq!(
            interior_document["accessors"][position_accessor_index]["count"],
            8
        );
        assert_eq!(
            interior_document["accessors"][index_accessor_index]["count"],
            12
        );
        assert_eq!(
            interior_document["extensions"]["EXT_structural_metadata"]["propertyTables"][0]["count"],
            1
        );

        client
            .batch_execute(
                "BEGIN; \
                 CREATE TABLE public.poc_buildings_projected_adapter_test AS \
                 SELECT id, name, building_type, base_height_m, height_m, color, \
                        ST_Transform(geom, 3857) AS geom \
                 FROM public.poc_buildings",
            )
            .await
            .expect("create transaction-local projected fixture");
        let mut projected_source = source.clone();
        projected_source.table = "poc_buildings_projected_adapter_test".to_string();
        projected_source.srid = 3857;
        validate_source(&client, "projected_footprints", &projected_source)
            .await
            .expect("projected footprint source and transform should validate");
        let projected_features =
            query_normalized_features(&client, &projected_source, TileCoord::root())
                .await
                .expect("projected footprints should normalize to EPSG:4326");
        assert_eq!(projected_features.len(), root_features.len());
        assert!(projected_features.iter().all(|feature| {
            const TOLERANCE_DEG: f64 = 1.0e-8;
            let NormalizedGeometry::GeographicFootprint(fragment) = &feature.geometry else {
                return false;
            };
            fragment.geometry.polygons().iter().all(|polygon| {
                polygon.exterior.points.iter().all(|point| {
                    source.bounds.west - TOLERANCE_DEG <= point.x
                        && point.x <= source.bounds.east + TOLERANCE_DEG
                        && source.bounds.south - TOLERANCE_DEG <= point.y
                        && point.y <= source.bounds.north + TOLERANCE_DEG
                })
            })
        }));
        for feature in &projected_features {
            let mesh = footprint_feature_mesh(feature, root_frame);
            assert_eq!(mesh.vertices.len(), 24);
            assert_eq!(mesh.indices.len(), 36);
        }
        client
            .batch_execute("ROLLBACK")
            .await
            .expect("drop transaction-local projected fixture");

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

        let error = query_normalized_features(&client, &source, TileCoord::root())
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

    #[tokio::test]
    #[ignore = "requires DATABASE_URL, the PostGIS fixtures, and RDNAPTRANS2018 grids"]
    async fn surface_fixture_preserves_xyz_through_query_and_content_route() {
        let database_url = env::var("DATABASE_URL")
            .expect("DATABASE_URL is required for the ignored PostGIS integration test");

        let (client, connection) = tokio_postgres::connect(&database_url, NoTls)
            .await
            .expect("connect to PostGIS surface fixture database");
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("PostGIS connection error: {error}");
            }
        });
        client
            .batch_execute(include_str!(
                "../../../fixtures/postgis/surface_buildings_7415.sql"
            ))
            .await
            .expect("load surface fixture data");

        let catalog = fixture_catalog();
        let source = catalog
            .sources
            .get("surface_buildings_7415")
            .cloned()
            .expect("surface source should be configured");
        assert_eq!(source.max_level, 2, "native surfaces should be tiled");
        let profile = validate_source(&client, "surface_buildings_7415", &source)
            .await
            .expect("surface fixture and RDNAP grids should satisfy introspection");
        assert_eq!(profile.row_count, 2);
        assert_eq!(profile.srids, vec![7415]);
        assert_eq!(profile.geometry_types, vec!["MULTIPOLYGON", "POLYGON"]);
        assert_eq!(profile.zm_flags, vec![2]);
        crate::server::validate_catalog_sources(&catalog)
            .await
            .expect("server startup should fail-fast validate configured surface sources");

        // The RDNAP subtree candidate query batches coordinate conversion per
        // slot. Verify that collecting and splitting one PolygonZ, one
        // MultiPolygonZ, and the mixed pair preserves both IDs and the exact
        // per-feature NDR WKB produced by the former row-at-a-time transform.
        let mut exact_source = source.clone();
        exact_source.surface_subtree_envelope_shortcut = false;
        let exact_plan = build_surface_subtree_exact_candidates_query(&exact_source)
            .expect("surface exact-candidate query should build");
        assert_eq!(exact_plan.bindings, QueryBindings::Rdnaptrans2018Epsg1149);
        let west = vec![exact_source.bounds.west];
        let south = vec![exact_source.bounds.south];
        let east = vec![exact_source.bounds.east];
        let north = vec![exact_source.bounds.north];
        for (candidate_limit, candidate_offset) in [(1_i64, 0_i64), (1, 1), (2, 0)] {
            let candidate_limits = vec![candidate_limit];
            let candidate_offsets = vec![candidate_offset];
            let batched_rows = client
                .query(
                    &exact_plan.sql,
                    &[
                        &west,
                        &south,
                        &east,
                        &north,
                        &candidate_limits,
                        &candidate_offsets,
                        &exact_source.srid,
                        &RDNAPTRANS2018_EPSG_1149_PIPELINE,
                    ],
                )
                .await
                .expect("batched exact-candidate query should run");
            let per_row_reference = client
                .query(
                    "SELECT id::text, \
                            ST_AsBinary( \
                              ST_TransformPipeline(geom, $1, 4979), \
                              'NDR' \
                            ) \
                     FROM public.surface_buildings_7415 \
                     ORDER BY id \
                     LIMIT $2 OFFSET $3",
                    &[
                        &RDNAPTRANS2018_EPSG_1149_PIPELINE,
                        &candidate_limit,
                        &candidate_offset,
                    ],
                )
                .await
                .expect("per-row transform reference should run");

            assert_eq!(batched_rows.len(), per_row_reference.len());
            for (batched, reference) in batched_rows.iter().zip(&per_row_reference) {
                assert_eq!(
                    batched
                        .try_get::<_, String>(1)
                        .expect("batched candidate ID should decode"),
                    reference
                        .try_get::<_, String>(0)
                        .expect("reference candidate ID should decode")
                );
                assert_eq!(
                    batched
                        .try_get::<_, Vec<u8>>(2)
                        .expect("batched candidate WKB should decode"),
                    reference
                        .try_get::<_, Vec<u8>>(1)
                        .expect("reference candidate WKB should decode")
                );
            }
        }

        // Content uses the same collection transform in bounded batches, with
        // attribute arrays split back into the adapter's row contract. Compare
        // the mixed PolygonZ/MultiPolygonZ fixture byte-for-byte with the
        // former row-at-a-time transform.
        let content_plan =
            build_normalized_geometry_query(&source).expect("surface content query should build");
        assert_eq!(
            content_plan.attributes,
            vec!["identificatie", "name", "surface_kind", "color"]
        );
        let content_rows = client
            .query(
                &content_plan.sql,
                &[
                    &source.bounds.west,
                    &source.bounds.south,
                    &source.bounds.east,
                    &source.bounds.north,
                    &source.srid,
                    &RDNAPTRANS2018_EPSG_1149_PIPELINE,
                ],
            )
            .await
            .expect("batched surface content query should run");
        let content_reference = client
            .query(
                "SELECT id::text, \
                        ST_AsBinary(ST_TransformPipeline(geom, $1, 4979), 'NDR'), \
                        NULL::bytea, \
                        identificatie::text, name::text, surface_kind::text, color::text \
                 FROM public.surface_buildings_7415 \
                 ORDER BY id::text",
                &[&RDNAPTRANS2018_EPSG_1149_PIPELINE],
            )
            .await
            .expect("per-row content transform reference should run");
        assert_eq!(content_rows.len(), content_reference.len());
        for (batched, reference) in content_rows.iter().zip(&content_reference) {
            assert_eq!(
                batched.try_get::<_, String>(0).expect("batched ID"),
                reference.try_get::<_, String>(0).expect("reference ID")
            );
            assert_eq!(
                batched.try_get::<_, Vec<u8>>(1).expect("batched WKB"),
                reference.try_get::<_, Vec<u8>>(1).expect("reference WKB")
            );
            assert_eq!(
                batched
                    .try_get::<_, Option<Vec<u8>>>(2)
                    .expect("batched source boundary"),
                reference
                    .try_get::<_, Option<Vec<u8>>>(2)
                    .expect("reference source boundary")
            );
            for column in 3..7 {
                assert_eq!(
                    batched
                        .try_get::<_, Option<String>>(column)
                        .expect("batched attribute"),
                    reference
                        .try_get::<_, Option<String>>(column)
                        .expect("reference attribute")
                );
            }
        }

        let features = query_normalized_features(&client, &source, TileCoord::root())
            .await
            .expect("surface root tile should query");
        assert_eq!(features.len(), 2);
        for feature in &features {
            let NormalizedGeometry::GeodeticSurface(geometry) = &feature.geometry else {
                panic!("surface adapter output should be normalized EPSG:4979 XYZ");
            };
            assert!(geometry.polygons().iter().all(|polygon| {
                polygon
                    .exterior
                    .points
                    .iter()
                    .all(|point| point.z.is_finite() && point.z > 170.0)
            }));
            assert!(feature.attributes.contains_key("identificatie"));
            assert!(!feature.attributes.contains_key("height_m"));
        }

        query_normalized_features(
            &client,
            &source,
            TileCoord::new(1, 0, 0).expect("valid quadtree coordinate"),
        )
        .await
        .expect("surface child tile should accept whole-feature candidates");

        let availability = query_subtree_availability(&client, &source, TileCoord::root())
            .await
            .expect("surface subtree availability should use exact core clipping");
        let layout = subtree_layout(&source, TileCoord::root()).expect("surface subtree layout");

        let mut delayed_content_source = source.clone();
        delayed_content_source.subtree_levels = 1;
        delayed_content_source.tileset.content_start_level = 2;
        let delayed_availability =
            query_subtree_availability(&client, &delayed_content_source, TileCoord::root())
                .await
                .expect("delayed surface content should use paged boolean child availability");
        assert!(delayed_availability.tile[0]);
        assert!(!delayed_availability.content[0]);
        assert!(
            delayed_availability
                .child_subtree
                .iter()
                .any(|available| *available)
        );

        let app = crate::server::build_app(catalog).expect("surface fixture app should build");
        let mut occupied_children = 0;
        let mut empty_children = 0;
        for y in 0..4 {
            for x in 0..4 {
                let tile = TileCoord::new(2, x, y).expect("level-two surface tile");
                let child = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .uri(format!(
                                "/sources/surface_buildings_7415/content/2/{x}/{y}.glb"
                            ))
                            .body(Body::empty())
                            .expect("surface child request should build"),
                    )
                    .await
                    .expect("surface child request should route");
                let availability_index = layout
                    .local_tiles
                    .iter()
                    .position(|candidate| *candidate == Some(tile))
                    .expect("level-two tile should have an availability slot");
                assert_eq!(
                    availability.content[availability_index],
                    child.status() == StatusCode::OK,
                    "content availability must match route status for {tile:?}"
                );
                match child.status() {
                    StatusCode::OK => occupied_children += 1,
                    StatusCode::NOT_FOUND => empty_children += 1,
                    status => panic!("unexpected surface child status {status}"),
                }
            }
        }
        assert!(occupied_children > 0, "fixture should occupy child tiles");
        assert!(empty_children > 0, "fixture should leave empty child tiles");

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/sources/surface_buildings_7415/content/0/0/0.glb")
                    .body(Body::empty())
                    .expect("surface content request should build"),
            )
            .await
            .expect("surface content request should route");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("surface GLB body should read");
        let json_length =
            u32::from_le_bytes(body[12..16].try_into().expect("JSON length")) as usize;
        let document: serde_json::Value = serde_json::from_slice(&body[20..20 + json_length])
            .expect("surface GLB JSON should parse");
        assert_eq!(
            document["meshes"][0]["primitives"][0]["attributes"]["NORMAL"],
            2
        );
        assert_eq!(document["materials"][0]["doubleSided"], true);
        assert_eq!(
            document["extensions"]["EXT_structural_metadata"]["propertyTables"][0]["count"],
            2
        );
    }
}
