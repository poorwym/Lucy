# Testing and Validation

## Fast Workspace Checks

Run the deterministic Rust checks without a database:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Frontend checks remain separate:

```sh
cd frontend
bun install --frozen-lockfile
bun run lint
bun run build
```

## PostGIS Integration

The EPSG:7415 integration path requires the image-baked RDNAPTRANS2018 grids.
Build the image, start the database, load both geometry strategies, verify the
grid sentinel and fixture shape, and run the server suite with a real database:

```sh
just up
just test-postgis
just server config/fixture-sources.yaml
```

`just test-postgis` performs these prerequisites:

1. reloads the legacy `poc_buildings` fixture;
2. reloads `surface_buildings_7415`;
3. checks the known EPSG:7415 grid result plus the explicit EPSG:1149
   ETRS89-to-WGS84 approximation pipeline;
4. checks that the surface fixture contains one PolygonZ, one MultiPolygonZ,
   and one polygon with an interior ring;
5. runs `lucy-server` tests with `DATABASE_URL` set.

Database-backed tests must fail clearly when their database URL or grids are
unavailable. Returning early and reporting a passing test is not acceptable.
The local recipes currently use the development container and database; CI
should use an isolated ephemeral `lucy_test` database.

The fixture server uses `config/fixture-sources.yaml`, which deliberately
excludes externally managed sources such as `controlled_airspace`. This keeps
a clean checkout runnable after loading only the committed SQL fixtures.
The native-surface fixture uses `max_level: 2`. PostGIS returns complete
PolygonZ/MultiPolygonZ candidates from a source-CRS bounding-box broad phase;
it never applies the footprint XY overlay. Core tests triangulate those
complete faces before clipping them to tile rectangles, verify that vertical
walls survive, and exercise half-open east/north ownership for faces on
internal split planes.

The coordinate sentinel uses tight tolerances to detect changes in the pinned
grids and pipeline. End-to-end WGS 84 datum accuracy remains the separate 1m
EPSG:1149 approximation contract; strict dynamic WGS 84 would require a named
realization and coordinate epoch that the current XYZ source does not provide.

## GLB Validation

Rust tests validate GLB headers, accessors, buffers, normals, feature IDs,
structural metadata, and the relative GLB node matrix. Coordinate tests verify
the complete source-root / runtime-axis / relative-node / tile-ENU chain
against direct tile-ENU-to-ECEF placement, require identity at the root, and
exercise local footprint precision and normals near longitude 0, 90, and 180
degrees. Native-surface tests additionally cover shared seam agreement between
adjacent tiles, area and normal preservation, vertical faces, half-open split
ownership, holes, disjoint multipolygon gaps, and contact without positive
area. The YIMO-127 acceptance target additionally calls for the
official Khronos glTF Validator against a GLB fetched from the surface content
route.

That external validator command is not yet wired into this repository. A
one-off full-output run with the official 3D Tiles Validator is recorded in the
[`nl_lod12_3d` comparison report](benchmarks/yimo-127-sibbe.md), but it is not a
repeatable CI gate. When a pinned dependency and script are added, the gate
should require zero core errors and warnings and explicitly account for the
validator's lack of full validation for `EXT_mesh_features` and
`EXT_structural_metadata`; those extensions still require Lucy's structural
tests and Cesium coverage.

## Cesium Smoke Test

The React demo accepts query parameters so the deterministic surface source can
be checked without editing the application or calling Cesium Ion:

```text
http://127.0.0.1:5173/?tileset=%2Fsources%2Fsurface_buildings_7415%2Ftileset.json&lon=5.8502&lat=50.8400&height=800&offline=1
```

`tileset`, `lon`, `lat`, and `height` select the source and camera; `offline=1`
uses the ellipsoid and no external imagery. The status overlay reports loaded
and failed tile counts. Vite proxies `/sources` to the local Lucy server.

An automated headless Cesium smoke test is not yet present. A future browser
gate should:

- select `/sources/surface_buildings_7415/tileset.json` through test config;
- disable Cesium Ion imagery and terrain;
- wait for initial tiles to load;
- require at least one loaded content tile and zero tile failures;
- fail on page and console errors;
- assert that the tileset bounding sphere is near the configured fixture;
- retain a screenshot as a diagnostic artifact.

Position, scale, axis, and double-transform correctness are covered by the
numeric Rust matrix-chain assertions against the ECEF transform. Browser pixels
alone are not a stable coordinate-system test; the pending smoke test still
provides end-to-end Cesium loading and presentation coverage.

## Manual Surface Route Check

With PostGIS and the server running:

```sh
curl --fail --output /tmp/surface-buildings-0-0-0.glb \
  http://127.0.0.1:8080/sources/surface_buildings_7415/content/0/0/0.glb
```

This only confirms that content was served. It does not replace the official
validator or Cesium smoke test. To inspect subdivision, first read the root
subtree availability and request occupied level-1 or level-2 content routes;
an occupied child must return tile-bounded geometry rather than a copy of the
whole source feature.
