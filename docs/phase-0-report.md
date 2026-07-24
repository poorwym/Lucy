# Lucy Phase 0 Standards Validation POC

This report describes the original footprint POC. The native EPSG:7415
PolygonZ/MultiPolygonZ extension is documented separately in
[`source-geometry-model.md`](source-geometry-model.md), with current validation
commands and remaining external checks in [`testing.md`](testing.md).

## Result

The Phase 0 path is implemented as a minimal Rust POC:

- Loads the fixed `poc_buildings` source config.
- Generates a 3D Tiles 1.1 implicit `tileset.json`.
- Generates a binary root `.subtree`.
- Queries one tile bbox from PostGIS as WKB.
- Converts supported WKB `Polygon` and `MultiPolygon` footprints into internal triangle meshes.
- Extrudes feature meshes from `base_height_m` to `base_height_m + height_m`.
- Encodes one GLB content tile from one or more extruded feature meshes.
- Serves POC tile routes consumed by the frontend Cesium demo.

## POC Routes

Start PostGIS, load the fixture, and run the POC server:

```sh
just up
just load-sample-fixture
DATABASE_URL=postgres://lucy:lucy@localhost:5432/lucy cargo run -p lucy -- serve --config config/poc-sources.yaml --bind 127.0.0.1:8080
```

Routes:

| Route | Purpose |
| --- | --- |
| `/tileset.json` | 3D Tiles 1.1 implicit tileset. |
| `/subtrees/0/0/0.subtree` | Root subtree binary. |
| `/content/{level}/{x}/{y}.glb` | PostGIS-backed GLB content tile. |
| `/phase-0-report.md` | This report. |

The root content tile is expected to contain fixture geometry:

```sh
curl -i http://127.0.0.1:8080/content/0/0/0.glb
```

## Minimum Standards Fields

`tileset.json`:

- `asset.version = "1.1"`
- root `boundingVolume.region`
- root `transform` placing the local ENU mesh frame at the source bounds center
- root `geometricError`
- root `content.uri`
- root `implicitTiling.subdivisionScheme = "QUADTREE"`
- root `implicitTiling.availableLevels`
- root `implicitTiling.subtreeLevels`
- root `implicitTiling.subtrees.uri`

`.subtree`:

- binary subtree header magic `subt`
- version `1`
- padded JSON chunk
- `tileAvailability`
- `contentAvailability`
- `childSubtreeAvailability`

GLB content:

- GLB magic/version/length header
- JSON chunk with glTF 2.0 asset, scene, relative tile-frame node matrix, mesh primitive, buffer, bufferViews, and accessors
- BIN chunk containing little-endian `UNSIGNED_INT` indices followed by glTF Y-up `FLOAT` `VEC3` positions and normals
- position accessor `min` and `max`

The tileset root remains the only ENU-to-ECEF placement. Content uses a
per-request tile-local ENU frame for `f32` precision; its GLB node transform is
only relative to the source ENU root frame and is identity for root content.

## Cesium Frontend Demo

Run the frontend demo while the Rust tile server is running:

```sh
cd frontend
bun run dev
```

Expected behavior:

- CesiumJS requests `http://127.0.0.1:8080/tileset.json`.
- CesiumJS requests `http://127.0.0.1:8080/subtrees/0/0/0.subtree`.
- CesiumJS can resolve `http://127.0.0.1:8080/content/0/0/0.glb` from the content URI template.
- The root tile contains non-empty extruded GLB geometry from the PostGIS fixture.
- The root tile transform places local meter geometry at the configured San Francisco block.
- The smoke scene includes a basic OpenStreetMap imagery layer for visual context.

The React/Vite frontend owns the Cesium viewer. The Rust server intentionally
does not embed inline HTML.

## Validation

Local verification:

```sh
cargo test -p lucy
```

The tests cover:

- source config validation
- quadtree region math
- tileset JSON golden output
- subtree binary header/padding and availability helpers
- PostGIS WKB bbox querying
- WKB-to-mesh conversion and unsupported geometry errors
- GLB header/chunks/JSON/binary payload
- real PostGIS WKB to mesh to GLB content tile
- POC HTTP route handling for tileset, subtree, root status, and report

External glTF validator status:

- The GLB encoder tests parse and validate the generated GLB structure locally.
- The official Khronos glTF Validator is required for the native-surface
  acceptance target but is not yet wired into this repository.
- Automated Cesium browser coverage is likewise still pending; neither check
  should be reported as completed based only on local Rust tests.

## Historical Phase 1 Gap List

The following list records the state when the Phase 0 report was written. It is
not the current native-surface implementation status; use
[`source-geometry-model.md`](source-geometry-model.md) and
[`testing.md`](testing.md) for the current contract and remaining validation.

- Feature metadata, picking IDs, batch tables, and 3D Tiles structural metadata are not emitted.
- The root subtree currently marks availability broadly for the POC instead of deriving sparse availability from PostGIS.
- Empty child content routes return 404; sparse availability should prevent Cesium from requesting those tiles.
- No HTTP connection pooling, caching, compression, or production error handling exists.
- The POC serves one fixed source and does not perform source discovery or schema introspection.
- Materials are not encoded into GLB yet.
- The Cesium frontend demo is manual; an automated browser smoke test should be added when the service shape stabilizes.
