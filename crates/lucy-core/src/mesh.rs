use std::fmt;

use crate::tile::GeographicRegionDegrees;

const WGS84_A: f64 = 6_378_137.0;
const WGS84_F: f64 = 1.0 / 298.257_223_563;
const WKB_POINT: u32 = 1;
const WKB_LINESTRING: u32 = 2;
const WKB_POLYGON: u32 = 3;
const WKB_MULTIPOLYGON: u32 = 6;
const MIN_RING_POINTS: usize = 4;
const EPSILON: f64 = 1e-12;

/// Internal triangle mesh for a Phase 0 footprint or extruded footprint.
///
/// Coordinate assumptions:
/// - WKB input is OGC WKB, not EWKB, with 2D EPSG:4326 `[longitude, latitude]`
///   positions in decimal degrees.
/// - Vertices are converted to a local tangent meter frame using an
///   equirectangular approximation about `MeshFrame`.
/// - `z` is local up in meters.
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
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshFrame {
    pub origin_longitude_deg: f64,
    pub origin_latitude_deg: f64,
    pub origin_height_m: f64,
    origin_ecef: [f64; 3],
}

impl MeshFrame {
    pub fn from_tile_region(region: GeographicRegionDegrees) -> Self {
        let origin_longitude_deg = (region.west + region.east) / 2.0;
        let origin_latitude_deg = (region.south + region.north) / 2.0;
        let origin_height_m = region.min_height_m;
        let origin_ecef =
            geodetic_to_ecef(origin_longitude_deg, origin_latitude_deg, origin_height_m);
        Self {
            origin_longitude_deg,
            origin_latitude_deg,
            origin_height_m,
            origin_ecef,
        }
    }

    /// Column-major transform from Lucy's glTF-local axes [East, Up, -North]
    /// to WGS84 ECEF.
    pub fn gltf_to_ecef_transform(self) -> [f64; 16] {
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
            cos_lat * cos_lon,
            cos_lat * sin_lon,
            sin_lat,
            0.0,
            sin_lat * cos_lon,
            sin_lat * sin_lon,
            -cos_lat,
            0.0,
            x,
            y,
            z,
            1.0,
        ]
    }

    fn project(self, lon_deg: f64, lat_deg: f64) -> Result<[f32; 3], MeshError> {
        if !lon_deg.is_finite() || !lat_deg.is_finite() {
            return Err(MeshError::InvalidGeometry(
                "WKB coordinate values must be finite".to_string(),
            ));
        }

        let point = geodetic_to_ecef(lon_deg, lat_deg, self.origin_height_m);
        let delta = [
            point[0] - self.origin_ecef[0],
            point[1] - self.origin_ecef[1],
            point[2] - self.origin_ecef[2],
        ];
        let lon = self.origin_longitude_deg.to_radians();
        let lat = self.origin_latitude_deg.to_radians();
        let (sin_lon, cos_lon) = lon.sin_cos();
        let (sin_lat, cos_lat) = lat.sin_cos();
        let east = -sin_lon * delta[0] + cos_lon * delta[1];
        let north =
            -sin_lat * cos_lon * delta[0] - sin_lat * sin_lon * delta[1] + cos_lat * delta[2];
        let up = cos_lat * cos_lon * delta[0] + cos_lat * sin_lon * delta[1] + sin_lat * delta[2];
        Ok([east as f32, north as f32, up as f32])
    }
}

fn geodetic_to_ecef(lon_deg: f64, lat_deg: f64, height_m: f64) -> [f64; 3] {
    let lon = lon_deg.to_radians();
    let lat = lat_deg.to_radians();
    let (sin_lon, cos_lon) = lon.sin_cos();
    let (sin_lat, cos_lat) = lat.sin_cos();
    let e2 = WGS84_F * (2.0 - WGS84_F);
    let n = WGS84_A / (1.0 - e2 * sin_lat * sin_lat).sqrt();
    [
        (n + height_m) * cos_lat * cos_lon,
        (n + height_m) * cos_lat * sin_lon,
        (n * (1.0 - e2) + height_m) * sin_lat,
    ]
}

#[tracing::instrument(level = "debug", skip(wkb, frame), fields(input_wkb_bytes = wkb.len()))]
pub fn wkb_footprint_to_mesh(wkb: &[u8], frame: MeshFrame) -> Result<TriangleMesh, MeshError> {
    let geometry = read_wkb_geometry(wkb)?;

    let mut mesh = TriangleMesh::new();
    match geometry {
        Geometry::Polygon(polygon) => append_polygon_mesh(&mut mesh, &polygon, frame)?,
        Geometry::MultiPolygon(polygons) => {
            if polygons.is_empty() {
                return Err(MeshError::InvalidGeometry(
                    "MultiPolygon must contain at least one polygon".to_string(),
                ));
            }

            for polygon in polygons {
                append_polygon_mesh(&mut mesh, &polygon, frame)?;
            }
        }
    }

    if mesh.vertices.is_empty() || mesh.indices.is_empty() {
        return Err(MeshError::InvalidGeometry(
            "WKB geometry produced an empty mesh".to_string(),
        ));
    }

    tracing::debug!(
        vertex_count = mesh.vertices.len(),
        triangle_count = mesh.indices.len() / 3,
        "mesh generated"
    );
    Ok(mesh)
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
    validate_extrusion_heights(base_height_m, height_m)?;
    let geometry = read_wkb_geometry(wkb)?;
    let top_height_m = base_height_m + height_m;

    let mut mesh = TriangleMesh::new();
    match geometry {
        Geometry::Polygon(polygon) => {
            append_extruded_polygon_mesh(&mut mesh, &polygon, frame, base_height_m, top_height_m)?
        }
        Geometry::MultiPolygon(polygons) => {
            if polygons.is_empty() {
                return Err(MeshError::InvalidGeometry(
                    "MultiPolygon must contain at least one polygon".to_string(),
                ));
            }

            for polygon in polygons {
                append_extruded_polygon_mesh(
                    &mut mesh,
                    &polygon,
                    frame,
                    base_height_m,
                    top_height_m,
                )?;
            }
        }
    }

    if mesh.vertices.is_empty() || mesh.indices.is_empty() {
        return Err(MeshError::InvalidGeometry(
            "WKB geometry produced an empty extruded mesh".to_string(),
        ));
    }

    tracing::debug!(
        vertex_count = mesh.vertices.len(),
        triangle_count = mesh.indices.len() / 3,
        "extruded mesh generated"
    );
    Ok(mesh)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeshError {
    UnexpectedEof {
        offset: usize,
        needed: usize,
        len: usize,
    },
    InvalidByteOrder(u8),
    UnsupportedGeometryType {
        type_code: u32,
        type_name: &'static str,
    },
    UnsupportedGeometry(String),
    InvalidGeometry(String),
}

impl fmt::Display for MeshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MeshError::UnexpectedEof {
                offset,
                needed,
                len,
            } => write!(
                f,
                "unexpected end of WKB at byte {offset}: needed {needed} byte(s), len is {len}"
            ),
            MeshError::InvalidByteOrder(byte_order) => {
                write!(f, "invalid WKB byte order marker {byte_order}")
            }
            MeshError::UnsupportedGeometryType {
                type_code,
                type_name,
            } => write!(
                f,
                "unsupported WKB geometry type {type_name} ({type_code}); expected Polygon or MultiPolygon"
            ),
            MeshError::UnsupportedGeometry(message) => {
                write!(f, "unsupported WKB geometry: {message}")
            }
            MeshError::InvalidGeometry(message) => write!(f, "invalid WKB geometry: {message}"),
        }
    }
}

impl std::error::Error for MeshError {}

fn read_wkb_geometry(wkb: &[u8]) -> Result<Geometry, MeshError> {
    let mut reader = WkbReader::new(wkb);
    let geometry = reader.read_geometry()?;
    reader.expect_finished()?;
    Ok(geometry)
}

fn validate_extrusion_heights(base_height_m: f32, height_m: f32) -> Result<(), MeshError> {
    if !base_height_m.is_finite() || !height_m.is_finite() {
        return Err(MeshError::InvalidGeometry(
            "extrusion heights must be finite".to_string(),
        ));
    }

    if height_m <= 0.0 {
        return Err(MeshError::InvalidGeometry(
            "extrusion height_m must be greater than zero".to_string(),
        ));
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
enum Geometry {
    Polygon(Polygon),
    MultiPolygon(Vec<Polygon>),
}

#[derive(Debug, Clone, PartialEq)]
struct Polygon {
    rings: Vec<Vec<Point2>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Point2 {
    x: f64,
    y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteOrder {
    BigEndian,
    LittleEndian,
}

struct WkbReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> WkbReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_geometry(&mut self) -> Result<Geometry, MeshError> {
        let byte_order = self.read_byte_order()?;
        let type_code = self.read_u32(byte_order)?;
        if is_extended_wkb_type(type_code) {
            return Err(MeshError::UnsupportedGeometry(
                "extended WKB Z/M/SRID type flags are not supported in Phase 0".to_string(),
            ));
        }

        match type_code {
            WKB_POLYGON => Ok(Geometry::Polygon(self.read_polygon_body(byte_order)?)),
            WKB_MULTIPOLYGON => {
                let polygon_count = self.read_count(byte_order, "polygon count")?;
                let mut polygons = Vec::with_capacity(polygon_count);
                for _ in 0..polygon_count {
                    match self.read_geometry()? {
                        Geometry::Polygon(polygon) => polygons.push(polygon),
                        Geometry::MultiPolygon(_) => {
                            return Err(MeshError::InvalidGeometry(
                                "MultiPolygon members must be Polygon values".to_string(),
                            ));
                        }
                    }
                }
                Ok(Geometry::MultiPolygon(polygons))
            }
            _ => Err(MeshError::UnsupportedGeometryType {
                type_code,
                type_name: wkb_type_name(type_code),
            }),
        }
    }

    fn read_polygon_body(&mut self, byte_order: ByteOrder) -> Result<Polygon, MeshError> {
        let ring_count = self.read_count(byte_order, "ring count")?;
        if ring_count == 0 {
            return Err(MeshError::InvalidGeometry(
                "Polygon must contain at least one ring".to_string(),
            ));
        }

        let mut rings = Vec::with_capacity(ring_count);
        for _ in 0..ring_count {
            let point_count = self.read_count(byte_order, "ring point count")?;
            let mut ring = Vec::with_capacity(point_count);
            for _ in 0..point_count {
                ring.push(Point2 {
                    x: self.read_f64(byte_order)?,
                    y: self.read_f64(byte_order)?,
                });
            }
            rings.push(ring);
        }

        Ok(Polygon { rings })
    }

    fn expect_finished(&self) -> Result<(), MeshError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(MeshError::InvalidGeometry(format!(
                "WKB has {} trailing byte(s)",
                self.bytes.len() - self.offset
            )))
        }
    }

    fn read_byte_order(&mut self) -> Result<ByteOrder, MeshError> {
        match self.read_exact(1)?[0] {
            0 => Ok(ByteOrder::BigEndian),
            1 => Ok(ByteOrder::LittleEndian),
            value => Err(MeshError::InvalidByteOrder(value)),
        }
    }

    fn read_count(&mut self, byte_order: ByteOrder, field: &str) -> Result<usize, MeshError> {
        usize::try_from(self.read_u32(byte_order)?).map_err(|_| {
            MeshError::InvalidGeometry(format!("{field} does not fit in usize on this platform"))
        })
    }

    fn read_u32(&mut self, byte_order: ByteOrder) -> Result<u32, MeshError> {
        let bytes: [u8; 4] = self.read_exact(4)?.try_into().expect("slice length");
        Ok(match byte_order {
            ByteOrder::BigEndian => u32::from_be_bytes(bytes),
            ByteOrder::LittleEndian => u32::from_le_bytes(bytes),
        })
    }

    fn read_f64(&mut self, byte_order: ByteOrder) -> Result<f64, MeshError> {
        let bytes: [u8; 8] = self.read_exact(8)?.try_into().expect("slice length");
        Ok(match byte_order {
            ByteOrder::BigEndian => f64::from_be_bytes(bytes),
            ByteOrder::LittleEndian => f64::from_le_bytes(bytes),
        })
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], MeshError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(MeshError::UnexpectedEof {
                offset: self.offset,
                needed: len,
                len: self.bytes.len(),
            })?;

        if end > self.bytes.len() {
            return Err(MeshError::UnexpectedEof {
                offset: self.offset,
                needed: len,
                len: self.bytes.len(),
            });
        }

        let slice = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(slice)
    }
}

fn append_polygon_mesh(
    mesh: &mut TriangleMesh,
    polygon: &Polygon,
    frame: MeshFrame,
) -> Result<(), MeshError> {
    if polygon.rings.len() > 1 {
        return Err(MeshError::UnsupportedGeometry(
            "Polygon interior rings are not supported in Phase 0".to_string(),
        ));
    }

    let ring =
        normalized_exterior_ring(polygon.rings.first().ok_or_else(|| {
            MeshError::InvalidGeometry("Polygon has no exterior ring".to_string())
        })?)?;
    let triangles = triangulate_simple_ring(&ring)?;
    let base_index = u32::try_from(mesh.vertices.len()).map_err(|_| {
        MeshError::InvalidGeometry("mesh has too many vertices for u32 indices".to_string())
    })?;

    for point in &ring {
        mesh.vertices.push(MeshVertex {
            position: frame.project(point.x, point.y)?,
        });
    }

    for [a, b, c] in triangles {
        mesh.indices.extend_from_slice(&[
            base_index + a as u32,
            base_index + b as u32,
            base_index + c as u32,
        ]);
    }

    Ok(())
}

fn append_extruded_polygon_mesh(
    mesh: &mut TriangleMesh,
    polygon: &Polygon,
    frame: MeshFrame,
    base_height_m: f32,
    top_height_m: f32,
) -> Result<(), MeshError> {
    if polygon.rings.len() > 1 {
        return Err(MeshError::UnsupportedGeometry(
            "Polygon interior rings are not supported in Phase 0".to_string(),
        ));
    }

    let ring =
        normalized_exterior_ring(polygon.rings.first().ok_or_else(|| {
            MeshError::InvalidGeometry("Polygon has no exterior ring".to_string())
        })?)?;
    let top_triangles = triangulate_simple_ring(&ring)?;
    let bottom_base = u32::try_from(mesh.vertices.len()).map_err(|_| {
        MeshError::InvalidGeometry("mesh has too many vertices for u32 indices".to_string())
    })?;
    let top_base = bottom_base
        .checked_add(u32::try_from(ring.len()).map_err(|_| {
            MeshError::InvalidGeometry("ring has too many vertices for u32 indices".to_string())
        })?)
        .ok_or_else(|| MeshError::InvalidGeometry("mesh index overflowed u32".to_string()))?;

    for point in &ring {
        let mut position = frame.project(point.x, point.y)?;
        position[2] = base_height_m;
        mesh.vertices.push(MeshVertex { position });
    }

    for point in &ring {
        let mut position = frame.project(point.x, point.y)?;
        position[2] = top_height_m;
        mesh.vertices.push(MeshVertex { position });
    }

    for [a, b, c] in &top_triangles {
        mesh.indices.extend_from_slice(&[
            top_base + *a as u32,
            top_base + *b as u32,
            top_base + *c as u32,
        ]);
    }

    for [a, b, c] in &top_triangles {
        mesh.indices.extend_from_slice(&[
            bottom_base + *c as u32,
            bottom_base + *b as u32,
            bottom_base + *a as u32,
        ]);
    }

    for edge_start in 0..ring.len() {
        let edge_end = (edge_start + 1) % ring.len();
        let bottom_start = bottom_base + edge_start as u32;
        let bottom_end = bottom_base + edge_end as u32;
        let top_start = top_base + edge_start as u32;
        let top_end = top_base + edge_end as u32;

        mesh.indices.extend_from_slice(&[
            bottom_start,
            bottom_end,
            top_end,
            bottom_start,
            top_end,
            top_start,
        ]);
    }

    Ok(())
}

fn normalized_exterior_ring(ring: &[Point2]) -> Result<Vec<Point2>, MeshError> {
    if ring.len() < MIN_RING_POINTS {
        return Err(MeshError::InvalidGeometry(format!(
            "Polygon exterior ring must contain at least {MIN_RING_POINTS} points"
        )));
    }

    if ring.first() != ring.last() {
        return Err(MeshError::InvalidGeometry(
            "Polygon exterior ring must be closed".to_string(),
        ));
    }

    let mut points = ring[..ring.len() - 1].to_vec();
    points.dedup_by(|a, b| points_equal(*a, *b));

    if points.len() < 3 {
        return Err(MeshError::InvalidGeometry(
            "Polygon exterior ring must contain at least three distinct vertices".to_string(),
        ));
    }

    let area = signed_area(&points);
    if area.abs() < EPSILON {
        return Err(MeshError::InvalidGeometry(
            "Polygon exterior ring area is zero".to_string(),
        ));
    }

    if area < 0.0 {
        points.reverse();
    }

    Ok(points)
}

fn triangulate_simple_ring(points: &[Point2]) -> Result<Vec<[usize; 3]>, MeshError> {
    if points.len() == 3 {
        return Ok(vec![[0, 1, 2]]);
    }

    if is_convex_ring(points) {
        return Ok((1..points.len() - 1)
            .map(|index| [0, index, index + 1])
            .collect());
    }

    let ccw = signed_area(points) > 0.0;
    let mut remaining: Vec<usize> = (0..points.len()).collect();
    let mut triangles = Vec::with_capacity(points.len() - 2);

    while remaining.len() > 3 {
        let mut ear_index = None;

        for index in 0..remaining.len() {
            let prev = remaining[(index + remaining.len() - 1) % remaining.len()];
            let curr = remaining[index];
            let next = remaining[(index + 1) % remaining.len()];

            if !is_convex(points[prev], points[curr], points[next], ccw) {
                continue;
            }

            let contains_other_point = remaining.iter().copied().any(|candidate| {
                candidate != prev
                    && candidate != curr
                    && candidate != next
                    && point_in_triangle(
                        points[candidate],
                        points[prev],
                        points[curr],
                        points[next],
                    )
            });

            if !contains_other_point {
                ear_index = Some(index);
                triangles.push([prev, curr, next]);
                break;
            }
        }

        let Some(index) = ear_index else {
            return Err(MeshError::InvalidGeometry(
                "Polygon exterior ring could not be triangulated as a simple polygon".to_string(),
            ));
        };
        remaining.remove(index);
    }

    triangles.push([remaining[0], remaining[1], remaining[2]]);
    Ok(triangles)
}

fn signed_area(points: &[Point2]) -> f64 {
    points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
        .map(|(a, b)| (a.x * b.y) - (b.x * a.y))
        .sum::<f64>()
        / 2.0
}

fn is_convex(a: Point2, b: Point2, c: Point2, ccw: bool) -> bool {
    let cross = cross(a, b, c);
    if ccw {
        cross > EPSILON
    } else {
        cross < -EPSILON
    }
}

fn is_convex_ring(points: &[Point2]) -> bool {
    if points.len() < 3 || signed_area(points) <= EPSILON {
        return false;
    }

    (0..points.len()).all(|index| {
        let prev = points[(index + points.len() - 1) % points.len()];
        let curr = points[index];
        let next = points[(index + 1) % points.len()];
        cross(prev, curr, next) > EPSILON
    })
}

fn point_in_triangle(p: Point2, a: Point2, b: Point2, c: Point2) -> bool {
    let c1 = cross(a, b, p);
    let c2 = cross(b, c, p);
    let c3 = cross(c, a, p);

    let has_negative = c1 < -EPSILON || c2 < -EPSILON || c3 < -EPSILON;
    let has_positive = c1 > EPSILON || c2 > EPSILON || c3 > EPSILON;
    !(has_negative && has_positive)
}

fn cross(a: Point2, b: Point2, c: Point2) -> f64 {
    ((b.x - a.x) * (c.y - a.y)) - ((b.y - a.y) * (c.x - a.x))
}

fn points_equal(a: Point2, b: Point2) -> bool {
    (a.x - b.x).abs() < EPSILON && (a.y - b.y).abs() < EPSILON
}

fn is_extended_wkb_type(type_code: u32) -> bool {
    type_code >= 1_000 || (type_code & 0xE000_0000) != 0
}

fn wkb_type_name(type_code: u32) -> &'static str {
    match type_code {
        WKB_POINT => "Point",
        WKB_LINESTRING => "LineString",
        WKB_POLYGON => "Polygon",
        WKB_MULTIPOLYGON => "MultiPolygon",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_frame() -> MeshFrame {
        MeshFrame::from_tile_region(GeographicRegionDegrees {
            west: -122.400525,
            south: 37.79310,
            east: -122.400525,
            north: 37.79310,
            min_height_m: 0.0,
            max_height_m: 100.0,
        })
    }

    #[test]
    fn tile_frame_origin_is_local_zero_and_transform_targets_ecef() {
        let frame = fixture_frame();
        let origin = frame
            .project(frame.origin_longitude_deg, frame.origin_latitude_deg)
            .expect("origin should project");
        assert!(origin.iter().all(|component| component.abs() < 1.0e-5));

        let transform = frame.gltf_to_ecef_transform();
        assert_eq!(&transform[12..15], &frame.origin_ecef);
    }

    fn sansome_office_ring() -> Vec<(f64, f64)> {
        vec![
            (-122.40120, 37.79252),
            (-122.40082, 37.79252),
            (-122.40082, 37.79282),
            (-122.40120, 37.79282),
            (-122.40120, 37.79252),
        ]
    }

    fn little_endian_polygon_wkb(ring: &[(f64, f64)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(1);
        bytes.extend_from_slice(&WKB_POLYGON.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&(ring.len() as u32).to_le_bytes());
        for (x, y) in ring {
            bytes.extend_from_slice(&x.to_le_bytes());
            bytes.extend_from_slice(&y.to_le_bytes());
        }
        bytes
    }

    fn little_endian_multipolygon_wkb(polygons: &[Vec<(f64, f64)>]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(1);
        bytes.extend_from_slice(&WKB_MULTIPOLYGON.to_le_bytes());
        bytes.extend_from_slice(&(polygons.len() as u32).to_le_bytes());
        for polygon in polygons {
            bytes.extend_from_slice(&little_endian_polygon_wkb(polygon));
        }
        bytes
    }

    #[test]
    fn fixture_multipolygon_wkb_produces_non_empty_mesh() {
        let wkb = little_endian_multipolygon_wkb(&[sansome_office_ring()]);
        let mesh = wkb_footprint_to_mesh(&wkb, fixture_frame()).expect("mesh should build");

        assert_eq!(mesh.vertices.len(), 4);
        assert_eq!(mesh.indices.len(), 6);
        assert_eq!(mesh.indices, vec![0, 1, 2, 0, 2, 3]);
        assert!(
            mesh.vertices
                .iter()
                .all(|vertex| vertex.position.iter().all(|value| value.is_finite()))
        );
    }

    #[test]
    fn clockwise_polygon_wkb_triangulates() {
        let mut ring = sansome_office_ring();
        ring.reverse();
        let wkb = little_endian_polygon_wkb(&ring);
        let mesh = wkb_footprint_to_mesh(&wkb, fixture_frame()).expect("mesh should build");

        assert_eq!(mesh.vertices.len(), 4);
        assert_eq!(mesh.indices.len(), 6);
        assert_eq!(mesh.indices, vec![0, 1, 2, 0, 2, 3]);
    }

    #[test]
    fn fixture_multipolygon_wkb_extrudes_to_building_mesh() {
        let wkb = little_endian_multipolygon_wkb(&[sansome_office_ring()]);
        let mesh = wkb_footprint_to_extruded_mesh(&wkb, fixture_frame(), 4.0, 96.0)
            .expect("extruded mesh should build");

        assert_eq!(mesh.vertices.len(), 8);
        assert_eq!(mesh.indices.len(), 36);
        assert_eq!(&mesh.indices[0..6], &[4, 5, 6, 4, 6, 7]);
        assert_eq!(&mesh.indices[6..12], &[2, 1, 0, 3, 2, 0]);
        assert!(
            mesh.vertices[0..4]
                .iter()
                .all(|vertex| vertex.position[2] == 4.0)
        );
        assert!(
            mesh.vertices[4..8]
                .iter()
                .all(|vertex| vertex.position[2] == 100.0)
        );
    }

    #[test]
    fn extruded_mesh_rejects_non_positive_height() {
        let wkb = little_endian_multipolygon_wkb(&[sansome_office_ring()]);
        let error = wkb_footprint_to_extruded_mesh(&wkb, fixture_frame(), 0.0, 0.0)
            .expect_err("zero height should fail");

        assert!(
            error.to_string().contains("height_m"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn unsupported_point_wkb_returns_clear_error() {
        let mut wkb = Vec::new();
        wkb.push(1);
        wkb.extend_from_slice(&WKB_POINT.to_le_bytes());
        wkb.extend_from_slice(&0.0_f64.to_le_bytes());
        wkb.extend_from_slice(&0.0_f64.to_le_bytes());

        let error = wkb_footprint_to_mesh(&wkb, fixture_frame()).expect_err("point should fail");
        assert!(
            error
                .to_string()
                .contains("unsupported WKB geometry type Point"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn polygon_with_holes_returns_clear_error() {
        let exterior = sansome_office_ring();
        let interior = vec![
            (-122.40110, 37.79260),
            (-122.40095, 37.79260),
            (-122.40095, 37.79270),
            (-122.40110, 37.79270),
            (-122.40110, 37.79260),
        ];

        let mut wkb = Vec::new();
        wkb.push(1);
        wkb.extend_from_slice(&WKB_POLYGON.to_le_bytes());
        wkb.extend_from_slice(&2_u32.to_le_bytes());
        for ring in [exterior, interior] {
            wkb.extend_from_slice(&(ring.len() as u32).to_le_bytes());
            for (x, y) in ring {
                wkb.extend_from_slice(&x.to_le_bytes());
                wkb.extend_from_slice(&y.to_le_bytes());
            }
        }

        let error = wkb_footprint_to_mesh(&wkb, fixture_frame()).expect_err("hole should fail");
        assert!(
            error.to_string().contains("interior rings"),
            "unexpected error: {error}"
        );
    }
}
