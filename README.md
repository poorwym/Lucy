# Lucy

[![Lucy — PostGIS to 3D Tiles, live](docs/assets/og.png)](https://lucy-demo.yimowu.com)

Lucy is a server-only service that turns PostGIS geometry into streamable
[3D Tiles 1.1](https://www.ogc.org/standard/3dtiles/) resources on demand.
PostGIS remains the source of truth; Lucy queries, transforms, tiles, and
encodes the data when a client requests it.

Lucy ships a CLI and a production container. It does not bundle Cesium, the
demo frontend, a database, or sample data.

## Live demo

Explore Lucy's on-demand 3D Tiles streaming in the
[interactive Cesium demo](https://lucy-demo.yimowu.com).

[![Lucy live demo showing PostGIS buildings streamed as 3D Tiles](docs/assets/demo-screenshot.png)](https://lucy-demo.yimowu.com)

## Quick start

The fastest repository-based development path needs Docker and
[just](https://github.com/casey/just):

```sh
just dev
```

This starts PostGIS, loads the deterministic sample buildings, and runs Lucy
in a source-mounted development container with automatic Rust rebuilds.

Verify the service:

```sh
curl --fail http://127.0.0.1:8080/health
curl --fail http://127.0.0.1:8080/tileset.json
curl --fail --output /tmp/lucy-root.glb \
  http://127.0.0.1:8080/content/0/0/0.glb
```

Stop the development stack with:

```sh
just dev-down
```

## Download the CLI

Versioned standalone archives are published for x86-64 and ARM64 on GNU/Linux
and macOS. Download the matching
[GitHub Release](https://github.com/poorwym/Lucy/releases), verify it against
the attached `SHA256SUMS`, and run:

```sh
tar -xzf lucy-v0.1.1-aarch64-apple-darwin.tar.gz
cd lucy-v0.1.1-aarch64-apple-darwin
./lucy --version
cp lucy.example.yaml lucy.yaml
```

The [user guide](docs/user-guide.md#standalone-archive) lists every supported
target, native runtime requirements, checksum commands, and service startup
steps. Windows and musl Linux users should use the container.

## Run the published image

Lucy `v0.1.1` is public on GHCR for `linux/amd64` and `linux/arm64`:

```sh
docker pull ghcr.io/poorwym/lucy:0.1.1
cp config/lucy.example.yaml lucy.yaml
```

Edit `lucy.yaml` so the table, columns, SRID, geometry model, and bounds match
your PostGIS source. Then inject the database URL at runtime:

```sh
docker run --rm -p 8080:8080 \
  --env DATABASE_URL='postgres://user:password@database-host:5432/database' \
  --mount type=bind,src="$PWD/lucy.yaml",dst=/etc/lucy/config.yaml,readonly \
  ghcr.io/poorwym/lucy:0.1.1
```

The database hostname must be reachable from the container; container-local
`localhost` does not refer to the host machine.

## CLI

Build or run the CLI from the workspace:

```sh
cp config/lucy.example.yaml lucy.yaml
export DATABASE_URL='postgres://user:password@localhost:5432/database'
cargo run -p lucy -- validate
cargo run -p lucy -- serve
```

The public commands are:

```text
lucy serve [--config <PATH>] [--bind <ADDRESS>]
lucy validate [--config <PATH>] [SOURCE_ID]
lucy --help
lucy --version
```

## Development checks

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
just docker-test
```

The optional React/Cesium application under `frontend/` is an independent API
consumer for manual compatibility checks. It is never included in Lucy
release artifacts.

## Documentation

The project documentation has three stable entry points:

| Document | Use it for |
| --- | --- |
| [User guide](docs/user-guide.md) | Installation, configuration, CLI, HTTP routes, containers, and operations. |
| [Architecture](docs/architecture.md) | Geometry models, coordinate handling, tiling, validation, and GLB encoding. |
| [Development](docs/development.md) | Local workflows, tests, fixtures, the Cesium demo, and dataset reproduction. |

Configuration examples live in `config/`, deterministic SQL fixtures in
`fixtures/postgis/`, and the production image definition in
`docker/lucy/Dockerfile`.
