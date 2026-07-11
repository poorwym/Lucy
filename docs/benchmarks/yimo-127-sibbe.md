# YIMO-127 Sibbe Comparison and Benchmark

## Status

No real 3DBAG Sibbe geometry is bundled in this repository yet, and no
pg2b3dm/Lucy comparison has been executed as part of this change. The committed
`surface_buildings_7415` fixture is synthetic and is intended for deterministic
correctness tests, not performance claims.

The benchmark remains data-dependent until a reviewed, attributed Sibbe subset
is committed or supplied locally.

## Required Input Manifest

Use one immutable subset from the 3DBAG Sibbe tile and record:

- 3DBAG release identifier and download URL;
- source file SHA-256;
- source layer, initially `lod12_3d`;
- selected `fid` values and extraction predicate;
- source CRS and geometry type profile;
- feature, polygon, ring, and input vertex counts;
- confirmation that at least one selected polygon has an interior ring;
- CC BY 4.0 attribution.

The extracted geometry should be stored as exact hex EWKB or another
lossless, offline fixture representation. CI must not rely on a mutable live
download URL.

## Comparable Runs

Both tools must consume the same PostGIS relation and attributes. Pin and
record the pg2b3dm release or container digest, Lucy commit, PostGIS/PROJ
versions, grid checksums, operating system, CPU, and memory.

For Lucy, define a cold request as the first content request after starting a
fresh Lucy process while PostGIS is already healthy. This is a process-cold
measurement, not proof of an empty database or operating-system page cache.
Record at least five fresh-process runs and report every value plus median;
avoid a pass/fail latency threshold in CI.

pg2b3dm performs batch generation while Lucy serves dynamic HTTP content. Its
whole export duration is not directly equivalent to one Lucy tile request.
Report both with their scopes instead of presenting a single speed ratio.

## Result Table

Populate this table only after running the pinned comparison:

| Metric | Lucy | pg2b3dm | Notes |
| --- | ---: | ---: | --- |
| Unique source features | TBD | TBD | Must match the input manifest |
| Output content files | TBD | TBD | Tiling policies may differ |
| Triangles | TBD | TBD | Different valid triangulations may differ |
| GLB bytes | TBD | TBD | Record metadata and material differences |
| Metadata properties | TBD | TBD | Record names and types |
| First/cold duration | TBD | TBD | State exact scope |
| Warm request duration | TBD | N/A | Optional Lucy diagnostic |

Also record:

- whether normals are present and unit length;
- material `doubleSided` behavior;
- feature ID and structural metadata extensions;
- root/node transform placement and axis convention;
- glTF Validator error/warning counts;
- Cesium position, height, and scale observations.

Triangle equality is not required when both triangulations cover the same
surface without filling holes or creating degenerate triangles. Any feature
count, metadata, vertical datum, or placement difference requires an explicit
explanation.

## Reference Workflow

The pg2b3dm Sibbe getting-started guide uses a 3DBAG GeoPackage, imports a 3D
layer into PostGIS, checks its EPSG:7415 conversion, and then generates 3D
Tiles. Lucy's comparison must additionally record its explicit
RDNAPTRANS2018 + EPSG:1149 1m approximation contract and current root-only
surface ownership. Keep the exact commands used for the comparison beside the
completed result table:

<https://geodan.github.io/pg2b3dm/getting_started.html>
