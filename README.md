# Lucy
A middleware layer for dynamically generating 3D tiles from a PostGIS database.

## Phase 0 POC Workspace

This repository currently contains a minimal Phase 0 Rust workspace only. It is
scoped to the fixed POC source contract in
[`docs/poc-source-contract.md`](docs/poc-source-contract.md) and does not start
an HTTP service, manage a PostgreSQL pool, or generate tiles yet.

Run the POC config loader:

```sh
cargo run -p lucy-poc -- config/poc-sources.yaml
```

Check the workspace:

```sh
cargo check
```
