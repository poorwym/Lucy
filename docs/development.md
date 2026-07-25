# Development

This guide covers repository workflows, validation, deterministic fixtures,
the optional Cesium client, and larger dataset reproduction.

## Repository layout

| Path | Responsibility |
| --- | --- |
| `crates/lucy-core` | Tiling, geometry, mesh generation, and GLB encoding. |
| `crates/lucy-server` | PostGIS access, source validation, HTTP routes, and process lifecycle. |
| `crates/lucy` | Public CLI and executable. |
| `config/` | Example, fixture, dataset, and benchmark catalogs. |
| `fixtures/postgis/` | Deterministic SQL fixtures. |
| `docker/` | Development database and production Lucy images. |
| `scripts/` | Container smoke tests, importers, and benchmark utilities. |
| `frontend/` | Optional React/Cesium API consumer. |

## Local development

Docker and [just](https://github.com/casey/just) are sufficient for the
containerized workflow:

```sh
just dev
```

This command:

1. builds and starts the pinned development PostGIS image;
2. loads the deterministic footprint fixture;
3. starts the source-mounted Lucy development image;
4. watches Rust sources, workspace manifests, and `config/development.yaml`.

The repository is mounted at `/workspace`; Cargo registry, Git, and target
caches use named volumes. Stop the stack with `just dev-down`.

For host development:

```sh
just up
just load-sample-fixture
just server
```

Useful service commands:

```sh
just ps
just logs lucy
just psql
just down
```

`just clean` also deletes Docker volumes, so use it only when the database and
build caches can be recreated.

## Deterministic fixtures

The local fixture catalog is `config/fixture-sources.yaml`.

### Extruded buildings

```sh
just load-sample-fixture
```

`fixtures/postgis/poc_buildings.sql` is a legacy filename retained by tests.
The script idempotently recreates `public.poc_buildings`, inserts six WGS 84
footprints, adds a GiST index, and analyzes the relation. User-facing examples
refer to this as the sample-building fixture.

### Native EPSG:7415 surfaces

```sh
just load-surface-fixture
just verify-rdnap-grids
just verify-surface-fixture
just server config/fixture-sources.yaml
```

`surface_buildings_7415` contains a `PolygonZ` roof with a hole and a
`MultiPolygonZ` shell with roof, floor, and vertical faces. It exercises the
explicit RD/NAP transform, native-surface triangulation, clipping, seams, and
half-open split ownership.

## Test matrix

Run deterministic Rust checks:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Run database-backed server tests:

```sh
just up
just test-postgis
```

`just test-postgis` reloads both fixtures, verifies the pinned coordinate grid
sentinel and fixture shape, and runs ignored `lucy-server` integration tests
with a real `DATABASE_URL`. Database-backed tests must fail clearly when the
database or grids are missing; silently skipping them is not an acceptable
pass.

Build and exercise the production image:

```sh
just docker-build
just docker-test
```

The smoke script verifies:

- runtime UID `10001`;
- absence of the source tree, frontend, fixtures, Cargo, and rustc;
- health, tileset, root subtree, and root GLB responses.

## GLB and route checks

Rust tests validate GLB headers, chunks, accessors, buffers, normals, feature
IDs, structural metadata, materials, and matrix ownership. They also cover
surface holes, vertical faces, multipolygon gaps, adjacent seams, boundary
ownership, clipping area, and normals.

For a running native-surface fixture:

```sh
curl --fail --output /tmp/surface-buildings-0-0-0.glb \
  http://127.0.0.1:8080/sources/surface_buildings_7415/content/0/0/0.glb
```

This confirms that content was served, but does not replace an official glTF
or 3D Tiles validator. Those validators are not yet pinned as repeatable CI
gates.

## Optional Cesium client

The frontend is an independent development consumer, not part of the Lucy
distribution:

```sh
cd frontend
bun install --frozen-lockfile
bun run dev
```

With Lucy serving the native fixture, open:

```text
http://127.0.0.1:5173/?tileset=%2Fsources%2Fsurface_buildings_7415%2Ftileset.json&lon=5.8502&lat=50.8400&height=800&offline=1
```

The query parameters select the tileset and camera. `offline=1` uses the
ellipsoid without external imagery. Vite proxies `/sources` to the local Lucy
server.

Frontend checks are:

```sh
cd frontend
bun run lint
bun run build
```

There is no automated browser smoke test yet. The Rust matrix tests remain the
source of truth for coordinate placement; browser validation should eventually
add end-to-end loading, console-error, tile-failure, and screenshot checks.

## Helsinki Kalasatama LoD2 dataset

The larger reproducible dataset is the City of Helsinki's Kalasatama Digital
Twins CityGML 2.0 archive, published under CC BY 4.0.

| Item | Value |
| --- | --- |
| Catalog | <https://hri.fi/data/en_GB/dataset/helsingin-3d-kaupunkimalli> |
| Archive | <https://3d.hel.ninja/data/citygml/Helsinki3D_CityGML_Kalasatama_20190326.zip> |
| SHA-256 | `ef6a787068b82642e5a0be5e20268e137075bb41fdbf0ec88619ad79926e2299` |
| Source CRS | ETRS-GK25 / N2000, serialized as easting, northing, height |
| Normalized relation | `public.helsinki_kalasatama_lod2`, EPSG:4979 `MULTIPOLYGON Z` |

The source contains 2,919 buildings with usable LoD2 geometry and 79,822
polygons. Twelve zero-width wall polygons have no renderable area and are
reported and omitted. The importer preserves audit counts while normalizing
the remaining faces into 295,576 stored 3D triangles.

For each non-degenerate face, the importer selects the dominant XY, XZ, or YZ
projection, applies structured `ST_MakeValid` only when needed, performs
constrained Delaunay triangulation in that plane, restores the original XYZ
axis order, and then transforms the triangle vertices to EPSG:4979.

The source uses EPSG:3879 horizontally and N2000 heights. The importer consumes
the dataset's GIS coordinate order `(easting, northing, height)`, applies the
pinned `fi_nls_fin2023n2000.tif` geoid grid, and stores EPSG:4979 XYZ. The
sentinel is:

```text
(25497750, 6676280, 2.68 N2000 m)
  -> (24.9594331545, 60.1993151098, 20.2747003 ellipsoidal m)
```

Reproduce the import:

```sh
just up
just download-helsinki-kalasatama
just load-helsinki-kalasatama
just verify-fin2023n2000-grid
just verify-helsinki-kalasatama
just server config/helsinki-kalasatama-lod2.yaml
```

To reuse an existing archive:

```sh
just load-helsinki-kalasatama /path/to/Helsinki3D_CityGML_Kalasatama_20190326.zip
```

The configured outward-rounded EPSG:4979 extent is:

```text
west/east:    24.9487997 / 25.0052481 degrees
south/north:  60.1648910 / 60.2045649 degrees
height:        5.51 / 154.12 metres ellipsoidal
```

The recorded verification covered:

- relation inventory, type, dimension, and SRID;
- full Lucy source validation;
- all 64 level-3 requests: 51 GLBs, 13 empty tiles, and no failures;
- materialization through level 7: 2,122 subtrees, 8,888 GLBs, and
  310,118,796 content bytes;
- no invalid GLBs according to the workspace structural summarizer.

This full materialization is the evidence for enabling
`surface_subtree_envelope_shortcut` in the dataset catalog. Redistributed data
or derived displays must attribute the City of Helsinki / Helsinki Region
Infoshare source under CC BY 4.0.

## Release maintenance

Local multi-platform publication uses a named `docker-container` Buildx
builder:

```sh
just docker-publish 0.1.1
```

The recipe publishes immutable SemVer and commit tags but does not move
`latest`. The GitHub release workflow runs after a pull request is merged into
`main` or when manually dispatched. Before publishing, native runners build
and execute the four contracted GNU/Linux and macOS targets. Each executable
must report the workspace version and serve `/health`, `/`, and
`/tileset.json` from the credential-free sample configuration.

The workflow packages each binary with `LICENSE`, the standalone README, and
`lucy.example.yaml`. It verifies per-archive SHA-256 files, combines them into
the released `SHA256SUMS`, publishes SemVer, SHA, and `latest` image tags,
attaches all four archives to the matching GitHub Release, and prepares the
next patch version. Artifact filenames, the Git tag, and `lucy --version` all
derive from the same workspace version.

The workflow uses the repository `GITHUB_TOKEN`; maintainers do not need a
personal GHCR token. GHCR package visibility is configured separately in the
package settings.

## Documentation ownership

Keep project documentation in the root README and these three guides:

- `README.md`: product summary, shortest working path, and navigation;
- `docs/user-guide.md`: public behavior and operator instructions;
- `docs/architecture.md`: implementation contracts and design decisions;
- `docs/development.md`: contributor workflows, tests, fixtures, and dataset
  reproduction.

Update an existing section before creating another Markdown file. Keep
temporary output, investigation notes, benchmark dumps, and planning material
out of the tracked documentation tree.
