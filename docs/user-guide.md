# User Guide

This guide describes the supported Lucy v0.1 service surface: how to configure
a PostGIS source, validate it, start Lucy, and consume the generated 3D Tiles.
Implementation details live in [Architecture](architecture.md), while
repository workflows live in [Development](development.md).

## Product boundary

Lucy v0.1 is a backend service and CLI. The production image contains the
`lucy` executable and runtime system libraries only. It does not contain:

- PostGIS or source data;
- Cesium or another viewer;
- the repository frontend;
- Rust build tools, source code, or test fixtures;
- database credentials.

Any 3D Tiles client can consume Lucy's public HTTP routes independently.

## Installation

### Standalone archive

Each GitHub Release publishes `lucy` archives for this v0.1 matrix:

| Archive target | Native runtime |
| --- | --- |
| `x86_64-unknown-linux-gnu` | 64-bit x86 GNU/Linux with glibc 2.35 or newer. |
| `aarch64-unknown-linux-gnu` | 64-bit ARM GNU/Linux with glibc 2.35 or newer. |
| `x86_64-apple-darwin` | Intel Mac with macOS 10.12 or newer. |
| `aarch64-apple-darwin` | Apple Silicon Mac with macOS 11 or newer. |

Choose a full SemVer and the target matching the host. For example:

```sh
VERSION=0.1.1
TARGET=aarch64-apple-darwin
ARCHIVE="lucy-v${VERSION}-${TARGET}.tar.gz"
BASE_URL="https://github.com/poorwym/Lucy/releases/download/v${VERSION}"

curl --fail --location --remote-name "${BASE_URL}/${ARCHIVE}"
curl --fail --location --remote-name "${BASE_URL}/SHA256SUMS"
grep "  ${ARCHIVE}$" SHA256SUMS >"${ARCHIVE}.sha256"
shasum -a 256 --check "${ARCHIVE}.sha256"
tar -xzf "${ARCHIVE}"
cd "lucy-v${VERSION}-${TARGET}"
./lucy --version
```

On GNU/Linux, use `sha256sum --check "${ARCHIVE}.sha256"` instead of
`shasum`. Every archive contains the executable, Apache 2.0 license, concise
installation instructions, and `lucy.example.yaml`. Rust and PostgreSQL client
libraries are not required; Lucy connects directly to a separately operated
PostgreSQL database with PostGIS.

Windows, musl-based Linux distributions, 32-bit systems, and target triples
outside the table are unsupported in v0.1. Use the production container on
those hosts. macOS may require the usual first-run approval for an unsigned
downloaded executable.

### Production container

The public image is:

```text
ghcr.io/poorwym/lucy:0.1.1
```

It supports `linux/amd64` and `linux/arm64`. Use a full SemVer tag in
deployments; `latest` is movable.

```sh
docker pull ghcr.io/poorwym/lucy:0.1.1
```

### Build from source

Lucy uses the Rust toolchain pinned by the release workflow:

```sh
cargo build --locked --release -p lucy
target/release/lucy --version
```

## Configure a source

Start from the credential-free reference catalog:

```sh
cp config/lucy.example.yaml lucy.yaml
```

The catalog contains a map of named sources and an optional default:

```yaml
default_source: buildings
validation:
  startup: metadata
sources:
  buildings:
    connection: ${DATABASE_URL}
    schema: public
    table: buildings
    geometry_column: geom
    id_column: id
    srid: 4326
    source_model: extruded_footprint
    base_height_column: base_height_m
    height_column: height_m
    geometry_types:
      - Polygon
      - MultiPolygon
    bounds:
      west: -122.40130
      south: 37.79245
      east: -122.39975
      north: 37.79375
      min_height_m: 0.0
      max_height_m: 100.0
    min_level: 0
    max_level: 16
    subtree_levels: 4
    max_features_per_tile: 1000
    tileset:
      root_geometric_error_m: 512.0
      content_uri_template: "content/{level}/{x}/{y}.glb"
      subtree_uri_template: "subtrees/{level}/{x}/{y}.subtree"
    attributes: []
    material:
      default_base_color: [0.72, 0.70, 0.65, 1.0]
```

Committed catalogs should use the exact `${DATABASE_URL}` placeholder.
Lucy resolves it from the process environment and exits if it is missing.
Other secret-expansion forms are not supported in v0.1.

### Source fields

| Field | Meaning |
| --- | --- |
| Source map key | Stable source ID used in `/sources/{source_id}/...` routes. |
| `connection` | PostgreSQL connection string or `${DATABASE_URL}`. |
| `schema`, `table` | Relation containing the source features. |
| `geometry_column`, `id_column` | Geometry and stable unique feature ID columns. |
| `srid` | Positive PostGIS SRID declared by the geometry column. |
| `source_model` | `extruded_footprint` or `surface_geometry_z`. |
| `coordinate_operation` | Supported explicit 3D transform; only valid for native surfaces that are not already EPSG:4979. |
| `base_height_column`, `height_column` | Extrusion inputs; `height_column` is required for footprints and both fields are invalid for native surfaces. |
| `geometry_types` | Allowed `Polygon`/`MultiPolygon` or `PolygonZ`/`MultiPolygonZ` types for the selected model. |
| `bounds` | WGS 84 longitude/latitude and ellipsoidal-height root extent. |
| `min_level`, `max_level` | Implicit-quadtree range. `min_level` must currently be `0`. |
| `subtree_levels` | Levels represented by each implicit-tiling subtree. |
| `max_features_per_tile` | Overflow threshold; Lucy returns HTTP 409 rather than truncating features. |
| `tileset.content_start_level` | First level that may advertise content; defaults to `0`. |
| `tileset.root_geometric_error_m` | Root geometric error; child error halves at each level. |
| URI templates | Emitted into the tileset and must contain `{level}`, `{x}`, and `{y}`. |
| `attributes` | Columns encoded as string-valued structural metadata. |
| `material.color_column` | Optional `#RRGGBB` or `#RRGGBBAA` feature color. |
| `material.default_base_color` | Fallback RGBA values in the range `0..=1`. |

See [Architecture](architecture.md#source-models) for the different geometry
semantics of the two source models.

### Relation requirements

An extruded-footprint relation needs:

- a stable, unique, non-null feature ID;
- non-null `Polygon` or `MultiPolygon` geometry in the configured SRID;
- a positive extrusion-height column;
- an optional base-height column, where missing or null values become `0m`;
- any configured metadata or color columns.

A native-surface relation needs the same stable feature ID plus non-null
`PolygonZ` or `MultiPolygonZ` geometry. Its Z ordinates are authoritative, so
it must not configure extrusion-height columns.

Add a GiST index to the geometry column for spatial filtering. The configured
geographic and ellipsoidal-height bounds must contain the complete source;
`lucy validate` can verify them with a full scan.

## Validation

The catalog-level `validation.startup` policy is:

| Value | Startup behavior |
| --- | --- |
| `metadata` | Default. Checks relation metadata, required columns, permissions, declared geometry shape, and transform availability. |
| `full` | Adds a relation scan for IDs, geometry shape, finite coordinates, and configured bounds. |
| `none` | Skips database startup probes; request-time decoding and validation still apply. |

The explicit command always performs full, read-only validation:

```sh
export DATABASE_URL='postgres://user:password@localhost:5432/database'
lucy validate --config lucy.yaml
lucy validate --config lucy.yaml buildings
```

Validation never modifies the source relation.

## Start the service

### CLI

```sh
export DATABASE_URL='postgres://user:password@localhost:5432/database'
lucy serve --config lucy.yaml --bind 127.0.0.1:8080
```

For a standalone executable, configuration is resolved from `--config`, then
`LUCY_CONFIG`, then `lucy.yaml`. The bind address is resolved from `--bind`,
then `LUCY_BIND`, then `127.0.0.1:8080`.

### Container

```sh
docker run --rm -p 8080:8080 \
  --env DATABASE_URL='postgres://user:password@database-host:5432/database' \
  --env RUST_LOG='lucy=info,lucy_server=info' \
  --mount type=bind,src="$PWD/lucy.yaml",dst=/etc/lucy/config.yaml,readonly \
  ghcr.io/poorwym/lucy:0.1.0
```

The container contract is:

| Setting | Value |
| --- | --- |
| Entrypoint / command | `/usr/local/bin/lucy serve` |
| Configuration | `/etc/lucy/config.yaml` |
| Bind address | `0.0.0.0:8080` |
| Runtime user | UID/GID `10001` |
| Health check | `GET /health` through the internal `__healthcheck` command |
| Shutdown | Graceful SIGTERM |

Set `LUCY_LOG_FORMAT=json` for JSON logs. Override the default config, bind
address, port, or health check only as one consistent deployment change.

## HTTP API

| Route | Result |
| --- | --- |
| `GET /health` | Readiness response with `status: "ok"`. |
| `GET /` | Service version, default source, source count, and route summary. |
| `GET /sources/{source_id}/tileset.json` | 3D Tiles 1.1 tileset. |
| `GET /sources/{source_id}/subtrees/{level}/{x}/{y}.subtree` | Binary implicit-tiling subtree. |
| `GET /sources/{source_id}/content/{level}/{x}/{y}.glb` | Binary glTF content tile. |
| `GET /tileset.json`, `/subtrees/...`, `/content/...` | Aliases for the default source. |

Verify a running sample:

```sh
curl --fail http://127.0.0.1:8080/health
curl --fail http://127.0.0.1:8080/tileset.json
curl --fail --output /tmp/lucy-root.subtree \
  http://127.0.0.1:8080/subtrees/0/0/0.subtree
curl --fail --output /tmp/lucy-root.glb \
  http://127.0.0.1:8080/content/0/0/0.glb
```

Structured 4xx and 5xx bodies expose `error.code` and `error.message`. Exact
diagnostic text, permissive development CORS behavior, and the current
`/metrics` JSON response are experimental.

## Operations and upgrades

- Keep credentials in runtime secrets; never bake them into the image or
  commit them in catalogs.
- Mount the catalog read-only and grant the database user only the required
  `SELECT` access.
- Use `/health` for readiness and container orchestration.
- Persist PostGIS independently; Lucy is a stateless serving process.
- Pin production deployments to a full SemVer or image digest.
- Validate catalogs and source data before upgrading.
- Read the GitHub Release notes for configuration or behavior changes.

The v0.1 CLI, HTTP routes, configuration fields, port, volume, and secret
contracts remain compatible across v0.1 patch releases. New optional fields
may be added with defaults; incompatible schema changes require an explicit
migration.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Help/version completed, validation passed, or shutdown was clean. |
| `1` | Configuration, database, validation, bind, health, or runtime failure. |
| `2` | Invalid CLI syntax or option value. |
