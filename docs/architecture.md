# Architecture

Lucy is a stateless serving layer between PostGIS and any 3D Tiles client.
The database remains authoritative; Lucy does not copy source relations into
an internal catalog or bundle a viewer.

```text
PostGIS -> lucy-server -> lucy-core -> 3D Tiles / subtree / GLB -> client
               |             |
        query + CRS       tiling + mesh
        normalization     + encoding
```

The workspace keeps domain geometry and encoding in `lucy-core`, while
database access, validation, HTTP routing, and process lifecycle live in
`lucy-server`. The `lucy` crate owns the public CLI.

## Source models

Lucy supports two explicit geometry strategies. They share tiling, feature
metadata, materials, and GLB encoding, but intentionally use different query
and mesh contracts.

| Model | Database geometry | Vertical source | Adapter output |
| --- | --- | --- | --- |
| `extruded_footprint` | `Polygon` / `MultiPolygon` | `base_height_column` plus required `height_column` | EPSG:4326 clipped polygon XY and original-boundary XY |
| `surface_geometry_z` | `PolygonZ` / `MultiPolygonZ` | Geometry Z ordinates | EPSG:4979 longitude, latitude, and ellipsoidal height |

### Extruded footprints

Footprints may use any positive SRID available to PostGIS and PROJ. For each
tile, PostGIS:

1. filters through the spatial index in the source CRS;
2. intersects the footprint with the tile envelope;
3. preserves the part of the original feature boundary inside the tile;
4. transforms the results to EPSG:4326.

Lucy extrudes the clipped caps from `base_height_m` to
`base_height_m + height_m`. Only original feature boundaries become side
walls; tile-cut edges do not. Adjacent fragments therefore reconstruct a
continuous shell without artificial internal walls.

Interior rings are supported. Curves, lines, points, measured coordinates,
geometry collections, and arbitrary solids are rejected.

### Native XYZ surfaces

Native surfaces retain every source Z ordinate. They must not configure
extrusion columns and are never flattened or re-extruded.

PostGIS applies an indexed bounding-box broad phase and returns complete XYZ
candidates. Lucy then:

1. validates and triangulates each face in a stable source-wide ENU frame;
2. clips triangles against the requested longitude/latitude tile;
3. interpolates new edge vertices in the source frame;
4. transforms retained positions and normals into tile-local ENU.

The SQL path deliberately avoids `ST_Intersection`, `ST_Area`, and other XY
overlay operations that would collapse vertical faces.

Every polygon ring must be closed, finite, and planar within the current
`0.01m` local-frame tolerance. It must contain at least three distinct
vertices, while holes must remain inside and not intersect the exterior.
Input winding may use either direction.

## Coordinate boundary

The PostGIS adapter is Lucy's coordinate-system boundary. Source SRIDs and WKB
do not cross into mesh generation:

| Source model | Standard core domain |
| --- | --- |
| Footprint | EPSG:4326 longitude/latitude plus local extrusion metres |
| Native surface | EPSG:4979 longitude/latitude/ellipsoidal-height XYZ |

Configured `bounds` always use this standard geographic domain:

- west/east: longitude in degrees;
- south/north: latitude in degrees;
- min/max height: ellipsoidal metres.

For EPSG:7415 surfaces, Lucy supports the explicit
`rdnaptrans2018_epsg_1149` operation. It applies the official RD/NAP grids and
then the EPSG:1149 ETRS89-to-WGS84 approximation. The latter has a declared
1m accuracy and is appropriate for building visualization, not survey-grade
dynamic WGS 84.

The development PostGIS image pins:

| Grid | SHA-256 |
| --- | --- |
| `nl_nsgi_rdtrans2018.tif` | `7653831191b424e715a906468962fc60071cfb71a3186b2a58f098bab8bf41de` |
| `nl_nsgi_nlgeo2018.tif` | `f8e32c56bf8940fc3fefbc0e413eb4546633ed598b669c63856cddf8328992c0` |

`PROJ_NETWORK=OFF` prevents runtime grid drift. `just verify-rdnap-grids`
checks that `(121302, 487371, 2.68)` in EPSG:7415 becomes approximately
`(4.8923670359, 52.3731792027, 45.6625858)` in EPSG:4979. An unchanged height
proves that the vertical grid was skipped.

Sources already stored as EPSG:4979 omit `coordinate_operation`. Other native
3D source CRS combinations are rejected rather than delegated to an ambiguous
automatic transform.

## Validation model

Source entries reject unknown fields and incompatible combinations before any
route becomes ready.

Metadata validation is bounded independently of table size. It checks:

- relation and required-column existence;
- `SELECT` permissions;
- geometry typmod, SRID, and declared type;
- available ID and nullability constraints;
- coordinate-operation availability.

Full validation additionally scans IDs and geometry, checks finite coordinates
and actual type/dimension/SRID profiles, and verifies that transformed native
surface extrema fit the configured bounds.

Request-time decoding and mesh validation always remain active, even when
startup validation is disabled.

`surface_subtree_envelope_shortcut: true` is a separate operator assertion for
large audited native-surface relations. It allows a contained feature envelope
to prove subtree occupancy without decoding geometry. Neither metadata nor
full validation certifies that stronger topology and triangulation promise;
operators must re-audit after data or mesh-contract changes.

## Tiling and availability

Lucy emits a 3D Tiles 1.1 implicit `QUADTREE`. `min_level` is currently fixed
at `0`; `max_level` is at most `31`; and subtree depth is at most `8`.

Footprint occupancy uses the same positive-area predicate as content. Native
surface occupancy uses paged GiST candidates followed by exact core clipping,
unless an audited envelope shortcut settles the result.

Availability follows these rules:

- occupied tiles are tile-available;
- content is unavailable when a tile exceeds `max_features_per_tile`;
- an overflowing non-leaf remains traversable so children can resolve it;
- overflow at `max_level` returns structured `tile_overflow` / HTTP 409;
- ancestors of occupied descendants remain tile-available;
- empty non-root subtree requests return 404;
- final partial subtrees keep fixed bitstream sizes and clear out-of-range
  bits.

Mixed availability uses Morton-ordered little-endian bitstreams with
deterministic padding. URI templates affect emitted tileset URLs but do not
create new HTTP routes.

The root and tileset share `root_geometric_error_m`. Implicit child error is:

```text
error(level) = root_geometric_error_m / 2^level
```

Refinement is `REPLACE`. `content_start_level` can keep early ancestor levels
as traversal-only nodes.

## GLB encoding

One content request produces one glTF 2.0 binary asset. Feature meshes share a
single triangle primitive and draw call. The binary buffer contains:

- `UNSIGNED_INT` triangle indices;
- glTF Y-up `FLOAT VEC3` positions and normals;
- per-vertex `COLOR_0`;
- per-vertex `_FEATURE_ID_0`.

`EXT_mesh_features` maps feature IDs to an embedded
`EXT_structural_metadata` property table. The source ID is preserved as
`featureId`; configured attributes are encoded as string columns. Missing
values use a NUL sentinel so they remain distinct from real empty strings.

The material uses a white PBR base color so vertex colors remain authoritative.
Feature colors accept `#RRGGBB` or `#RRGGBBAA`; any transparency selects
`BLEND`, otherwise the material is `OPAQUE`. Textures and typed numeric
metadata are not part of v0.1.

## Transform ownership

There is exactly one ECEF placement transform: the tileset root maps the
source-wide ENU frame to ECEF. Each requested tile uses a tile-local ENU frame
to preserve `f32` precision.

Let `T_source` and `T_tile` map their ENU frames to ECEF, `C` be
`inverse(T_source) * T_tile`, and `R` convert runtime glTF Y-up to 3D Tiles
Z-up. Lucy writes tile ENU positions as `g = inverse(R) * ENU` and the GLB node
matrix as `M = inverse(R) * C * R`:

```text
T_source * R * M * g = T_tile * ENU
```

The root content matrix is identity. The GLB node never contains a second ECEF
translation.

Crossing native triangles use closed clip planes so adjacent tiles share seam
positions. Faces wholly on an internal split plane use half-open ownership:
west and south are included, internal east and north are excluded, while the
outer source boundary remains included.

## Deliberate limits

- Lucy does not discover database sources automatically.
- Bounds are operator-provided in metadata mode.
- Feature overflow is explicit; results are never silently truncated.
- The service does not include cache persistence, static export, a viewer,
  authentication, or an administrative control plane.
- Exact diagnostics, current metrics output, development CORS behavior,
  textures, and typed metadata may change during v0.1.
