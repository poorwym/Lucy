use std::fmt;

const WKB_LINE_STRING: u32 = 2;
const WKB_POLYGON: u32 = 3;
const WKB_MULTI_LINE_STRING: u32 = 5;
const WKB_MULTIPOLYGON: u32 = 6;
const EWKB_Z_FLAG: u32 = 0x8000_0000;
const EWKB_M_FLAG: u32 = 0x4000_0000;
const EWKB_SRID_FLAG: u32 = 0x2000_0000;
const EWKB_FLAG_MASK: u32 = EWKB_Z_FLAG | EWKB_M_FLAG | EWKB_SRID_FLAG;

/// A two-dimensional source coordinate.
///
/// For Lucy's normalized PostGIS query contract this is `[longitude, latitude]`
/// in degrees, but the WKB decoder intentionally does not attach a CRS to the
/// values. CRS validation and transformation belong to the source adapter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point2D {
    pub x: f64,
    pub y: f64,
}

/// A three-dimensional source coordinate whose third ordinate is Z, not M.
///
/// A `surface_geometry_z` PostGIS query normalizes these values to its explicit
/// EPSG:4979 target contract, so Lucy interprets them as longitude degrees,
/// latitude degrees, and ellipsoidal height metres when constructing a mesh.
/// Datum accuracy (including the configured ETRS89 approximation, if any) is
/// owned by the source adapter rather than the WKB decoder.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point3D {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ring2D {
    /// WKB rings retain their closing coordinate. Mesh validation removes it
    /// only after confirming that the ring is closed.
    pub points: Vec<Point2D>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LineString2D {
    pub points: Vec<Point2D>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MultiLineString2D {
    pub lines: Vec<LineString2D>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ring3D {
    /// WKB rings retain their closing coordinate. Mesh validation removes it
    /// only after confirming that the ring is closed.
    pub points: Vec<Point3D>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Polygon2D {
    pub exterior: Ring2D,
    pub interiors: Vec<Ring2D>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Polygon3D {
    pub exterior: Ring3D,
    pub interiors: Vec<Ring3D>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FootprintGeometry {
    Polygon(Polygon2D),
    MultiPolygon(Vec<Polygon2D>),
}

impl FootprintGeometry {
    pub fn polygons(&self) -> &[Polygon2D] {
        match self {
            Self::Polygon(polygon) => std::slice::from_ref(polygon),
            Self::MultiPolygon(polygons) => polygons,
        }
    }
}

/// One tile-contained footprint area together with the portions of the
/// original feature boundary that survive inside that tile.
#[derive(Debug, Clone, PartialEq)]
pub struct FootprintFragment {
    pub geometry: FootprintGeometry,
    pub source_boundary: MultiLineString2D,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceGeometryZ {
    Polygon(Polygon3D),
    MultiPolygon(Vec<Polygon3D>),
}

/// Geometry after a source adapter has validated and normalized its coordinate
/// reference system. Downstream Lucy code can rely on each variant's coordinate
/// semantics without carrying an SRID or performing another CRS conversion.
#[derive(Debug, Clone, PartialEq)]
pub enum NormalizedGeometry {
    /// Tile-clipped EPSG:4326 longitude/latitude in degrees plus the surviving
    /// original feature boundary. Elevation is supplied by the extrusion model
    /// as local metres above the source region's base height.
    GeographicFootprint(FootprintFragment),
    /// EPSG:4979 longitude/latitude in degrees plus ellipsoidal height metres.
    GeodeticSurface(SurfaceGeometryZ),
}

impl SurfaceGeometryZ {
    pub fn polygons(&self) -> &[Polygon3D] {
        match self {
            Self::Polygon(polygon) => std::slice::from_ref(polygon),
            Self::MultiPolygon(polygons) => polygons,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinateDimension {
    Xy,
    Xyz,
    Xym,
    Xyzm,
}

impl CoordinateDimension {
    fn ordinate_count(self) -> usize {
        match self {
            Self::Xy => 2,
            Self::Xyz | Self::Xym => 3,
            Self::Xyzm => 4,
        }
    }

    fn has_z(self) -> bool {
        matches!(self, Self::Xyz | Self::Xyzm)
    }

    fn has_m(self) -> bool {
        matches!(self, Self::Xym | Self::Xyzm)
    }
}

impl fmt::Display for CoordinateDimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Xy => write!(f, "XY"),
            Self::Xyz => write!(f, "XYZ"),
            Self::Xym => write!(f, "XYM"),
            Self::Xyzm => write!(f, "XYZM"),
        }
    }
}

/// Decode an OGC/ISO WKB or PostGIS EWKB Polygon/MultiPolygon footprint.
///
/// The footprint path deliberately requires exactly XY coordinates. It never
/// accepts XYZ or silently discards Z.
pub fn decode_footprint_wkb(wkb: &[u8]) -> Result<FootprintGeometry, WkbError> {
    let geometry = decode_polygonal_wkb(wkb, CoordinateDimension::Xy)?;
    match geometry {
        RawPolygonal::Polygon(polygon) => Ok(FootprintGeometry::Polygon(polygon.into_2d()?)),
        RawPolygonal::MultiPolygon(polygons) => Ok(FootprintGeometry::MultiPolygon(
            polygons
                .into_iter()
                .map(RawPolygon::into_2d)
                .collect::<Result<_, _>>()?,
        )),
    }
}

/// Decode an OGC/ISO WKB or PostGIS EWKB PolygonZ/MultiPolygonZ.
///
/// Exactly XYZ is required. XY, XYM, and XYZM inputs return a dimension error,
/// so native surface geometry can never be silently flattened or confuse M for
/// elevation.
pub fn decode_surface_geometry_z_wkb(wkb: &[u8]) -> Result<SurfaceGeometryZ, WkbError> {
    let geometry = decode_polygonal_wkb(wkb, CoordinateDimension::Xyz)?;
    match geometry {
        RawPolygonal::Polygon(polygon) => Ok(SurfaceGeometryZ::Polygon(polygon.into_3d()?)),
        RawPolygonal::MultiPolygon(polygons) => Ok(SurfaceGeometryZ::MultiPolygon(
            polygons
                .into_iter()
                .map(RawPolygon::into_3d)
                .collect::<Result<_, _>>()?,
        )),
    }
}

/// Decode an OGC/ISO WKB or PostGIS EWKB MultiLineString.
///
/// Exactly XY is required for both the outer geometry and every LineString
/// member. Empty MultiLineStrings and empty LineString members are preserved.
pub fn decode_multi_line_string_wkb(wkb: &[u8]) -> Result<MultiLineString2D, WkbError> {
    let mut reader = WkbReader::new(wkb);
    let geometry = reader.read_multi_line_string(CoordinateDimension::Xy)?;
    reader.expect_finished()?;
    Ok(geometry)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WkbError {
    UnexpectedEof {
        offset: usize,
        needed: usize,
        len: usize,
    },
    InvalidByteOrder(u8),
    UnsupportedGeometryType {
        type_code: u32,
        base_type: u32,
    },
    UnsupportedTypeEncoding(u32),
    CoordinateDimensionMismatch {
        expected: CoordinateDimension,
        actual: CoordinateDimension,
        type_code: u32,
    },
    MixedMultiPolygonDimension {
        expected: CoordinateDimension,
        actual: CoordinateDimension,
        member_index: usize,
    },
    MultiPolygonMemberIsNotPolygon {
        member_index: usize,
        base_type: u32,
    },
    GeometryIsNotMultiLineString {
        type_code: u32,
        base_type: u32,
    },
    MixedMultiLineStringDimension {
        expected: CoordinateDimension,
        actual: CoordinateDimension,
        member_index: usize,
    },
    MultiLineStringMemberIsNotLineString {
        member_index: usize,
        base_type: u32,
    },
    PolygonHasNoRings,
    MultiPolygonHasNoPolygons,
    CountOverflow(&'static str),
    NonFiniteCoordinate {
        byte_offset: usize,
        ordinate: &'static str,
    },
    TrailingBytes(usize),
}

impl fmt::Display for WkbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof {
                offset,
                needed,
                len,
            } => write!(
                f,
                "unexpected end of WKB at byte {offset}: needed {needed} byte(s), len is {len}"
            ),
            Self::InvalidByteOrder(value) => write!(f, "invalid WKB byte order marker {value}"),
            Self::UnsupportedGeometryType {
                type_code,
                base_type,
            } => write!(
                f,
                "unsupported WKB geometry type {base_type} (encoded type {type_code}); expected Polygon or MultiPolygon"
            ),
            Self::UnsupportedTypeEncoding(type_code) => {
                write!(f, "unsupported WKB type encoding {type_code}")
            }
            Self::CoordinateDimensionMismatch {
                expected,
                actual,
                type_code,
            } => write!(
                f,
                "WKB type {type_code} has {actual} coordinates; expected {expected}"
            ),
            Self::MixedMultiPolygonDimension {
                expected,
                actual,
                member_index,
            } => write!(
                f,
                "MultiPolygon member {member_index} has {actual} coordinates; outer geometry uses {expected}"
            ),
            Self::MultiPolygonMemberIsNotPolygon {
                member_index,
                base_type,
            } => write!(
                f,
                "MultiPolygon member {member_index} has geometry type {base_type}; expected Polygon"
            ),
            Self::GeometryIsNotMultiLineString {
                type_code,
                base_type,
            } => write!(
                f,
                "WKB geometry type {base_type} (encoded type {type_code}) is not a MultiLineString"
            ),
            Self::MixedMultiLineStringDimension {
                expected,
                actual,
                member_index,
            } => write!(
                f,
                "MultiLineString member {member_index} has {actual} coordinates; outer geometry uses {expected}"
            ),
            Self::MultiLineStringMemberIsNotLineString {
                member_index,
                base_type,
            } => write!(
                f,
                "MultiLineString member {member_index} has geometry type {base_type}; expected LineString"
            ),
            Self::PolygonHasNoRings => write!(f, "Polygon must contain at least one ring"),
            Self::MultiPolygonHasNoPolygons => {
                write!(f, "MultiPolygon must contain at least one Polygon")
            }
            Self::CountOverflow(field) => {
                write!(f, "WKB {field} exceeds addressable input size")
            }
            Self::NonFiniteCoordinate {
                byte_offset,
                ordinate,
            } => write!(
                f,
                "WKB {ordinate} coordinate at byte {byte_offset} is not finite"
            ),
            Self::TrailingBytes(count) => write!(f, "WKB has {count} trailing byte(s)"),
        }
    }
}

impl std::error::Error for WkbError {}

#[derive(Debug, Clone, Copy, PartialEq)]
struct RawPoint {
    x: f64,
    y: f64,
    z: Option<f64>,
    m: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
struct RawPolygon {
    rings: Vec<Vec<RawPoint>>,
}

impl RawPolygon {
    fn into_2d(self) -> Result<Polygon2D, WkbError> {
        let mut rings = self.rings.into_iter();
        let exterior = Ring2D {
            points: rings
                .next()
                .ok_or(WkbError::PolygonHasNoRings)?
                .into_iter()
                .map(|point| Point2D {
                    x: point.x,
                    y: point.y,
                })
                .collect(),
        };
        let interiors = rings
            .map(|ring| Ring2D {
                points: ring
                    .into_iter()
                    .map(|point| Point2D {
                        x: point.x,
                        y: point.y,
                    })
                    .collect(),
            })
            .collect();
        Ok(Polygon2D {
            exterior,
            interiors,
        })
    }

    fn into_3d(self) -> Result<Polygon3D, WkbError> {
        fn convert_ring(ring: Vec<RawPoint>) -> Result<Ring3D, WkbError> {
            Ok(Ring3D {
                points: ring
                    .into_iter()
                    .map(|point| {
                        Ok(Point3D {
                            x: point.x,
                            y: point.y,
                            z: point.z.ok_or(WkbError::CoordinateDimensionMismatch {
                                expected: CoordinateDimension::Xyz,
                                actual: CoordinateDimension::Xy,
                                type_code: WKB_POLYGON,
                            })?,
                        })
                    })
                    .collect::<Result<_, WkbError>>()?,
            })
        }

        let mut rings = self.rings.into_iter();
        let exterior = convert_ring(rings.next().ok_or(WkbError::PolygonHasNoRings)?)?;
        let interiors = rings.map(convert_ring).collect::<Result<_, _>>()?;
        Ok(Polygon3D {
            exterior,
            interiors,
        })
    }
}

enum RawPolygonal {
    Polygon(RawPolygon),
    MultiPolygon(Vec<RawPolygon>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteOrder {
    BigEndian,
    LittleEndian,
}

#[derive(Debug, Clone, Copy)]
struct WkbHeader {
    byte_order: ByteOrder,
    type_code: u32,
    base_type: u32,
    dimension: CoordinateDimension,
}

fn decode_polygonal_wkb(
    wkb: &[u8],
    expected_dimension: CoordinateDimension,
) -> Result<RawPolygonal, WkbError> {
    let mut reader = WkbReader::new(wkb);
    let geometry = reader.read_polygonal(expected_dimension)?;
    reader.expect_finished()?;
    Ok(geometry)
}

struct WkbReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> WkbReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_polygonal(
        &mut self,
        expected_dimension: CoordinateDimension,
    ) -> Result<RawPolygonal, WkbError> {
        let header = self.read_header()?;
        if header.dimension != expected_dimension {
            return Err(WkbError::CoordinateDimensionMismatch {
                expected: expected_dimension,
                actual: header.dimension,
                type_code: header.type_code,
            });
        }

        match header.base_type {
            WKB_POLYGON => Ok(RawPolygonal::Polygon(self.read_polygon_body(header)?)),
            WKB_MULTIPOLYGON => {
                let polygon_count = self.read_count(header.byte_order, "polygon count")?;
                if polygon_count == 0 {
                    return Err(WkbError::MultiPolygonHasNoPolygons);
                }
                self.ensure_minimum_items(polygon_count, 5, "polygon count")?;
                let mut polygons = Vec::with_capacity(polygon_count);
                for member_index in 0..polygon_count {
                    let member = self.read_header()?;
                    if member.base_type != WKB_POLYGON {
                        return Err(WkbError::MultiPolygonMemberIsNotPolygon {
                            member_index,
                            base_type: member.base_type,
                        });
                    }
                    if member.dimension != header.dimension {
                        return Err(WkbError::MixedMultiPolygonDimension {
                            expected: header.dimension,
                            actual: member.dimension,
                            member_index,
                        });
                    }
                    polygons.push(self.read_polygon_body(member)?);
                }
                Ok(RawPolygonal::MultiPolygon(polygons))
            }
            base_type => Err(WkbError::UnsupportedGeometryType {
                type_code: header.type_code,
                base_type,
            }),
        }
    }

    fn read_multi_line_string(
        &mut self,
        expected_dimension: CoordinateDimension,
    ) -> Result<MultiLineString2D, WkbError> {
        let header = self.read_header()?;
        if header.dimension != expected_dimension {
            return Err(WkbError::CoordinateDimensionMismatch {
                expected: expected_dimension,
                actual: header.dimension,
                type_code: header.type_code,
            });
        }
        if header.base_type != WKB_MULTI_LINE_STRING {
            return Err(WkbError::GeometryIsNotMultiLineString {
                type_code: header.type_code,
                base_type: header.base_type,
            });
        }

        let line_count = self.read_count(header.byte_order, "line string count")?;
        self.ensure_minimum_items(line_count, 5, "line string count")?;
        let mut lines = Vec::with_capacity(line_count);
        for member_index in 0..line_count {
            let member = self.read_header()?;
            if member.base_type != WKB_LINE_STRING {
                return Err(WkbError::MultiLineStringMemberIsNotLineString {
                    member_index,
                    base_type: member.base_type,
                });
            }
            if member.dimension != header.dimension {
                return Err(WkbError::MixedMultiLineStringDimension {
                    expected: header.dimension,
                    actual: member.dimension,
                    member_index,
                });
            }
            lines.push(self.read_line_string_body(member)?);
        }

        Ok(MultiLineString2D { lines })
    }

    fn read_header(&mut self) -> Result<WkbHeader, WkbError> {
        let byte_order = self.read_byte_order()?;
        let type_code = self.read_u32(byte_order)?;
        let has_ewkb_flags = type_code & EWKB_FLAG_MASK != 0;

        let (base_type, dimension, has_srid) = if has_ewkb_flags {
            let base_type = type_code & !EWKB_FLAG_MASK;
            if base_type >= 1_000 {
                return Err(WkbError::UnsupportedTypeEncoding(type_code));
            }
            let has_z = type_code & EWKB_Z_FLAG != 0;
            let has_m = type_code & EWKB_M_FLAG != 0;
            let dimension = match (has_z, has_m) {
                (false, false) => CoordinateDimension::Xy,
                (true, false) => CoordinateDimension::Xyz,
                (false, true) => CoordinateDimension::Xym,
                (true, true) => CoordinateDimension::Xyzm,
            };
            (base_type, dimension, type_code & EWKB_SRID_FLAG != 0)
        } else {
            let dimension_code = type_code / 1_000;
            let dimension = match dimension_code {
                0 => CoordinateDimension::Xy,
                1 => CoordinateDimension::Xyz,
                2 => CoordinateDimension::Xym,
                3 => CoordinateDimension::Xyzm,
                _ => return Err(WkbError::UnsupportedTypeEncoding(type_code)),
            };
            (type_code % 1_000, dimension, false)
        };

        if has_srid {
            // The source adapter owns SRID validation. Consuming the value here
            // is nevertheless required to parse EWKB without shifting the body.
            let _embedded_srid = self.read_u32(byte_order)?;
        }

        Ok(WkbHeader {
            byte_order,
            type_code,
            base_type,
            dimension,
        })
    }

    fn read_polygon_body(&mut self, header: WkbHeader) -> Result<RawPolygon, WkbError> {
        let ring_count = self.read_count(header.byte_order, "ring count")?;
        if ring_count == 0 {
            return Err(WkbError::PolygonHasNoRings);
        }
        self.ensure_minimum_items(ring_count, 4, "ring count")?;

        let mut rings = Vec::with_capacity(ring_count);
        for _ in 0..ring_count {
            let point_count = self.read_count(header.byte_order, "ring point count")?;
            let coordinate_bytes = header
                .dimension
                .ordinate_count()
                .checked_mul(std::mem::size_of::<f64>())
                .ok_or(WkbError::CountOverflow("coordinate width"))?;
            self.ensure_minimum_items(point_count, coordinate_bytes, "ring point count")?;

            let mut ring = Vec::with_capacity(point_count);
            for _ in 0..point_count {
                ring.push(self.read_point(header)?);
            }
            rings.push(ring);
        }
        Ok(RawPolygon { rings })
    }

    fn read_line_string_body(&mut self, header: WkbHeader) -> Result<LineString2D, WkbError> {
        let point_count = self.read_count(header.byte_order, "line string point count")?;
        let coordinate_bytes = header
            .dimension
            .ordinate_count()
            .checked_mul(std::mem::size_of::<f64>())
            .ok_or(WkbError::CountOverflow("coordinate width"))?;
        self.ensure_minimum_items(point_count, coordinate_bytes, "line string point count")?;

        let mut points = Vec::with_capacity(point_count);
        for _ in 0..point_count {
            let point = self.read_point(header)?;
            points.push(Point2D {
                x: point.x,
                y: point.y,
            });
        }
        Ok(LineString2D { points })
    }

    fn read_point(&mut self, header: WkbHeader) -> Result<RawPoint, WkbError> {
        let x = self.read_finite_f64(header.byte_order, "X")?;
        let y = self.read_finite_f64(header.byte_order, "Y")?;
        let z = if header.dimension.has_z() {
            Some(self.read_finite_f64(header.byte_order, "Z")?)
        } else {
            None
        };
        let m = if header.dimension.has_m() {
            Some(self.read_finite_f64(header.byte_order, "M")?)
        } else {
            None
        };
        Ok(RawPoint { x, y, z, m })
    }

    fn expect_finished(&self) -> Result<(), WkbError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(WkbError::TrailingBytes(self.bytes.len() - self.offset))
        }
    }

    fn read_byte_order(&mut self) -> Result<ByteOrder, WkbError> {
        match self.read_exact(1)?[0] {
            0 => Ok(ByteOrder::BigEndian),
            1 => Ok(ByteOrder::LittleEndian),
            value => Err(WkbError::InvalidByteOrder(value)),
        }
    }

    fn read_count(
        &mut self,
        byte_order: ByteOrder,
        field: &'static str,
    ) -> Result<usize, WkbError> {
        usize::try_from(self.read_u32(byte_order)?).map_err(|_| WkbError::CountOverflow(field))
    }

    fn read_u32(&mut self, byte_order: ByteOrder) -> Result<u32, WkbError> {
        let bytes: [u8; 4] = self.read_exact(4)?.try_into().expect("slice length");
        Ok(match byte_order {
            ByteOrder::BigEndian => u32::from_be_bytes(bytes),
            ByteOrder::LittleEndian => u32::from_le_bytes(bytes),
        })
    }

    fn read_finite_f64(
        &mut self,
        byte_order: ByteOrder,
        ordinate: &'static str,
    ) -> Result<f64, WkbError> {
        let byte_offset = self.offset;
        let bytes: [u8; 8] = self.read_exact(8)?.try_into().expect("slice length");
        let value = match byte_order {
            ByteOrder::BigEndian => f64::from_be_bytes(bytes),
            ByteOrder::LittleEndian => f64::from_le_bytes(bytes),
        };
        if !value.is_finite() {
            return Err(WkbError::NonFiniteCoordinate {
                byte_offset,
                ordinate,
            });
        }
        Ok(value)
    }

    fn ensure_minimum_items(
        &self,
        count: usize,
        minimum_item_bytes: usize,
        field: &'static str,
    ) -> Result<(), WkbError> {
        let needed = count
            .checked_mul(minimum_item_bytes)
            .ok_or(WkbError::CountOverflow(field))?;
        let remaining = self.bytes.len() - self.offset;
        if needed > remaining {
            return Err(WkbError::UnexpectedEof {
                offset: self.offset,
                needed,
                len: self.bytes.len(),
            });
        }
        Ok(())
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], WkbError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(WkbError::UnexpectedEof {
                offset: self.offset,
                needed: len,
                len: self.bytes.len(),
            })?;
        if end > self.bytes.len() {
            return Err(WkbError::UnexpectedEof {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn push_u32(bytes: &mut Vec<u8>, value: u32, little_endian: bool) {
        if little_endian {
            bytes.extend_from_slice(&value.to_le_bytes());
        } else {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
    }

    fn push_f64(bytes: &mut Vec<u8>, value: f64, little_endian: bool) {
        if little_endian {
            bytes.extend_from_slice(&value.to_le_bytes());
        } else {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
    }

    fn write_header(
        bytes: &mut Vec<u8>,
        type_code: u32,
        little_endian: bool,
        embedded_srid: Option<u32>,
    ) {
        bytes.push(u8::from(little_endian));
        push_u32(bytes, type_code, little_endian);
        if type_code & EWKB_SRID_FLAG != 0 {
            push_u32(
                bytes,
                embedded_srid.expect("EWKB SRID flag requires a test SRID"),
                little_endian,
            );
        }
    }

    fn write_line_string(
        type_code: u32,
        points: &[[f64; 4]],
        little_endian: bool,
        embedded_srid: Option<u32>,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_header(&mut bytes, type_code, little_endian, embedded_srid);
        push_u32(&mut bytes, points.len() as u32, little_endian);

        let ordinate_count = if type_code & EWKB_FLAG_MASK != 0 {
            2 + usize::from(type_code & EWKB_Z_FLAG != 0)
                + usize::from(type_code & EWKB_M_FLAG != 0)
        } else {
            match type_code / 1_000 {
                0 => 2,
                1 | 2 => 3,
                3 => 4,
                _ => panic!("unsupported test WKB type code {type_code}"),
            }
        };
        for point in points {
            for value in &point[..ordinate_count] {
                push_f64(&mut bytes, *value, little_endian);
            }
        }
        bytes
    }

    fn write_multi_line_string(
        type_code: u32,
        lines: &[Vec<u8>],
        little_endian: bool,
        embedded_srid: Option<u32>,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_header(&mut bytes, type_code, little_endian, embedded_srid);
        push_u32(&mut bytes, lines.len() as u32, little_endian);
        for line in lines {
            bytes.extend_from_slice(line);
        }
        bytes
    }

    fn write_polygon(type_code: u32, rings: &[Vec<[f64; 3]>], little_endian: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(u8::from(little_endian));
        if little_endian {
            bytes.extend_from_slice(&type_code.to_le_bytes());
            bytes.extend_from_slice(&(rings.len() as u32).to_le_bytes());
        } else {
            bytes.extend_from_slice(&type_code.to_be_bytes());
            bytes.extend_from_slice(&(rings.len() as u32).to_be_bytes());
        }
        let has_z = type_code == 1_003 || type_code & EWKB_Z_FLAG != 0;
        for ring in rings {
            if little_endian {
                bytes.extend_from_slice(&(ring.len() as u32).to_le_bytes());
            } else {
                bytes.extend_from_slice(&(ring.len() as u32).to_be_bytes());
            }
            for point in ring {
                for value in if has_z { &point[..3] } else { &point[..2] } {
                    if little_endian {
                        bytes.extend_from_slice(&value.to_le_bytes());
                    } else {
                        bytes.extend_from_slice(&value.to_be_bytes());
                    }
                }
            }
        }
        bytes
    }

    fn square(z: f64) -> Vec<[f64; 3]> {
        vec![
            [5.0, 50.0, z],
            [5.001, 50.0, z],
            [5.001, 50.001, z],
            [5.0, 50.001, z],
            [5.0, 50.0, z],
        ]
    }

    #[test]
    fn decodes_polygon_z_and_preserves_interior_ring_z() {
        let wkb = write_polygon(1_003, &[square(42.0), square(43.0)], true);
        let geometry = decode_surface_geometry_z_wkb(&wkb).expect("PolygonZ should decode");
        let SurfaceGeometryZ::Polygon(polygon) = geometry else {
            panic!("expected PolygonZ")
        };

        assert_eq!(polygon.exterior.points[0].z, 42.0);
        assert_eq!(polygon.interiors.len(), 1);
        assert_eq!(polygon.interiors[0].points[2].z, 43.0);
    }

    #[test]
    fn decodes_big_endian_multipolygon_z() {
        let polygon = write_polygon(1_003, &[square(12.5)], false);
        let mut wkb = vec![0];
        wkb.extend_from_slice(&1_006_u32.to_be_bytes());
        wkb.extend_from_slice(&1_u32.to_be_bytes());
        wkb.extend_from_slice(&polygon);

        let geometry = decode_surface_geometry_z_wkb(&wkb).expect("MultiPolygonZ should decode");
        let SurfaceGeometryZ::MultiPolygon(polygons) = geometry else {
            panic!("expected MultiPolygonZ")
        };
        assert_eq!(polygons.len(), 1);
        assert_eq!(polygons[0].exterior.points[3].z, 12.5);
    }

    #[test]
    fn decodes_ewkb_polygon_z() {
        let wkb = write_polygon(EWKB_Z_FLAG | WKB_POLYGON, &[square(7.0)], true);
        let geometry = decode_surface_geometry_z_wkb(&wkb).expect("EWKB PolygonZ should decode");
        let SurfaceGeometryZ::Polygon(polygon) = geometry else {
            panic!("expected PolygonZ")
        };
        assert_eq!(polygon.exterior.points[1].z, 7.0);
    }

    #[test]
    fn footprint_and_surface_decoders_never_drop_or_invent_z() {
        let xy = write_polygon(WKB_POLYGON, &[square(0.0)], true);
        decode_footprint_wkb(&xy).expect("XY footprint should decode");
        let error = decode_surface_geometry_z_wkb(&xy).expect_err("surface requires Z");
        assert!(matches!(
            error,
            WkbError::CoordinateDimensionMismatch {
                expected: CoordinateDimension::Xyz,
                actual: CoordinateDimension::Xy,
                ..
            }
        ));

        let xyz = write_polygon(1_003, &[square(3.0)], true);
        let error = decode_footprint_wkb(&xyz).expect_err("footprint must not discard Z");
        assert!(matches!(
            error,
            WkbError::CoordinateDimensionMismatch {
                expected: CoordinateDimension::Xy,
                actual: CoordinateDimension::Xyz,
                ..
            }
        ));
    }

    #[test]
    fn rejects_mixed_multipolygon_dimensions() {
        let polygon_xy = write_polygon(WKB_POLYGON, &[square(0.0)], true);
        let mut wkb = vec![1];
        wkb.extend_from_slice(&1_006_u32.to_le_bytes());
        wkb.extend_from_slice(&1_u32.to_le_bytes());
        wkb.extend_from_slice(&polygon_xy);

        let error = decode_surface_geometry_z_wkb(&wkb).expect_err("mixed dimensions must fail");
        assert!(matches!(
            error,
            WkbError::MixedMultiPolygonDimension {
                expected: CoordinateDimension::Xyz,
                actual: CoordinateDimension::Xy,
                member_index: 0
            }
        ));
    }

    #[test]
    fn decodes_ogc_multi_line_string_with_member_byte_orders() {
        let first = write_line_string(
            WKB_LINE_STRING,
            &[[1.0, 2.0, 0.0, 0.0], [3.0, 4.0, 0.0, 0.0]],
            true,
            None,
        );
        let second = write_line_string(
            WKB_LINE_STRING,
            &[[5.0, 6.0, 0.0, 0.0], [7.0, 8.0, 0.0, 0.0]],
            false,
            None,
        );
        let wkb = write_multi_line_string(WKB_MULTI_LINE_STRING, &[first, second], false, None);

        let geometry = decode_multi_line_string_wkb(&wkb).expect("MultiLineString should decode");

        assert_eq!(geometry.lines.len(), 2);
        assert_eq!(geometry.lines[0].points[1], Point2D { x: 3.0, y: 4.0 });
        assert_eq!(geometry.lines[1].points[0], Point2D { x: 5.0, y: 6.0 });
    }

    #[test]
    fn decodes_ewkb_multi_line_string_with_embedded_srids() {
        let line = write_line_string(
            EWKB_SRID_FLAG | WKB_LINE_STRING,
            &[[5.85, 50.84, 0.0, 0.0], [5.86, 50.85, 0.0, 0.0]],
            true,
            Some(4326),
        );
        let wkb = write_multi_line_string(
            EWKB_SRID_FLAG | WKB_MULTI_LINE_STRING,
            &[line],
            true,
            Some(4326),
        );

        let geometry = decode_multi_line_string_wkb(&wkb).expect("EWKB should decode");

        assert_eq!(geometry.lines.len(), 1);
        assert_eq!(geometry.lines[0].points.len(), 2);
        assert_eq!(geometry.lines[0].points[0].x, 5.85);
    }

    #[test]
    fn decodes_empty_multi_line_string_and_empty_member() {
        let empty = write_multi_line_string(WKB_MULTI_LINE_STRING, &[], true, None);
        assert_eq!(
            decode_multi_line_string_wkb(&empty).expect("empty collection should decode"),
            MultiLineString2D { lines: Vec::new() }
        );

        let empty_line = write_line_string(WKB_LINE_STRING, &[], false, None);
        let with_empty_member =
            write_multi_line_string(WKB_MULTI_LINE_STRING, &[empty_line], true, None);
        assert!(
            decode_multi_line_string_wkb(&with_empty_member)
                .expect("empty member should decode")
                .lines[0]
                .points
                .is_empty()
        );
    }

    #[test]
    fn rejects_non_line_string_multi_line_string_member() {
        let polygon = write_polygon(WKB_POLYGON, &[square(0.0)], true);
        let wkb = write_multi_line_string(WKB_MULTI_LINE_STRING, &[polygon], true, None);

        let error = decode_multi_line_string_wkb(&wkb).expect_err("Polygon member must fail");

        assert!(matches!(
            error,
            WkbError::MultiLineStringMemberIsNotLineString {
                member_index: 0,
                base_type: WKB_POLYGON
            }
        ));
    }

    #[test]
    fn rejects_multi_line_string_dimension_mismatches() {
        let xyz_collection = write_multi_line_string(1_005, &[], true, None);
        let outer_error = decode_multi_line_string_wkb(&xyz_collection)
            .expect_err("XYZ collection must not be flattened");
        assert!(matches!(
            outer_error,
            WkbError::CoordinateDimensionMismatch {
                expected: CoordinateDimension::Xy,
                actual: CoordinateDimension::Xyz,
                type_code: 1_005
            }
        ));

        let xyz_member = write_line_string(
            EWKB_Z_FLAG | WKB_LINE_STRING,
            &[[1.0, 2.0, 3.0, 0.0], [4.0, 5.0, 6.0, 0.0]],
            true,
            None,
        );
        let mixed = write_multi_line_string(WKB_MULTI_LINE_STRING, &[xyz_member], true, None);
        let member_error =
            decode_multi_line_string_wkb(&mixed).expect_err("mixed member dimension must fail");
        assert!(matches!(
            member_error,
            WkbError::MixedMultiLineStringDimension {
                expected: CoordinateDimension::Xy,
                actual: CoordinateDimension::Xyz,
                member_index: 0
            }
        ));
    }

    #[test]
    fn rejects_non_multi_line_string_and_trailing_bytes() {
        let line = write_line_string(
            WKB_LINE_STRING,
            &[[1.0, 2.0, 0.0, 0.0], [3.0, 4.0, 0.0, 0.0]],
            true,
            None,
        );
        let wrong_type =
            decode_multi_line_string_wkb(&line).expect_err("top-level LineString must fail");
        assert!(matches!(
            wrong_type,
            WkbError::GeometryIsNotMultiLineString {
                type_code: WKB_LINE_STRING,
                base_type: WKB_LINE_STRING
            }
        ));

        let mut with_trailing = write_multi_line_string(WKB_MULTI_LINE_STRING, &[], true, None);
        with_trailing.extend_from_slice(&[0xaa, 0xbb]);
        assert_eq!(
            decode_multi_line_string_wkb(&with_trailing).expect_err("trailing bytes must fail"),
            WkbError::TrailingBytes(2)
        );
    }
}
