use std::fmt;

use crate::geometry::{
    FootprintGeometry, Point2D, Point3D, Polygon2D, Polygon3D, Ring2D, Ring3D, SurfaceGeometryZ,
    WkbError, decode_footprint_wkb, decode_surface_geometry_z_wkb,
};
use crate::source::SourceBounds;
use crate::tile::GeographicRegionDegrees;

const WGS84_A: f64 = 6_378_137.0;
const WGS84_F: f64 = 1.0 / 298.257_223_563;
const MIN_CLOSED_RING_POINTS: usize = 4;
const COORDINATE_EPSILON: f64 = 1.0e-12;
const PROJECTED_EPSILON: f64 = 1.0e-9;
const NORMAL_EPSILON: f64 = 1.0e-12;
const MAX_TRIANGULATION_DEVIATION: f64 = 1.0e-8;
pub const DEFAULT_MAX_NON_PLANAR_DISTANCE_M: f64 = 0.01;

/// Column-major axis conversion from glTF Y-up `[east, up, -north]` to Lucy
/// ENU `[east, north, up]`.
const GLTF_Y_UP_TO_ENU: [f64; 16] = [
    1.0, 0.0, 0.0, 0.0, // glTF X -> east
    0.0, 0.0, 1.0, 0.0, // glTF Y -> up
    0.0, -1.0, 0.0, 0.0, // glTF Z -> -north
    0.0, 0.0, 0.0, 1.0,
];

/// Inverse of [`GLTF_Y_UP_TO_ENU`].
const ENU_TO_GLTF_Y_UP: [f64; 16] = [
    1.0, 0.0, 0.0, 0.0, // east -> glTF X
    0.0, 0.0, -1.0, 0.0, // north -> -glTF Z
    0.0, 1.0, 0.0, 0.0, // up -> glTF Y
    0.0, 0.0, 0.0, 1.0,
];

/// A triangle mesh in a right-handed, tile-local ENU frame.
///
/// Positions and normals are kept Z-up internally. The GLB encoder is solely
/// responsible for rotating `[east, north, up]` to glTF Y-up
/// `[east, up, -north]`.
#[derive(Debug, Clone, PartialEq)]
pub struct TriangleMesh {
    pub vertices: Vec<MeshVertex>,
    pub indices: Vec<u32>,
}

impl TriangleMesh {
    fn new() -> Self {
        Self {
            vertices: Vec::new(),
            indices: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
}

/// A local tangent frame used for mesh construction or tileset placement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshFrame {
    pub origin_longitude_deg: f64,
    pub origin_latitude_deg: f64,
    pub origin_height_m: f64,
    origin_ecef: [f64; 3],
}

impl MeshFrame {
    /// Build the source frame whose ENU-to-ECEF transform is emitted once on
    /// the tileset root.
    pub fn from_source_bounds(bounds: &SourceBounds) -> Self {
        Self::from_geodetic_origin(
            (bounds.west + bounds.east) / 2.0,
            (bounds.south + bounds.north) / 2.0,
            bounds.min_height_m,
        )
    }

    /// Build the tile-local frame used to generate one content request.
    ///
    /// The tile centre keeps final `f32` vertex coordinates small even when a
    /// source spans a large area. Extrusion heights remain offsets from the
    /// region's minimum height; native surface Z values remain absolute
    /// ellipsoidal heights.
    pub fn from_tile_region(region: GeographicRegionDegrees) -> Self {
        Self::from_geodetic_origin(
            (region.west + region.east) / 2.0,
            (region.south + region.north) / 2.0,
            region.min_height_m,
        )
    }

    pub fn from_geodetic_origin(
        origin_longitude_deg: f64,
        origin_latitude_deg: f64,
        origin_height_m: f64,
    ) -> Self {
        let origin_ecef =
            geodetic_to_ecef(origin_longitude_deg, origin_latitude_deg, origin_height_m);
        Self {
            origin_longitude_deg,
            origin_latitude_deg,
            origin_height_m,
            origin_ecef,
        }
    }

    /// Column-major transform from local ENU `[East, North, Up]` to WGS84 ECEF.
    /// This is the one placement transform emitted on the tileset root.
    pub fn enu_to_ecef_transform(self) -> [f64; 16] {
        let lon = self.origin_longitude_deg.to_radians();
        let lat = self.origin_latitude_deg.to_radians();
        let (sin_lon, cos_lon) = lon.sin_cos();
        let (sin_lat, cos_lat) = lat.sin_cos();
        let [x, y, z] = self.origin_ecef;
        [
            -sin_lon,
            cos_lon,
            0.0,
            0.0,
            -sin_lat * cos_lon,
            -sin_lat * sin_lon,
            cos_lat,
            0.0,
            cos_lat * cos_lon,
            cos_lat * sin_lon,
            sin_lat,
            0.0,
            x,
            y,
            z,
            1.0,
        ]
    }

    /// Column-major glTF node transform that maps `tile_frame` coordinates
    /// into this source frame without adding a second ECEF placement.
    ///
    /// Mesh buffers are written as `g = R^-1 * ENU`, where `R` is the runtime
    /// glTF Y-up to 3D Tiles Z-up conversion. If `C = inverse(T_source) *
    /// T_tile`, the relative node matrix is `M = R^-1 * C * R`. The complete
    /// runtime chain is therefore `T_source * R * M * g = T_tile * ENU`.
    pub fn gltf_node_transform_for(self, tile_frame: Self) -> [f64; 16] {
        let source_transform = self.enu_to_ecef_transform();
        let tile_transform = tile_frame.enu_to_ecef_transform();

        let source_axes = [
            [
                source_transform[0],
                source_transform[1],
                source_transform[2],
            ],
            [
                source_transform[4],
                source_transform[5],
                source_transform[6],
            ],
            [
                source_transform[8],
                source_transform[9],
                source_transform[10],
            ],
        ];
        let tile_axes = [
            [tile_transform[0], tile_transform[1], tile_transform[2]],
            [tile_transform[4], tile_transform[5], tile_transform[6]],
            [tile_transform[8], tile_transform[9], tile_transform[10]],
        ];
        let origin_delta = sub3(tile_frame.origin_ecef, self.origin_ecef);

        let source_from_tile_enu = [
            dot3(source_axes[0], tile_axes[0]),
            dot3(source_axes[1], tile_axes[0]),
            dot3(source_axes[2], tile_axes[0]),
            0.0,
            dot3(source_axes[0], tile_axes[1]),
            dot3(source_axes[1], tile_axes[1]),
            dot3(source_axes[2], tile_axes[1]),
            0.0,
            dot3(source_axes[0], tile_axes[2]),
            dot3(source_axes[1], tile_axes[2]),
            dot3(source_axes[2], tile_axes[2]),
            0.0,
            dot3(source_axes[0], origin_delta),
            dot3(source_axes[1], origin_delta),
            dot3(source_axes[2], origin_delta),
            1.0,
        ];

        multiply_matrix4(
            ENU_TO_GLTF_Y_UP,
            multiply_matrix4(source_from_tile_enu, GLTF_Y_UP_TO_ENU),
        )
    }

    /// Project EPSG:4979 longitude/latitude/ellipsoidal height into local ENU.
    /// The calculation is performed in f64 and is an exact ECEF basis change;
    /// values are cast to f32 only when appended to the final mesh.
    pub fn project_geodetic(
        self,
        longitude_deg: f64,
        latitude_deg: f64,
        ellipsoidal_height_m: f64,
    ) -> Result<[f64; 3], MeshError> {
        if !self.origin_longitude_deg.is_finite()
            || !self.origin_latitude_deg.is_finite()
            || !self.origin_height_m.is_finite()
        {
            return Err(MeshError::InvalidFrame(
                "frame origin coordinates must be finite".to_string(),
            ));
        }
        if !(-180.0..=180.0).contains(&self.origin_longitude_deg)
            || !(-90.0..=90.0).contains(&self.origin_latitude_deg)
        {
            return Err(MeshError::InvalidFrame(format!(
                "frame origin longitude/latitude must be within [-180, 180] / [-90, 90], got ({}, {})",
                self.origin_longitude_deg, self.origin_latitude_deg
            )));
        }
        if !longitude_deg.is_finite()
            || !latitude_deg.is_finite()
            || !ellipsoidal_height_m.is_finite()
        {
            return Err(MeshError::NonFiniteCoordinate {
                polygon_index: 0,
                ring_index: 0,
                point_index: 0,
            });
        }
        if !(-180.0..=180.0).contains(&longitude_deg) || !(-90.0..=90.0).contains(&latitude_deg) {
            return Err(MeshError::GeodeticCoordinateOutOfRange {
                longitude_deg,
                latitude_deg,
            });
        }

        let point = geodetic_to_ecef(longitude_deg, latitude_deg, ellipsoidal_height_m);
        let delta = sub3(point, self.origin_ecef);
        let lon = self.origin_longitude_deg.to_radians();
        let lat = self.origin_latitude_deg.to_radians();
        let (sin_lon, cos_lon) = lon.sin_cos();
        let (sin_lat, cos_lat) = lat.sin_cos();
        Ok([
            -sin_lon * delta[0] + cos_lon * delta[1],
            -sin_lat * cos_lon * delta[0] - sin_lat * sin_lon * delta[1] + cos_lat * delta[2],
            cos_lat * cos_lon * delta[0] + cos_lat * sin_lon * delta[1] + sin_lat * delta[2],
        ])
    }

    fn project_local_height(
        self,
        longitude_deg: f64,
        latitude_deg: f64,
        local_height_m: f64,
    ) -> Result<[f64; 3], MeshError> {
        self.project_geodetic(
            longitude_deg,
            latitude_deg,
            self.origin_height_m + local_height_m,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfaceMeshOptions {
    pub max_non_planar_distance_m: f64,
}

impl Default for SurfaceMeshOptions {
    fn default() -> Self {
        Self {
            max_non_planar_distance_m: DEFAULT_MAX_NON_PLANAR_DISTANCE_M,
        }
    }
}

#[tracing::instrument(level = "debug", skip(wkb, frame), fields(input_wkb_bytes = wkb.len()))]
pub fn wkb_footprint_to_mesh(wkb: &[u8], frame: MeshFrame) -> Result<TriangleMesh, MeshError> {
    let geometry = decode_footprint_wkb(wkb)?;
    footprint_to_mesh(&geometry, frame)
}

pub fn footprint_to_mesh(
    geometry: &FootprintGeometry,
    frame: MeshFrame,
) -> Result<TriangleMesh, MeshError> {
    build_footprint_mesh(geometry, frame, None)
}

#[tracing::instrument(
    level = "debug",
    skip(wkb, frame),
    fields(input_wkb_bytes = wkb.len())
)]
pub fn wkb_footprint_to_extruded_mesh(
    wkb: &[u8],
    frame: MeshFrame,
    base_height_m: f32,
    height_m: f32,
) -> Result<TriangleMesh, MeshError> {
    let geometry = decode_footprint_wkb(wkb)?;
    footprint_to_extruded_mesh(
        &geometry,
        frame,
        f64::from(base_height_m),
        f64::from(height_m),
    )
}

pub fn footprint_to_extruded_mesh(
    geometry: &FootprintGeometry,
    frame: MeshFrame,
    base_height_m: f64,
    extrusion_height_m: f64,
) -> Result<TriangleMesh, MeshError> {
    validate_extrusion_heights(base_height_m, extrusion_height_m)?;
    build_footprint_mesh(
        geometry,
        frame,
        Some((base_height_m, base_height_m + extrusion_height_m)),
    )
}

#[tracing::instrument(level = "debug", skip(wkb, frame), fields(input_wkb_bytes = wkb.len()))]
pub fn wkb_surface_geometry_z_to_mesh(
    wkb: &[u8],
    frame: MeshFrame,
) -> Result<TriangleMesh, MeshError> {
    wkb_surface_geometry_z_to_mesh_with_options(wkb, frame, SurfaceMeshOptions::default())
}

pub fn wkb_surface_geometry_z_to_mesh_with_options(
    wkb: &[u8],
    frame: MeshFrame,
    options: SurfaceMeshOptions,
) -> Result<TriangleMesh, MeshError> {
    let geometry = decode_surface_geometry_z_wkb(wkb)?;
    surface_geometry_z_to_mesh_with_options(&geometry, frame, options)
}

pub fn surface_geometry_z_to_mesh(
    geometry: &SurfaceGeometryZ,
    frame: MeshFrame,
) -> Result<TriangleMesh, MeshError> {
    surface_geometry_z_to_mesh_with_options(geometry, frame, SurfaceMeshOptions::default())
}

pub fn surface_geometry_z_to_mesh_with_options(
    geometry: &SurfaceGeometryZ,
    frame: MeshFrame,
    options: SurfaceMeshOptions,
) -> Result<TriangleMesh, MeshError> {
    if !options.max_non_planar_distance_m.is_finite() || options.max_non_planar_distance_m < 0.0 {
        return Err(MeshError::InvalidSurfaceOptions(
            "max_non_planar_distance_m must be finite and nonnegative".to_string(),
        ));
    }
    if geometry.polygons().is_empty() {
        return Err(MeshError::EmptyGeometry);
    }

    let mut mesh = TriangleMesh::new();
    for (polygon_index, polygon) in geometry.polygons().iter().enumerate() {
        append_surface_polygon(&mut mesh, polygon, frame, options, polygon_index)?;
    }
    ensure_nonempty_mesh(mesh, "native surface")
}

fn build_footprint_mesh(
    geometry: &FootprintGeometry,
    frame: MeshFrame,
    extrusion: Option<(f64, f64)>,
) -> Result<TriangleMesh, MeshError> {
    if geometry.polygons().is_empty() {
        return Err(MeshError::EmptyGeometry);
    }

    let mut mesh = TriangleMesh::new();
    for (polygon_index, polygon) in geometry.polygons().iter().enumerate() {
        match extrusion {
            Some((base_height_m, top_height_m)) => append_extruded_polygon(
                &mut mesh,
                polygon,
                frame,
                base_height_m,
                top_height_m,
                polygon_index,
            )?,
            None => append_footprint_cap(&mut mesh, polygon, frame, 0.0, polygon_index)?,
        }
    }

    ensure_nonempty_mesh(
        mesh,
        if extrusion.is_some() {
            "extruded footprint"
        } else {
            "footprint"
        },
    )
}

fn append_footprint_cap(
    mesh: &mut TriangleMesh,
    polygon: &Polygon2D,
    frame: MeshFrame,
    local_height_m: f64,
    polygon_index: usize,
) -> Result<(), MeshError> {
    let prepared = prepare_footprint_polygon(polygon, frame, polygon_index)?;
    let triangles = triangulate_rings(&prepared.projected_rings, polygon_index)?;
    let positions = project_footprint_rings(&prepared.source_rings, frame, local_height_m)?;
    let normal = upward_face_normal(&positions, polygon_index)?;
    append_indexed_face(mesh, &positions, &triangles, normal, polygon_index)
}

fn append_extruded_polygon(
    mesh: &mut TriangleMesh,
    polygon: &Polygon2D,
    frame: MeshFrame,
    base_height_m: f64,
    top_height_m: f64,
    polygon_index: usize,
) -> Result<(), MeshError> {
    let prepared = prepare_footprint_polygon(polygon, frame, polygon_index)?;
    let triangles = triangulate_rings(&prepared.projected_rings, polygon_index)?;
    let bottom_positions = project_footprint_rings(&prepared.source_rings, frame, base_height_m)?;
    let top_positions = project_footprint_rings(&prepared.source_rings, frame, top_height_m)?;
    let top_normal = upward_face_normal(&top_positions, polygon_index)?;

    append_indexed_face(mesh, &top_positions, &triangles, top_normal, polygon_index)?;
    append_indexed_face(
        mesh,
        &bottom_positions,
        &triangles,
        scale3(top_normal, -1.0),
        polygon_index,
    )?;

    let mut flattened_offset = 0;
    for (ring_index, ring) in prepared.source_rings.iter().enumerate() {
        for edge_start in 0..ring.len() {
            let edge_end = (edge_start + 1) % ring.len();
            let bottom_start = bottom_positions[flattened_offset + edge_start];
            let bottom_end = bottom_positions[flattened_offset + edge_end];
            let top_end = top_positions[flattened_offset + edge_end];
            let top_start = top_positions[flattened_offset + edge_start];
            let normal = normalize3(cross3(
                sub3(bottom_end, bottom_start),
                sub3(top_start, bottom_start),
            ))
            .ok_or(MeshError::DegenerateEdge {
                polygon_index,
                ring_index,
                edge_index: edge_start,
            })?;
            append_quad(
                mesh,
                [bottom_start, bottom_end, top_end, top_start],
                normal,
                polygon_index,
            )?;
        }
        flattened_offset += ring.len();
    }
    Ok(())
}

fn append_surface_polygon(
    mesh: &mut TriangleMesh,
    polygon: &Polygon3D,
    frame: MeshFrame,
    options: SurfaceMeshOptions,
    polygon_index: usize,
) -> Result<(), MeshError> {
    let mut source_rings = Vec::with_capacity(polygon.interiors.len() + 1);
    source_rings.push(normalize_ring3(&polygon.exterior, polygon_index, 0)?);
    for (interior_index, ring) in polygon.interiors.iter().enumerate() {
        source_rings.push(normalize_ring3(ring, polygon_index, interior_index + 1)?);
    }

    let local_rings = source_rings
        .iter()
        .map(|ring| {
            ring.iter()
                .map(|point| frame.project_geodetic(point.x, point.y, point.z))
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    let face_normal = newell_normal(&local_rings[0]).ok_or(MeshError::DegenerateRing {
        polygon_index,
        ring_index: 0,
    })?;

    let dropped_axis = dominant_axis(face_normal);
    let projected_rings = local_rings
        .iter()
        .map(|ring| {
            ring.iter()
                .map(|point| project_dominant(*point, dropped_axis))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    validate_polygon_topology(&projected_rings, polygon_index)?;

    let reference = local_rings[0][0];
    let mut max_distance_m = 0.0_f64;
    for ring in &local_rings {
        for point in ring {
            max_distance_m = max_distance_m.max(dot3(sub3(*point, reference), face_normal).abs());
        }
    }
    if max_distance_m > options.max_non_planar_distance_m {
        return Err(MeshError::NonPlanarSurface {
            polygon_index,
            max_distance_m,
            tolerance_m: options.max_non_planar_distance_m,
        });
    }

    let triangles = triangulate_rings(&projected_rings, polygon_index)?;
    let positions = local_rings.into_iter().flatten().collect::<Vec<_>>();
    append_indexed_face(mesh, &positions, &triangles, face_normal, polygon_index)
}

struct PreparedFootprint {
    source_rings: Vec<Vec<Point2D>>,
    projected_rings: Vec<Vec<[f64; 2]>>,
}

fn prepare_footprint_polygon(
    polygon: &Polygon2D,
    frame: MeshFrame,
    polygon_index: usize,
) -> Result<PreparedFootprint, MeshError> {
    let mut source_rings = Vec::with_capacity(polygon.interiors.len() + 1);
    source_rings.push(normalize_ring2(&polygon.exterior, polygon_index, 0)?);
    for (interior_index, ring) in polygon.interiors.iter().enumerate() {
        source_rings.push(normalize_ring2(ring, polygon_index, interior_index + 1)?);
    }

    let mut projected_rings = project_footprint_xy(&source_rings, frame)?;
    for ring_index in 0..source_rings.len() {
        let area = signed_area2(&projected_rings[ring_index]);
        if area.abs() <= PROJECTED_EPSILON {
            return Err(MeshError::DegenerateRing {
                polygon_index,
                ring_index,
            });
        }
        let should_reverse = if ring_index == 0 {
            area < 0.0
        } else {
            area > 0.0
        };
        if should_reverse {
            source_rings[ring_index].reverse();
            projected_rings[ring_index].reverse();
        }
    }
    validate_polygon_topology(&projected_rings, polygon_index)?;
    Ok(PreparedFootprint {
        source_rings,
        projected_rings,
    })
}

fn project_footprint_xy(
    source_rings: &[Vec<Point2D>],
    frame: MeshFrame,
) -> Result<Vec<Vec<[f64; 2]>>, MeshError> {
    source_rings
        .iter()
        .map(|ring| {
            ring.iter()
                .map(|point| {
                    frame
                        .project_local_height(point.x, point.y, 0.0)
                        .map(|position| [position[0], position[1]])
                })
                .collect()
        })
        .collect()
}

fn project_footprint_rings(
    source_rings: &[Vec<Point2D>],
    frame: MeshFrame,
    local_height_m: f64,
) -> Result<Vec<[f64; 3]>, MeshError> {
    source_rings
        .iter()
        .flatten()
        .map(|point| frame.project_local_height(point.x, point.y, local_height_m))
        .collect()
}

fn normalize_ring2(
    ring: &Ring2D,
    polygon_index: usize,
    ring_index: usize,
) -> Result<Vec<Point2D>, MeshError> {
    if ring.points.len() < MIN_CLOSED_RING_POINTS {
        return Err(MeshError::RingTooShort {
            polygon_index,
            ring_index,
            point_count: ring.points.len(),
        });
    }
    for (point_index, point) in ring.points.iter().enumerate() {
        if !point.x.is_finite() || !point.y.is_finite() {
            return Err(MeshError::NonFiniteCoordinate {
                polygon_index,
                ring_index,
                point_index,
            });
        }
    }
    if !point2_equal(ring.points[0], *ring.points.last().expect("nonempty ring")) {
        return Err(MeshError::RingNotClosed {
            polygon_index,
            ring_index,
        });
    }

    let mut points = Vec::with_capacity(ring.points.len() - 1);
    for point in &ring.points[..ring.points.len() - 1] {
        if points
            .last()
            .is_none_or(|previous| !point2_equal(*previous, *point))
        {
            points.push(*point);
        }
    }
    if points.len() >= 2 && point2_equal(points[0], *points.last().expect("nonempty points")) {
        points.pop();
    }
    if points.len() < 3 {
        return Err(MeshError::RingTooShort {
            polygon_index,
            ring_index,
            point_count: points.len(),
        });
    }
    Ok(points)
}

fn normalize_ring3(
    ring: &Ring3D,
    polygon_index: usize,
    ring_index: usize,
) -> Result<Vec<Point3D>, MeshError> {
    if ring.points.len() < MIN_CLOSED_RING_POINTS {
        return Err(MeshError::RingTooShort {
            polygon_index,
            ring_index,
            point_count: ring.points.len(),
        });
    }
    for (point_index, point) in ring.points.iter().enumerate() {
        if !point.x.is_finite() || !point.y.is_finite() || !point.z.is_finite() {
            return Err(MeshError::NonFiniteCoordinate {
                polygon_index,
                ring_index,
                point_index,
            });
        }
    }
    if !point3_equal(ring.points[0], *ring.points.last().expect("nonempty ring")) {
        return Err(MeshError::RingNotClosed {
            polygon_index,
            ring_index,
        });
    }

    let mut points = Vec::with_capacity(ring.points.len() - 1);
    for point in &ring.points[..ring.points.len() - 1] {
        if points
            .last()
            .is_none_or(|previous| !point3_equal(*previous, *point))
        {
            points.push(*point);
        }
    }
    if points.len() >= 2 && point3_equal(points[0], *points.last().expect("nonempty points")) {
        points.pop();
    }
    if points.len() < 3 {
        return Err(MeshError::RingTooShort {
            polygon_index,
            ring_index,
            point_count: points.len(),
        });
    }
    Ok(points)
}

fn validate_polygon_topology(
    rings: &[Vec<[f64; 2]>],
    polygon_index: usize,
) -> Result<(), MeshError> {
    for (ring_index, ring) in rings.iter().enumerate() {
        if signed_area2(ring).abs() <= PROJECTED_EPSILON {
            return Err(MeshError::DegenerateRing {
                polygon_index,
                ring_index,
            });
        }
        if ring_self_intersects(ring) {
            return Err(MeshError::SelfIntersectingRing {
                polygon_index,
                ring_index,
            });
        }
    }

    let exterior = &rings[0];
    for hole_index in 1..rings.len() {
        let hole = &rings[hole_index];
        if rings_intersect(exterior, hole) || !point_in_ring_strict(hole[0], exterior) {
            return Err(MeshError::HoleOutsideExterior {
                polygon_index,
                ring_index: hole_index,
            });
        }
        for (other_hole_index, other) in rings.iter().enumerate().take(hole_index).skip(1) {
            if rings_intersect(other, hole)
                || point_in_ring_strict(hole[0], other)
                || point_in_ring_strict(other[0], hole)
            {
                return Err(MeshError::IntersectingInteriorRings {
                    polygon_index,
                    first_ring_index: other_hole_index,
                    second_ring_index: hole_index,
                });
            }
        }
    }
    Ok(())
}

fn triangulate_rings(
    rings: &[Vec<[f64; 2]>],
    polygon_index: usize,
) -> Result<Vec<usize>, MeshError> {
    let vertex_count = rings.iter().map(Vec::len).sum::<usize>();
    let mut vertices = Vec::with_capacity(vertex_count * 2);
    let mut hole_indices = Vec::with_capacity(rings.len().saturating_sub(1));
    let mut offset = 0;
    for (ring_index, ring) in rings.iter().enumerate() {
        if ring_index > 0 {
            hole_indices.push(offset);
        }
        for [x, y] in ring {
            vertices.extend_from_slice(&[*x, *y]);
        }
        offset += ring.len();
    }

    let triangles = earcutr::earcut(&vertices, &hole_indices, 2).map_err(|error| {
        MeshError::TriangulationFailed {
            polygon_index,
            message: format!("{error:?}"),
        }
    })?;
    if triangles.is_empty() || !triangles.len().is_multiple_of(3) {
        return Err(MeshError::TriangulationFailed {
            polygon_index,
            message: "earcut returned no complete triangles".to_string(),
        });
    }
    if triangles.iter().any(|index| *index >= vertex_count) {
        return Err(MeshError::TriangulationFailed {
            polygon_index,
            message: "earcut returned an out-of-range vertex index".to_string(),
        });
    }
    let deviation = earcutr::deviation(&vertices, &hole_indices, 2, &triangles);
    if !deviation.is_finite() || deviation > MAX_TRIANGULATION_DEVIATION {
        return Err(MeshError::TriangulationDeviation {
            polygon_index,
            deviation,
        });
    }
    Ok(triangles)
}

fn append_indexed_face(
    mesh: &mut TriangleMesh,
    positions: &[[f64; 3]],
    triangles: &[usize],
    desired_normal: [f64; 3],
    polygon_index: usize,
) -> Result<(), MeshError> {
    let normal = normalize3(desired_normal).ok_or(MeshError::DegenerateRing {
        polygon_index,
        ring_index: 0,
    })?;
    let base_index = reserve_vertices(mesh, positions.len())?;
    let normal_f32 = vec3_to_f32(normal)?;
    for position in positions {
        mesh.vertices.push(MeshVertex {
            position: vec3_to_f32(*position)?,
            normal: normal_f32,
        });
    }

    for triangle in triangles.chunks_exact(3) {
        let a = triangle[0];
        let mut b = triangle[1];
        let mut c = triangle[2];
        let triangle_normal = cross3(
            sub3(positions[b], positions[a]),
            sub3(positions[c], positions[a]),
        );
        if length3(triangle_normal) <= NORMAL_EPSILON {
            return Err(MeshError::DegenerateTriangle { polygon_index });
        }
        if dot3(triangle_normal, normal) < 0.0 {
            std::mem::swap(&mut b, &mut c);
        }
        mesh.indices.extend_from_slice(&[
            checked_index(base_index, a)?,
            checked_index(base_index, b)?,
            checked_index(base_index, c)?,
        ]);
    }
    Ok(())
}

fn append_quad(
    mesh: &mut TriangleMesh,
    positions: [[f64; 3]; 4],
    normal: [f64; 3],
    polygon_index: usize,
) -> Result<(), MeshError> {
    append_indexed_face(mesh, &positions, &[0, 1, 2, 0, 2, 3], normal, polygon_index)
}

fn reserve_vertices(mesh: &TriangleMesh, additional: usize) -> Result<u32, MeshError> {
    let total = mesh
        .vertices
        .len()
        .checked_add(additional)
        .ok_or(MeshError::IndexOverflow)?;
    if total > u32::MAX as usize {
        return Err(MeshError::IndexOverflow);
    }
    u32::try_from(mesh.vertices.len()).map_err(|_| MeshError::IndexOverflow)
}

fn checked_index(base_index: u32, local_index: usize) -> Result<u32, MeshError> {
    base_index
        .checked_add(u32::try_from(local_index).map_err(|_| MeshError::IndexOverflow)?)
        .ok_or(MeshError::IndexOverflow)
}

fn ensure_nonempty_mesh(
    mesh: TriangleMesh,
    context: &'static str,
) -> Result<TriangleMesh, MeshError> {
    if mesh.vertices.is_empty() || mesh.indices.is_empty() {
        Err(MeshError::MeshIsEmpty(context))
    } else {
        tracing::debug!(
            vertex_count = mesh.vertices.len(),
            triangle_count = mesh.indices.len() / 3,
            context,
            "mesh generated"
        );
        Ok(mesh)
    }
}

fn validate_extrusion_heights(
    base_height_m: f64,
    extrusion_height_m: f64,
) -> Result<(), MeshError> {
    if !base_height_m.is_finite() || !extrusion_height_m.is_finite() {
        return Err(MeshError::InvalidExtrusion {
            base_height_m,
            extrusion_height_m,
            reason: "base_height_m and height_m must be finite",
        });
    }
    if extrusion_height_m <= 0.0 {
        return Err(MeshError::InvalidExtrusion {
            base_height_m,
            extrusion_height_m,
            reason: "height_m must be greater than zero",
        });
    }
    if !((base_height_m + extrusion_height_m).is_finite()) {
        return Err(MeshError::InvalidExtrusion {
            base_height_m,
            extrusion_height_m,
            reason: "base_height_m + height_m must be finite",
        });
    }
    Ok(())
}

fn upward_face_normal(positions: &[[f64; 3]], polygon_index: usize) -> Result<[f64; 3], MeshError> {
    // The exterior is first in every flattened footprint position list. Its
    // projected winding was normalized to CCW, so Newell points generally up.
    // A direct triangle search avoids needing the exterior ring length here.
    for index in 1..positions.len().saturating_sub(1) {
        let normal = cross3(
            sub3(positions[index], positions[0]),
            sub3(positions[index + 1], positions[0]),
        );
        if let Some(mut normal) = normalize3(normal) {
            if normal[2] < 0.0 {
                normal = scale3(normal, -1.0);
            }
            return Ok(normal);
        }
    }
    Err(MeshError::DegenerateRing {
        polygon_index,
        ring_index: 0,
    })
}

fn geodetic_to_ecef(lon_deg: f64, lat_deg: f64, height_m: f64) -> [f64; 3] {
    let lon = lon_deg.to_radians();
    let lat = lat_deg.to_radians();
    let (sin_lon, cos_lon) = lon.sin_cos();
    let (sin_lat, cos_lat) = lat.sin_cos();
    let eccentricity_squared = WGS84_F * (2.0 - WGS84_F);
    let prime_vertical_radius = WGS84_A / (1.0 - eccentricity_squared * sin_lat * sin_lat).sqrt();
    [
        (prime_vertical_radius + height_m) * cos_lat * cos_lon,
        (prime_vertical_radius + height_m) * cos_lat * sin_lon,
        (prime_vertical_radius * (1.0 - eccentricity_squared) + height_m) * sin_lat,
    ]
}

fn newell_normal(points: &[[f64; 3]]) -> Option<[f64; 3]> {
    let mut normal = [0.0; 3];
    for (current, next) in points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
    {
        normal[0] += (current[1] - next[1]) * (current[2] + next[2]);
        normal[1] += (current[2] - next[2]) * (current[0] + next[0]);
        normal[2] += (current[0] - next[0]) * (current[1] + next[1]);
    }
    normalize3(normal)
}

#[derive(Debug, Clone, Copy)]
enum Axis {
    X,
    Y,
    Z,
}

fn dominant_axis(normal: [f64; 3]) -> Axis {
    let absolute = [normal[0].abs(), normal[1].abs(), normal[2].abs()];
    if absolute[0] >= absolute[1] && absolute[0] >= absolute[2] {
        Axis::X
    } else if absolute[1] >= absolute[2] {
        Axis::Y
    } else {
        Axis::Z
    }
}

fn project_dominant(point: [f64; 3], dropped_axis: Axis) -> [f64; 2] {
    // Cyclic coordinate pairs preserve a right-handed orientation for the
    // positive dominant normal. Final 3D winding is checked independently.
    match dropped_axis {
        Axis::X => [point[1], point[2]],
        Axis::Y => [point[2], point[0]],
        Axis::Z => [point[0], point[1]],
    }
}

fn ring_self_intersects(ring: &[[f64; 2]]) -> bool {
    for first in 0..ring.len() {
        let first_next = (first + 1) % ring.len();
        for second in first + 1..ring.len() {
            let second_next = (second + 1) % ring.len();
            if first == second
                || first_next == second
                || second_next == first
                || (first == 0 && second_next == 0)
            {
                continue;
            }
            if segments_intersect(
                ring[first],
                ring[first_next],
                ring[second],
                ring[second_next],
            ) {
                return true;
            }
        }
    }
    false
}

fn rings_intersect(first: &[[f64; 2]], second: &[[f64; 2]]) -> bool {
    (0..first.len()).any(|first_index| {
        let first_next = (first_index + 1) % first.len();
        (0..second.len()).any(|second_index| {
            let second_next = (second_index + 1) % second.len();
            segments_intersect(
                first[first_index],
                first[first_next],
                second[second_index],
                second[second_next],
            )
        })
    })
}

fn segments_intersect(a: [f64; 2], b: [f64; 2], c: [f64; 2], d: [f64; 2]) -> bool {
    let ab_c = cross2(a, b, c);
    let ab_d = cross2(a, b, d);
    let cd_a = cross2(c, d, a);
    let cd_b = cross2(c, d, b);
    if ((ab_c > PROJECTED_EPSILON && ab_d < -PROJECTED_EPSILON)
        || (ab_c < -PROJECTED_EPSILON && ab_d > PROJECTED_EPSILON))
        && ((cd_a > PROJECTED_EPSILON && cd_b < -PROJECTED_EPSILON)
            || (cd_a < -PROJECTED_EPSILON && cd_b > PROJECTED_EPSILON))
    {
        return true;
    }
    (ab_c.abs() <= PROJECTED_EPSILON && point_on_segment(c, a, b))
        || (ab_d.abs() <= PROJECTED_EPSILON && point_on_segment(d, a, b))
        || (cd_a.abs() <= PROJECTED_EPSILON && point_on_segment(a, c, d))
        || (cd_b.abs() <= PROJECTED_EPSILON && point_on_segment(b, c, d))
}

fn point_on_segment(point: [f64; 2], start: [f64; 2], end: [f64; 2]) -> bool {
    point[0] >= start[0].min(end[0]) - PROJECTED_EPSILON
        && point[0] <= start[0].max(end[0]) + PROJECTED_EPSILON
        && point[1] >= start[1].min(end[1]) - PROJECTED_EPSILON
        && point[1] <= start[1].max(end[1]) + PROJECTED_EPSILON
}

fn point_in_ring_strict(point: [f64; 2], ring: &[[f64; 2]]) -> bool {
    if (0..ring.len()).any(|index| {
        let next = (index + 1) % ring.len();
        cross2(ring[index], ring[next], point).abs() <= PROJECTED_EPSILON
            && point_on_segment(point, ring[index], ring[next])
    }) {
        return false;
    }

    let mut inside = false;
    for (a, b) in ring
        .iter()
        .zip(ring.iter().cycle().skip(1))
        .take(ring.len())
    {
        if (a[1] > point[1]) != (b[1] > point[1])
            && point[0] < (b[0] - a[0]) * (point[1] - a[1]) / (b[1] - a[1]) + a[0]
        {
            inside = !inside;
        }
    }
    inside
}

fn signed_area2(points: &[[f64; 2]]) -> f64 {
    points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
        .map(|(a, b)| a[0] * b[1] - b[0] * a[1])
        .sum::<f64>()
        / 2.0
}

fn cross2(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

fn point2_equal(a: Point2D, b: Point2D) -> bool {
    (a.x - b.x).abs() <= COORDINATE_EPSILON && (a.y - b.y).abs() <= COORDINATE_EPSILON
}

fn point3_equal(a: Point3D, b: Point3D) -> bool {
    point2_equal(Point2D { x: a.x, y: a.y }, Point2D { x: b.x, y: b.y })
        && (a.z - b.z).abs() <= COORDINATE_EPSILON
}

fn sub3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn scale3(vector: [f64; 3], scale: f64) -> [f64; 3] {
    [vector[0] * scale, vector[1] * scale, vector[2] * scale]
}

fn cross3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn multiply_matrix4(left: [f64; 16], right: [f64; 16]) -> [f64; 16] {
    let mut product = [0.0; 16];
    for column in 0..4 {
        for row in 0..4 {
            product[column * 4 + row] = (0..4)
                .map(|inner| left[inner * 4 + row] * right[column * 4 + inner])
                .sum();
        }
    }
    product
}

fn length3(vector: [f64; 3]) -> f64 {
    dot3(vector, vector).sqrt()
}

fn normalize3(vector: [f64; 3]) -> Option<[f64; 3]> {
    let length = length3(vector);
    if !length.is_finite() || length <= NORMAL_EPSILON {
        None
    } else {
        Some(scale3(vector, 1.0 / length))
    }
}

fn vec3_to_f32(vector: [f64; 3]) -> Result<[f32; 3], MeshError> {
    let converted = [vector[0] as f32, vector[1] as f32, vector[2] as f32];
    if converted.iter().all(|component| component.is_finite()) {
        Ok(converted)
    } else {
        Err(MeshError::CoordinateOutsideF32Range)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MeshError {
    Wkb(WkbError),
    InvalidFrame(String),
    InvalidSurfaceOptions(String),
    EmptyGeometry,
    MeshIsEmpty(&'static str),
    InvalidExtrusion {
        base_height_m: f64,
        extrusion_height_m: f64,
        reason: &'static str,
    },
    RingTooShort {
        polygon_index: usize,
        ring_index: usize,
        point_count: usize,
    },
    RingNotClosed {
        polygon_index: usize,
        ring_index: usize,
    },
    NonFiniteCoordinate {
        polygon_index: usize,
        ring_index: usize,
        point_index: usize,
    },
    GeodeticCoordinateOutOfRange {
        longitude_deg: f64,
        latitude_deg: f64,
    },
    DegenerateRing {
        polygon_index: usize,
        ring_index: usize,
    },
    SelfIntersectingRing {
        polygon_index: usize,
        ring_index: usize,
    },
    HoleOutsideExterior {
        polygon_index: usize,
        ring_index: usize,
    },
    IntersectingInteriorRings {
        polygon_index: usize,
        first_ring_index: usize,
        second_ring_index: usize,
    },
    NonPlanarSurface {
        polygon_index: usize,
        max_distance_m: f64,
        tolerance_m: f64,
    },
    DegenerateEdge {
        polygon_index: usize,
        ring_index: usize,
        edge_index: usize,
    },
    DegenerateTriangle {
        polygon_index: usize,
    },
    TriangulationFailed {
        polygon_index: usize,
        message: String,
    },
    TriangulationDeviation {
        polygon_index: usize,
        deviation: f64,
    },
    CoordinateOutsideF32Range,
    IndexOverflow,
}

impl fmt::Display for MeshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wkb(error) => write!(f, "failed to decode polygonal WKB: {error}"),
            Self::InvalidFrame(message) => write!(f, "invalid mesh frame: {message}"),
            Self::InvalidSurfaceOptions(message) => {
                write!(f, "invalid surface mesh options: {message}")
            }
            Self::EmptyGeometry => write!(f, "polygonal geometry is empty"),
            Self::MeshIsEmpty(context) => write!(f, "{context} produced an empty mesh"),
            Self::InvalidExtrusion {
                base_height_m,
                extrusion_height_m,
                reason,
            } => write!(
                f,
                "invalid extrusion base_height_m={base_height_m} height_m={extrusion_height_m}: {reason}"
            ),
            Self::RingTooShort {
                polygon_index,
                ring_index,
                point_count,
            } => write!(
                f,
                "polygon {polygon_index} ring {ring_index} has {point_count} point(s); a closed ring needs at least four"
            ),
            Self::RingNotClosed {
                polygon_index,
                ring_index,
            } => write!(f, "polygon {polygon_index} ring {ring_index} is not closed"),
            Self::NonFiniteCoordinate {
                polygon_index,
                ring_index,
                point_index,
            } => write!(
                f,
                "polygon {polygon_index} ring {ring_index} point {point_index} contains a non-finite coordinate"
            ),
            Self::GeodeticCoordinateOutOfRange {
                longitude_deg,
                latitude_deg,
            } => write!(
                f,
                "geodetic coordinate longitude={longitude_deg} latitude={latitude_deg} is outside [-180, 180] / [-90, 90]"
            ),
            Self::DegenerateRing {
                polygon_index,
                ring_index,
            } => write!(
                f,
                "polygon {polygon_index} ring {ring_index} is degenerate in its triangulation plane"
            ),
            Self::SelfIntersectingRing {
                polygon_index,
                ring_index,
            } => write!(
                f,
                "polygon {polygon_index} ring {ring_index} is self-intersecting"
            ),
            Self::HoleOutsideExterior {
                polygon_index,
                ring_index,
            } => write!(
                f,
                "polygon {polygon_index} interior ring {ring_index} is outside or touches the exterior"
            ),
            Self::IntersectingInteriorRings {
                polygon_index,
                first_ring_index,
                second_ring_index,
            } => write!(
                f,
                "polygon {polygon_index} interior rings {first_ring_index} and {second_ring_index} intersect or contain one another"
            ),
            Self::NonPlanarSurface {
                polygon_index,
                max_distance_m,
                tolerance_m,
            } => write!(
                f,
                "polygon {polygon_index} is non-planar: max point-to-plane distance {max_distance_m} m exceeds {tolerance_m} m"
            ),
            Self::DegenerateEdge {
                polygon_index,
                ring_index,
                edge_index,
            } => write!(
                f,
                "polygon {polygon_index} ring {ring_index} edge {edge_index} cannot form an extrusion wall"
            ),
            Self::DegenerateTriangle { polygon_index } => {
                write!(
                    f,
                    "polygon {polygon_index} triangulation contains a zero-area triangle"
                )
            }
            Self::TriangulationFailed {
                polygon_index,
                message,
            } => write!(f, "polygon {polygon_index} triangulation failed: {message}"),
            Self::TriangulationDeviation {
                polygon_index,
                deviation,
            } => write!(
                f,
                "polygon {polygon_index} triangulation area deviation {deviation} exceeds {MAX_TRIANGULATION_DEVIATION}"
            ),
            Self::CoordinateOutsideF32Range => {
                write!(f, "local mesh coordinate is outside the finite f32 range")
            }
            Self::IndexOverflow => write!(f, "mesh exceeds the u32 index range"),
        }
    }
}

impl std::error::Error for MeshError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Wkb(error) => Some(error),
            _ => None,
        }
    }
}

impl From<WkbError> for MeshError {
    fn from(error: WkbError) -> Self {
        Self::Wkb(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Polygon3D, Ring3D};
    use crate::tile::TileCoord;

    fn fixture_bounds() -> SourceBounds {
        SourceBounds {
            west: 5.0,
            south: 50.0,
            east: 5.01,
            north: 50.01,
            min_height_m: 0.0,
            max_height_m: 100.0,
        }
    }

    fn fixture_frame() -> MeshFrame {
        MeshFrame::from_source_bounds(&fixture_bounds())
    }

    fn ring2(points: &[(f64, f64)]) -> Ring2D {
        Ring2D {
            points: points
                .iter()
                .map(|(x, y)| Point2D { x: *x, y: *y })
                .collect(),
        }
    }

    fn square2(west: f64, south: f64, size: f64) -> Ring2D {
        ring2(&[
            (west, south),
            (west + size, south),
            (west + size, south + size),
            (west, south + size),
            (west, south),
        ])
    }

    fn point3(x: f64, y: f64, z: f64) -> Point3D {
        Point3D { x, y, z }
    }

    fn ring3(points: &[[f64; 3]]) -> Ring3D {
        Ring3D {
            points: points
                .iter()
                .map(|point| point3(point[0], point[1], point[2]))
                .collect(),
        }
    }

    fn assert_unit_normals(mesh: &TriangleMesh) {
        for vertex in &mesh.vertices {
            let length = vertex
                .normal
                .iter()
                .map(|component| component * component)
                .sum::<f32>()
                .sqrt();
            assert!((length - 1.0).abs() < 1.0e-5, "normal={:?}", vertex.normal);
        }
    }

    fn transform_vector(matrix: [f64; 16], vector: [f64; 4]) -> [f64; 4] {
        let mut result = [0.0; 4];
        for row in 0..4 {
            result[row] = (0..4)
                .map(|column| matrix[column * 4 + row] * vector[column])
                .sum();
        }
        result
    }

    fn assert_vector_close(actual: [f64; 4], expected: [f64; 4], tolerance: f64) {
        for component in 0..4 {
            let difference = (actual[component] - expected[component]).abs();
            assert!(
                difference <= tolerance,
                "component {component}: actual={} expected={} difference={difference} tolerance={tolerance}",
                actual[component],
                expected[component]
            );
        }
    }

    #[test]
    fn source_frame_origin_and_enu_transform_are_consistent() {
        let frame = fixture_frame();
        let origin = frame
            .project_geodetic(
                frame.origin_longitude_deg,
                frame.origin_latitude_deg,
                frame.origin_height_m,
            )
            .expect("origin should project");
        assert!(origin.iter().all(|component| component.abs() < 1.0e-8));

        let transform = frame.enu_to_ecef_transform();
        assert_eq!(&transform[12..15], &frame.origin_ecef);
        let east = [transform[0], transform[1], transform[2]];
        let north = [transform[4], transform[5], transform[6]];
        let up = [transform[8], transform[9], transform[10]];
        assert!(dot3(east, north).abs() < 1.0e-12);
        assert!(dot3(east, up).abs() < 1.0e-12);
        assert!(dot3(north, up).abs() < 1.0e-12);
        assert!((dot3(cross3(east, north), up) - 1.0).abs() < 1.0e-12);
    }

    #[test]
    fn relative_node_transform_completes_the_tile_local_ecef_chain() {
        let source_frame = MeshFrame::from_geodetic_origin(5.0, 50.0, -2.0);
        let tile_frame = MeshFrame::from_geodetic_origin(5.73, 50.41, 12.0);
        let node_transform = source_frame.gltf_node_transform_for(tile_frame);
        let tile_enu = [12.25, -4.5, 8.75, 1.0];
        let gltf_position = [tile_enu[0], tile_enu[2], -tile_enu[1], 1.0];

        let source_chain = transform_vector(
            source_frame.enu_to_ecef_transform(),
            transform_vector(
                GLTF_Y_UP_TO_ENU,
                transform_vector(node_transform, gltf_position),
            ),
        );
        let direct_tile_chain = transform_vector(tile_frame.enu_to_ecef_transform(), tile_enu);
        assert_vector_close(source_chain, direct_tile_chain, 1.0e-8);

        let tile_up = [0.0, 0.0, 1.0, 0.0];
        let gltf_up = [0.0, 1.0, 0.0, 0.0];
        let source_normal_chain = transform_vector(
            source_frame.enu_to_ecef_transform(),
            transform_vector(GLTF_Y_UP_TO_ENU, transform_vector(node_transform, gltf_up)),
        );
        let direct_tile_normal = transform_vector(tile_frame.enu_to_ecef_transform(), tile_up);
        assert_vector_close(source_normal_chain, direct_tile_normal, 1.0e-12);
    }

    #[test]
    fn root_tile_relative_node_transform_is_identity() {
        let bounds = fixture_bounds();
        let source_frame = MeshFrame::from_source_bounds(&bounds);
        let root_region = TileCoord::root()
            .geographic_region_degrees(&bounds)
            .expect("root region");
        let tile_frame = MeshFrame::from_tile_region(root_region);
        let transform = source_frame.gltf_node_transform_for(tile_frame);
        let identity = [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ];

        for (component, (actual, expected)) in transform.into_iter().zip(identity).enumerate() {
            assert!(
                (actual - expected).abs() < 1.0e-8,
                "matrix component {component}: actual={actual} expected={expected}"
            );
        }
    }

    #[test]
    fn project_geodetic_rejects_coordinates_outside_wgs84_domain() {
        let frame = MeshFrame::from_geodetic_origin(0.0, 0.0, 0.0);
        for (longitude_deg, latitude_deg) in [
            (-180.000_001, 0.0),
            (180.000_001, 0.0),
            (0.0, -90.000_001),
            (0.0, 90.000_001),
        ] {
            let error = frame
                .project_geodetic(longitude_deg, latitude_deg, 0.0)
                .expect_err("out-of-range geodetic coordinate should fail");
            assert!(matches!(
                error,
                MeshError::GeodeticCoordinateOutOfRange { .. }
            ));
        }

        for (longitude_deg, latitude_deg) in [(-180.0, -90.0), (180.0, 90.0), (0.0, 0.0)] {
            frame
                .project_geodetic(longitude_deg, latitude_deg, 0.0)
                .expect("closed WGS84 bounds should be accepted");
        }
    }

    #[test]
    fn project_geodetic_rejects_out_of_range_frame_origin() {
        let frame = MeshFrame::from_geodetic_origin(181.0, 0.0, 0.0);
        let error = frame
            .project_geodetic(0.0, 0.0, 0.0)
            .expect_err("out-of-range origin should fail");
        assert!(matches!(error, MeshError::InvalidFrame(_)));
    }

    #[test]
    fn extruded_footprint_supports_holes_and_flat_normals() {
        let polygon = Polygon2D {
            exterior: square2(5.001, 50.001, 0.006),
            interiors: vec![square2(5.003, 50.003, 0.002)],
        };
        let geometry = FootprintGeometry::Polygon(polygon);
        let mesh = footprint_to_extruded_mesh(&geometry, fixture_frame(), 2.0, 8.0)
            .expect("footprint with a hole should extrude");

        assert_eq!(mesh.vertices.len(), 48);
        assert_eq!(mesh.indices.len(), 96);
        assert_unit_normals(&mesh);
        assert!(
            mesh.vertices[0..8]
                .iter()
                .all(|vertex| vertex.normal[2] > 0.99)
        );
        assert!(
            mesh.vertices[8..16]
                .iter()
                .all(|vertex| vertex.normal[2] < -0.99)
        );
        assert!(
            mesh.vertices[16..]
                .iter()
                .all(|vertex| vertex.normal[2].abs() < 0.01)
        );
    }

    #[test]
    fn native_surface_hole_triangulates_without_filling_the_hole() {
        let exterior = ring3(&[
            [5.002, 50.002, 20.0],
            [5.005, 50.002, 20.0],
            [5.005, 50.005, 20.0],
            [5.002, 50.005, 20.0],
            [5.002, 50.002, 20.0],
        ]);
        let interior = ring3(&[
            [5.003, 50.003, 20.0],
            [5.004, 50.003, 20.0],
            [5.004, 50.004, 20.0],
            [5.003, 50.004, 20.0],
            [5.003, 50.003, 20.0],
        ]);
        let geometry = SurfaceGeometryZ::Polygon(Polygon3D {
            exterior,
            interiors: vec![interior],
        });
        let mesh = surface_geometry_z_to_mesh(&geometry, fixture_frame())
            .expect("surface hole should triangulate");

        assert_eq!(mesh.vertices.len(), 8);
        assert_eq!(mesh.indices.len(), 24);
        assert_unit_normals(&mesh);
        assert!(mesh.vertices.iter().all(|vertex| vertex.normal[2] > 0.99));
    }

    #[test]
    fn vertical_polygon_uses_dominant_plane_and_preserves_height() {
        let polygon = Polygon3D {
            exterior: ring3(&[
                [5.002, 50.002, 10.0],
                [5.006, 50.002, 10.0],
                [5.006, 50.002, 25.0],
                [5.002, 50.002, 25.0],
                [5.002, 50.002, 10.0],
            ]),
            interiors: Vec::new(),
        };
        let mesh = surface_geometry_z_to_mesh(&SurfaceGeometryZ::Polygon(polygon), fixture_frame())
            .expect("vertical PolygonZ should triangulate");

        assert_eq!(mesh.vertices.len(), 4);
        assert_eq!(mesh.indices.len(), 6);
        let min_up = mesh
            .vertices
            .iter()
            .map(|vertex| vertex.position[2])
            .fold(f32::INFINITY, f32::min);
        let max_up = mesh
            .vertices
            .iter()
            .map(|vertex| vertex.position[2])
            .fold(f32::NEG_INFINITY, f32::max);
        assert!((max_up - min_up - 15.0).abs() < 0.01);
        assert!(
            mesh.vertices
                .iter()
                .all(|vertex| vertex.normal[2].abs() < 0.01)
        );
    }

    #[test]
    fn multipolygon_surfaces_merge_into_one_feature_mesh() {
        let face = |west: f64| Polygon3D {
            exterior: ring3(&[
                [west, 50.002, 10.0],
                [west + 0.001, 50.002, 10.0],
                [west + 0.001, 50.003, 10.0],
                [west, 50.003, 10.0],
                [west, 50.002, 10.0],
            ]),
            interiors: Vec::new(),
        };
        let geometry = SurfaceGeometryZ::MultiPolygon(vec![face(5.002), face(5.005)]);
        let mesh = surface_geometry_z_to_mesh(&geometry, fixture_frame())
            .expect("MultiPolygonZ should produce one mesh");
        assert_eq!(mesh.vertices.len(), 8);
        assert_eq!(mesh.indices.len(), 12);
    }

    #[test]
    fn meaningfully_nonplanar_surface_is_rejected() {
        let geometry = SurfaceGeometryZ::Polygon(Polygon3D {
            exterior: ring3(&[
                [5.002, 50.002, 10.0],
                [5.006, 50.002, 10.0],
                [5.006, 50.006, 10.0],
                [5.002, 50.006, 11.0],
                [5.002, 50.002, 10.0],
            ]),
            interiors: Vec::new(),
        });
        let error = surface_geometry_z_to_mesh(&geometry, fixture_frame())
            .expect_err("warped surface should fail");
        assert!(matches!(error, MeshError::NonPlanarSurface { .. }));
    }

    #[test]
    fn self_intersecting_surface_ring_is_rejected() {
        let geometry = SurfaceGeometryZ::Polygon(Polygon3D {
            exterior: ring3(&[
                [5.002, 50.002, 10.0],
                [5.005, 50.002, 10.0],
                [5.002, 50.005, 10.0],
                [5.0045, 50.005, 10.0],
                [5.002, 50.002, 10.0],
            ]),
            interiors: Vec::new(),
        });
        let error = surface_geometry_z_to_mesh(&geometry, fixture_frame())
            .expect_err("self-intersection should fail");
        assert!(
            matches!(error, MeshError::SelfIntersectingRing { .. }),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn invalid_surface_wkb_dimension_is_structured() {
        let mut wkb = vec![1];
        wkb.extend_from_slice(&3_u32.to_le_bytes());
        wkb.extend_from_slice(&1_u32.to_le_bytes());
        wkb.extend_from_slice(&4_u32.to_le_bytes());
        for [x, y] in [[5.0_f64, 50.0_f64], [5.1, 50.0], [5.0, 50.1], [5.0, 50.0]] {
            wkb.extend_from_slice(&x.to_le_bytes());
            wkb.extend_from_slice(&y.to_le_bytes());
        }
        let error = wkb_surface_geometry_z_to_mesh(&wkb, fixture_frame())
            .expect_err("XY WKB must not enter the surface path");
        assert!(matches!(
            error,
            MeshError::Wkb(WkbError::CoordinateDimensionMismatch { .. })
        ));
    }

    #[test]
    fn extruded_mesh_rejects_non_positive_height() {
        let geometry = FootprintGeometry::Polygon(Polygon2D {
            exterior: square2(5.001, 50.001, 0.001),
            interiors: Vec::new(),
        });
        let error = footprint_to_extruded_mesh(&geometry, fixture_frame(), 0.0, 0.0)
            .expect_err("zero height should fail");
        assert!(error.to_string().contains("height_m"));
    }
}
