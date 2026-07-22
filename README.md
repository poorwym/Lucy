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

`just up` builds a local PostGIS image containing checksum-pinned
RDNAPTRANS2018 and FIN2023N2000 grids. They support the Netherlands fixture and
the separately downloaded Helsinki CityGML source without confusing NAP or
N2000 gravity-related heights with EPSG:4979 ellipsoidal height. Runtime PROJ
networking stays off.

The configured native-surface sample is available at:

```text
http://127.0.0.1:8080/sources/surface_buildings_7415/tileset.json
```

`config/fixture-sources.yaml` contains the two deterministic tables created by
`just load-fixtures` plus the separately managed `nl_lod12_3d` benchmark;
`poc_buildings` is explicitly its default source for legacy routes.
The completed pg2b3dm/Lucy full-materialization comparison is documented in
[`docs/benchmarks/yimo-127-sibbe.md`](docs/benchmarks/yimo-127-sibbe.md).
The independent Helsinki Kalasatama LoD2 dataset, its reproducible importer,
3D coordinate conversion, and full-materialization results are documented in
[`docs/datasets/helsinki-kalasatama.md`](docs/datasets/helsinki-kalasatama.md).
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
