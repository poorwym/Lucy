# Lucy Phase 0 Standards Validation POC

## Result

The Phase 0 path is implemented as a minimal Rust POC:

- Loads the fixed `poc_buildings` source config.
- Generates a 3D Tiles 1.1 implicit `tileset.json`.
- Generates a binary root `.subtree`.
- Queries one tile bbox from PostGIS as WKB.
- Converts supported WKB `Polygon` and `MultiPolygon` footprints into internal triangle meshes.
- Encodes one GLB content tile from one or more feature meshes.
- Serves POC routes for CesiumJS smoke testing.

## POC Routes

Start PostGIS, load the fixture, and run the POC server:

```sh
just up
just load-poc-fixture
DATABASE_URL=postgres://lucy:lucy@localhost:5432/lucy cargo run -p lucy-poc -- serve config/poc-sources.yaml 127.0.0.1:8080
```

Routes:

| Route | Purpose |
| --- | --- |
| `/tileset.json` | 3D Tiles 1.1 implicit tileset. |
| `/subtrees/0/0/0.subtree` | Root subtree binary. |
| `/content/{level}/{x}/{y}.glb` | PostGIS-backed GLB content tile. |
| `/cesium-smoke.html` | CesiumJS smoke page loading `/tileset.json`. |
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
- JSON chunk with glTF 2.0 asset, scene, node, mesh primitive, buffer, bufferViews, and accessors
- BIN chunk containing little-endian `UNSIGNED_INT` indices followed by `FLOAT` `VEC3` positions
- position accessor `min` and `max`

## Cesium Smoke Test

Open:

```text
http://127.0.0.1:8080/cesium-smoke.html
```

Expected behavior:

- CesiumJS requests `/tileset.json`.
- CesiumJS requests `/subtrees/0/0/0.subtree`.
- CesiumJS can resolve `/content/0/0/0.glb` from the content URI template.
- The root tile contains non-empty GLB geometry from the PostGIS fixture.
- The root tile transform places local meter geometry at the configured San Francisco block.

The smoke page uses CesiumJS from the public Cesium CDN, so browser execution
requires network access to that CDN. The Rust test suite validates the generated
tileset, subtree, WKB-to-mesh, and GLB structure locally without that CDN.

## Validation

Local verification:

```sh
cargo test -p lucy-poc
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
- POC HTTP route handling for tileset, subtree, smoke page, and report

External glTF validator status:

- Not required to run in CI for Phase 0.
- The GLB encoder tests parse and validate the generated GLB structure locally.
- A Phase 1 task should add an official glTF validator step if GLB generation becomes a release artifact.

## Known Phase 1 Gaps

- The content mesh is a footprint surface only; wall and roof extrusion are not implemented yet.
- Feature metadata, picking IDs, batch tables, and 3D Tiles structural metadata are not emitted.
- The root subtree currently marks availability broadly for the POC instead of deriving sparse availability from PostGIS.
- Empty child content routes return 404; sparse availability should prevent Cesium from requesting those tiles.
- No HTTP connection pooling, caching, compression, or production error handling exists.
- The POC serves one fixed source and does not perform source discovery or schema introspection.
- Materials are not encoded into GLB yet.
- The Cesium smoke page is manual and CDN-backed; an automated browser smoke test should be added when the service shape stabilizes.
