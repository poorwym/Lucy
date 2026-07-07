# Lucy
A middleware layer for dynamically generating 3D tiles from a PostGIS database.

## Phase 0 POC Workspace

This repository currently contains a minimal Phase 0 Rust workspace. It is
scoped to the fixed POC source contract in
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
just load-poc-fixture
DATABASE_URL=postgres://lucy:lucy@localhost:5432/lucy cargo run -p lucy-poc -- serve config/poc-sources.yaml 127.0.0.1:8080
```

Run the frontend demo separately:

```sh
cd frontend
bun run dev
```

Open the Vite URL and keep the Rust server running on `127.0.0.1:8080`.
