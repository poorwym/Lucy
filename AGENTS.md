# Repository Guidelines

## Project Structure & Module Organization

Lucy generates 3D Tiles from PostGIS data. Rust code is split into `crates/lucy-core` (domain logic), `crates/lucy-server` (HTTP and database services), and `crates/lucy` (the public executable). Configuration lives in `config/`, SQL fixtures in `fixtures/postgis/`, and design notes in `docs/`. The React/TypeScript Cesium demo and its assets are under `frontend/src/`. `target/` and `frontend/dist/` are generated.

## Build, Test, and Development Commands

- `cargo check --workspace`: type-check all Rust crates quickly.
- `cargo test --workspace`: run all Rust tests.
- `cargo fmt --all -- --check`: verify Rust formatting.
- `cargo clippy --workspace --all-targets --all-features`: run Rust lints.
- `just up` / `just down`: start or stop the local PostGIS container.
- `just load-sample-fixture`: load the deterministic local building fixture.
- `just server`: run the server using `config/development.yaml` at `127.0.0.1:8080`.
- `just dev`: run the source-mounted development image with hot reload.
- `just docker-build` / `just docker-test`: build and smoke-test the production image.
- `cd frontend && bun install && bun run dev`: install dependencies and start Vite.
- `cd frontend && bun run build && bun run lint`: build and lint the demo.

## Coding Style & Naming Conventions

Use `rustfmt` output (four-space indentation) and resolve Clippy warnings. Follow Rust conventions: `snake_case` for modules and functions, `CamelCase` for types, and `SCREAMING_SNAKE_CASE` for constants. TypeScript uses ESLint, two-space indentation, `PascalCase` components, and `camelCase` variables. Keep domain logic in `lucy-core`; database and transport concerns belong in `lucy-server`.

## Testing Guidelines

Place Rust unit tests beside their modules using `#[cfg(test)]`; put public API integration tests in a crate's `tests/` directory. Use behavioral names such as `rejects_invalid_tile_level`. Run `cargo test --workspace` before submitting. The frontend has no test runner, so run its lint and build scripts and manually verify UI changes with Vite and the Lucy server.

## Commit & Pull Request Guidelines

History favors scoped Conventional Commits with an optional issue key, for example `feat(core): emit material metadata (YIMO-126)`. Use an imperative subject and focused commits. Pull requests should explain the change and validation, link the issue, note configuration or schema effects, and include screenshots for visible frontend changes.

## Configuration & Security

Do not commit credentials or local `.env` files. Configure database access through `DATABASE_URL`; defaults in the `justfile` are intended only for local development. Review fixture and configuration changes for sensitive production data before committing.

## Postgres URL

postgresql://lucy:lucy@localhost:5432/lucy
