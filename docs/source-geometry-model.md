# Source Geometry Model

Lucy has two explicit source geometry strategies. They share tiling, feature
metadata, material, and GLB encoding, but their query and mesh construction
contracts are intentionally different.

## Strategies

### `extruded_footprint`

`extruded_footprint` accepts two-dimensional `Polygon` and `MultiPolygon`
footprints in any positive SRID registered in the deployed PostGIS
`spatial_ref_sys` table and supported by PROJ. The adapter queries and clips in
the source SRID, preserves the part of the original feature boundary inside
each tile, transforms both results to EPSG:4326, and builds a volume from
`base_height_column` plus the required `height_column`. Tile-cut edges divide
the caps but do not become artificial extrusion walls.

### `surface_geometry_z`

`surface_geometry_z` accepts native `PolygonZ` and `MultiPolygonZ` surfaces.
Every source Z ordinate is part of the geometry and must survive query,
decoding, triangulation, and GLB encoding. The strategy must not configure
`base_height_column` or `height_column` and never re-extrudes the input.

The deterministic sample is configured as follows:

```yaml
surface_buildings_7415:
  connection: ${DATABASE_URL}
  schema: public
  table: surface_buildings_7415
  geometry_column: geom
  id_column: id
  srid: 7415
  source_model: surface_geometry_z
  coordinate_operation: rdnaptrans2018_epsg_1149
  geometry_types:
    - PolygonZ
    - MultiPolygonZ
  min_level: 0
  max_level: 2
```

The fixture in `fixtures/postgis/surface_buildings_7415.sql` contains one
horizontal `PolygonZ` roof with an interior ring and one closed
`MultiPolygonZ` shell containing a roof, floor, and four vertical faces. It is
synthetic and deliberately small; no 3DBAG source geometry is copied into it.

## Configuration Migration

The adapter owns its standard target CRSs, so they are no longer repeated in
source configuration. For footprints, remove
`vertical_reference: local_ground_meters`; local extrusion metres are inherent
in the model. For native EPSG:7415 surfaces, replace:

```yaml
vertical_reference:
  crs: EPSG:7415
  target: EPSG:4979
  operation: rdnaptrans2018_epsg_1149
```

with:

```yaml
coordinate_operation: rdnaptrans2018_epsg_1149
```

The single `srid` field remains the declared database geometry SRID. Unknown
legacy fields are rejected during configuration loading instead of being
silently ignored.

## Coordinate and Bounds Contract

EPSG:7415 combines Amersfoort / RD New horizontal coordinates with NAP height.
NAP is a physical vertical datum and is not the ellipsoidal height expected by
the WGS 84 three-dimensional target CRS.

The PostGIS adapter is Lucy's coordinate-system boundary. It validates the
declared source SRID, executes all database-side spatial work, decodes the
returned WKB, and exposes one of two typed standard geometries:

| Source model | Adapter output |
| --- | --- |
| `extruded_footprint` | EPSG:4326 clipped polygon XY plus original-boundary `MultiLineString` XY |
| `surface_geometry_z` | EPSG:4979 longitude/latitude/ellipsoidal-height XYZ |

Neither WKB nor source SRID crosses into content generation. `lucy-core` can
therefore construct meshes without branching on a CRS. A missing vertical grid
is an error; Lucy never passes an unchanged NAP value through as if it were
ellipsoidal.

The datum policy is explicit. The [PROJ grid
catalog](https://cdn.proj.org/nl_nsgi_README.txt) identifies RDNAPTRANS2018 as
an RD/NAP-to-ETRS89 transform, not a dynamic WGS 84 realization. The
`rdnaptrans2018_epsg_1149` operation applies the EPSG:1149 zero-translation
ETRS89-to-WGS84 [approximation](https://epsg.org/transformation_1149/ETRS89-to-WGS-84-1.html)
after the horizontal and vertical grids. Its declared datum-approximation
accuracy is 1m, which is suitable for this 3D building visualization contract
but is not survey-grade WGS 84. A strict
dynamic conversion would require a named WGS 84 realization and coordinate
epoch as described by [PROJ's time-dependent transformation
model](https://proj.org/en/stable/operations/time_dependent_transformations.html);
neither can be inferred from XYZ WKB, so Lucy does not expose such a mode.
Sources already stored as EPSG:4979 omit `coordinate_operation`; identity is
implicit only for that source SRID. Other 3D CRS/operation combinations are
rejected instead of delegating an ambiguous vertical transform to
`ST_Transform`. The target SRID is intentionally not configurable, so config
cannot disagree with the geometry semantics expected by the core.

`bounds` is always expressed in Lucy's standard geographic output domain:

- `west` / `east`: longitude in degrees;
- `south` / `north`: latitude in degrees;
- `min_height_m` / `max_height_m`: ellipsoidal height in metres after the
  configured vertical transformation.

The height bounds are not source NAP extrema. For example, the synthetic
fixture uses NAP heights from 130m through 150m while its configured target
height interval is 170m through 201m.

## RDNAPTRANS2018 Deployment

The local PostGIS image installs these PROJ resources from the official datum
grid CDN during the image build:

| Resource | SHA-256 |
| --- | --- |
| `nl_nsgi_rdtrans2018.tif` | `7653831191b424e715a906468962fc60071cfb71a3186b2a58f098bab8bf41de` |
| `nl_nsgi_nlgeo2018.tif` | `f8e32c56bf8940fc3fefbc0e413eb4546633ed598b669c63856cddf8328992c0` |

The binaries are image dependencies and are not committed to this repository.
`PROJ_NETWORK=OFF` makes deployment deterministic. Production images must
provide the same named resources in PROJ's data directory and must pass the
known coordinate used by `just verify-rdnap-grids`. That recipe calls
`ST_TransformPipeline` with Lucy's complete, bound pipeline rather than asking
PROJ to select an implicit operation.

```sql
-- The full pipeline is defined once in lucy-server and the justfile.
SELECT ST_AsEWKT(ST_TransformPipeline(
    ST_GeomFromEWKT('SRID=7415;POINT Z (121302 487371 2.68)'),
    '<lucy rdnaptrans2018_epsg_1149 pipeline>',
    4979
));
```

The expected result is approximately longitude `4.8923670359`, latitude
`52.3731792027`, and ellipsoidal height `45.6625858m`. An unchanged height of
`2.68m` proves that the vertical grid was not applied.

The sentinel's tight numeric tolerance detects grid or pipeline drift; it does
not reduce the separate 1m EPSG:1149 datum-approximation budget.

Lucy uses an explicit RDNAPTRANS2018 pipeline for source features and the
inverse tile envelope. The grid names are therefore part of the runtime
contract, not an optional accuracy improvement.

## Source Validation

Catalog-level `validation.startup` selects `metadata` (the default), `full`, or
`none`. Metadata startup validation is bounded independently of relation size:
it checks the relation, required columns, SELECT permissions, declared PostGIS
geometry typmod, available ID/NULL constraints, and transformation capability.
Footprints use a source-SRID/4326 round-trip probe; surfaces use their
configured 3D operation. Generic typmods or missing constraints are reported
as warnings because metadata alone cannot prove the contents of every row.

Full validation additionally scans the relation for non-null, non-empty,
unique feature IDs, non-null/non-empty geometry, actual SRID/type/Z profiles,
and finite coordinates. For non-empty native-surface sources it transforms all
vertices, computes their exact EPSG:4979 extent, and rejects content outside
the advertised root region or height interval. Configure `startup: full` only
when that scan should gate readiness, or run it explicitly without changing
the configured startup policy:

```sh
cargo run -p lucy-poc -- validate config/sources.yaml [source_id]
```

`startup: none` skips PostGIS startup probes but does not disable request-time
geometry decoding and mesh validation. Bounds remain an operator-provided
source contract in metadata mode; exact extent verification belongs to full
validation. Validation never modifies source relations.

The surface strategy does not use `ST_IsValid`. GEOS applies polygon validity
rules in XY; a legitimate vertical wall has a line-shaped XY projection and is
reported as invalid even though it is a valid three-dimensional face. Surface
topology is instead validated while decoding and building the 3D mesh.

Every polygon ring must:

- repeat the first XYZ vertex as the last vertex;
- contain at least three distinct, finite vertices;
- keep interior rings strictly inside and nonintersecting with the exterior;
- be planar within the configured tolerance (the current default is 0.01m in
  the local source frame).

Input ring winding may be either direction. Triangulation projects each face to
its dominant local plane, which keeps vertical faces valid and supports
interior rings.

## Tile Ownership and Query Semantics

Footprints retain positive-area tile clipping. Their top and bottom caps use
the clipped polygon, while side walls use only the intersection of the
original feature boundary and the tile. A deep tile wholly inside a large
feature therefore has caps but no side walls; neighboring fragments together
reconstruct the feature without visible internal partitions.

Native surfaces use a different two-stage policy. PostGIS transforms the
requested EPSG:4979 tile envelope back into the source CRS, applies the indexed
bounding-box operator, and streams uncapped matching rows as complete XYZ
candidates. This is deliberately only a broad phase: native surfaces are never
passed through `ST_Intersection`, `ST_CollectionExtract`, `ST_Area`, or another
XY overlay operation. Such operations would collapse or discard legitimate
vertical `PolygonZ` faces.

Lucy validates and triangulates each complete face in the stable source-wide
ENU frame, then clips the resulting triangles against the requested tile's
longitude/latitude rectangle. New edge vertices compute an edge parameter from
the geographic clip plane and apply that same parameter to the source-frame
triangle positions before transforming into tile-local ENU. Clipping neither
re-extrudes the input nor creates caps or walls along tile cuts. A surface that
crosses a quadtree boundary therefore contributes bounded fragments with the
same feature id, attributes, material, winding, and face normal to each
intersected tile. Candidates that leave no positive three-dimensional triangle
area after clipping are omitted from that tile.

Crossing triangles treat all four clip planes as closed, giving neighboring
tiles identical seam positions. A positive-area face lying wholly on a split
plane instead follows half-open ownership: west and south boundaries are
included, while an internal east or north boundary is excluded. The outermost
east and north tiles include the source boundary. This assigns a vertical wall
on an internal split plane to exactly one tile without dropping walls on the
edge of the configured source. Feature limits still fail with `tile_overflow`
instead of returning truncated content.

## Transform and Axis Ownership

There is exactly one ECEF placement transform: the 3D Tiles root transform
places the source-wide ENU frame into ECEF. Each content request constructs a
second, tile-local ENU frame at that tile's horizontal centre and minimum
height. Native faces are triangulated in the source frame. Geodetic clip planes
supply the edge parameter used to interpolate each source-ENU triangle edge,
and the resulting positions and normals are transformed into the tile frame
before the final `f64` to `f32` cast. Coordinates therefore remain local even
for deep tiles.

The GLB node contains only the relative tile-frame-to-source-frame transform;
it never contains ECEF translation. Let `T_source` and `T_tile` map their ENU
frames to ECEF, `C = inverse(T_source) * T_tile`, and `R` be the runtime glTF
Y-up to 3D Tiles Z-up conversion. Lucy writes tile ENU buffers as
`g = inverse(R) * ENU` and emits `M = inverse(R) * C * R` on the GLB node. The
complete client chain is therefore:

```text
T_source * R * M * g = T_tile * ENU
```

At the root, the source and tile frames coincide and `M` is identity. Numeric
tests cover that identity, the full EPSG:4978 matrix chain, normal orientation,
and tile-local footprint precision near longitude 0, 90, and 180 degrees.

## Data Attribution

The synthetic fixture is original test data modeled after common 3DBAG surface
shapes. 3DBAG documentation and data are provided by the 3D geoinformation
research group at TU Delft and 3DGI under CC BY 4.0. The NSGI RDNAPTRANS2018
grids distributed by PROJ are also CC BY 4.0.

- 3DBAG: <https://docs.3dbag.nl/>
- PROJ NSGI grid notice: <https://cdn.proj.org/nl_nsgi_README.txt>
- NSGI RDNAPTRANS2018: <https://www.nsgi.nl/rdnaptrans>

A future committed real Sibbe subset must add its exact 3DBAG release, tile,
selected feature IDs, source checksum, extraction command, and attribution.
