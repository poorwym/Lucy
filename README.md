# Lucy
A middleware layer for dynamically generating 3D tiles from a PostGIS database.

## POC Workspace

Lucy keeps the original extruded-footprint POC and also supports native
`PolygonZ` / `MultiPolygonZ` surface sources. The two geometry strategies and
their coordinate contracts are documented in
[`docs/source-geometry-model.md`](docs/source-geometry-model.md). The original
footprint assumptions remain documented in
[`docs/poc-source-contract.md`](docs/poc-source-contract.md) and the standards
validation report in [`docs/phase-0-report.md`](docs/phase-0-report.md).

Run the POC config loader:

```sh
cargo run -p lucy-poc -- config/poc-sources.yaml
```

Check the workspace:

```sh
cargo check
```

Run the POC HTTP server:

```sh
just up
just load-fixtures
just verify-rdnap-grids
just fixture-server
```

Server startup uses bounded metadata validation by default. Run an explicit
full source scan separately when auditing imported data:

```sh
DATABASE_URL=postgres://lucy:lucy@localhost:5432/lucy \
  cargo run -p lucy-poc -- validate config/fixture-sources.yaml surface_buildings_7415
```

The optional final argument selects one source; omitting it validates every
configured source.

`just up` builds a local PostGIS image containing the checksum-pinned
RDNAPTRANS2018 horizontal and vertical grids required to convert EPSG:7415 NAP
heights to ETRS89 ellipsoidal heights. Lucy then applies the explicit EPSG:1149
ETRS89-to-WGS84 zero-translation approximation (1m accuracy contract) and tags
the result as EPSG:4979. Runtime PROJ networking stays off.

The configured native-surface sample is available at:

```text
http://127.0.0.1:8080/sources/surface_buildings_7415/tileset.json
```

`config/fixture-sources.yaml` contains the two deterministic tables created by
`just load-fixtures` plus the separately managed `nl_lod12_3d` benchmark;
`poc_buildings` is explicitly its default source for legacy routes.
The existing `just poc-server` command continues to use
`config/poc-sources.yaml`, including the separately managed
`controlled_airspace` source.

Run the frontend demo separately:

```sh
cd frontend
bun run dev
```

Open the Vite URL and keep the Rust server running on `127.0.0.1:8080`.
For the offline native-surface smoke URL and camera parameters, see
[`docs/testing.md`](docs/testing.md#cesium-smoke-test).

See [`docs/testing.md`](docs/testing.md) for unit, PostGIS, validator, and
Cesium verification status and commands.
