# Lucy

Lucy is server-only middleware that dynamically generates 3D Tiles from
PostGIS. Its v0.1 public CLI, HTTP, configuration, container, and platform
guarantees are defined in the
[v0.1 distribution contract](docs/distribution-v0.1.md).

## Run the CLI

Copy the example catalog, inject the database URL at runtime, then validate and
serve it:

```sh
cp config/lucy.example.yaml lucy.yaml
export DATABASE_URL='postgres://user:password@localhost:5432/database'
cargo run -p lucy -- validate
cargo run -p lucy -- serve
```

The standalone defaults are `lucy.yaml` and `127.0.0.1:8080`. Use
`lucy serve --help` for explicit configuration and bind options.

## Develop with Docker hot reload

The local PostGIS image under `docker/postgis/` is development infrastructure,
not a Lucy distribution. Start it together with the source-mounted development
image using:

```sh
just dev
```

The repository is mounted at `/workspace`; edits under `crates/` or to
`config/development.yaml` automatically rebuild and restart `lucy`. Stop the
stack with `just dev-down`.

To run directly on the host instead:

```sh
just up
just load-sample-fixture
just server
```

## Build and verify the production image

The final image contains only the release `lucy` binary and runs as a non-root
user. It does not contain the Rust toolchain, source tree, fixtures, Cesium, or
frontend assets.

```sh
just docker-build
just docker-test
```

Run it with a read-only catalog and a runtime secret:

```sh
docker run --rm -p 8080:8080 \
  --env DATABASE_URL='postgres://user:password@host.docker.internal:5432/database' \
  --mount type=bind,src="$PWD/lucy.yaml",dst=/etc/lucy/config.yaml,readonly \
  lucy:local
```

Maintainers can publish immutable version and commit tags to GHCR with:

```sh
just docker-publish 0.1.0
```

The recipe intentionally does not move `latest`; release automation owns that
stable-channel decision.

## More documentation

The geometry strategies and coordinate contracts are documented in
[`docs/source-geometry-model.md`](docs/source-geometry-model.md). Historical
Phase 0 behavior is retained in [`docs/phase-0-report.md`](docs/phase-0-report.md),
while current verification commands live in [`docs/testing.md`](docs/testing.md).
The separately hosted Cesium demo under `frontend/` is a consumer of Lucy's
HTTP API and is never bundled with Lucy distributions.
