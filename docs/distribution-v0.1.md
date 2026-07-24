# Lucy v0.1 Distribution Contract

This document freezes the smallest public distribution surface Lucy supports
through the v0.1 release line. Anything not listed as stable is internal or
experimental and may change without compatibility shims.

## Product boundary

Lucy v0.1 is a server-only PostGIS-to-3D-Tiles backend. The `lucy` executable,
production container, Compose consumers, standalone archives, and release
artifacts contain no Cesium code, frontend assets, viewer routes, sample data,
or database credentials. A viewer is an independent HTTP client.

## Stable command-line interface

The executable is named `lucy` and exposes these commands:

```text
lucy serve [--config <PATH>] [--bind <ADDRESS>]
lucy validate [--config <PATH>] [SOURCE_ID]
lucy --help
lucy --version
```

For the standalone executable, `--config` defaults to `$LUCY_CONFIG` and then
`lucy.yaml`; `--bind` defaults to `$LUCY_BIND` and then `127.0.0.1:8080`.
Explicit flags take precedence over environment variables. `validate` always
performs a full, read-only scan; its optional source ID limits the scan to one
configured source.

Exit codes are stable:

| Code | Meaning |
| --- | --- |
| `0` | Help/version completed, validation passed, or the server shut down cleanly. |
| `1` | Configuration, database, validation, bind, health, or runtime failure. |
| `2` | Invalid CLI syntax or option value. |

Positional configuration and bind arguments from the prototype executable are
not part of this contract.

## Stable HTTP surface

After startup validation succeeds, Lucy serves:

| Route | Contract |
| --- | --- |
| `GET /health` | HTTP 200 readiness response with `status: "ok"`. |
| `GET /` | Service status, version, default source, source count, and advertised routes. |
| `GET /sources/{source_id}/tileset.json` | 3D Tiles tileset JSON. |
| `GET /sources/{source_id}/subtrees/{level}/{x}/{y}.subtree` | Implicit-tiling subtree content. |
| `GET /sources/{source_id}/content/{level}/{x}/{y}.glb` | Binary glTF tile content. |
| `GET /tileset.json`, `/subtrees/...`, `/content/...` | Aliases for the configured default source. |

Structured 4xx and 5xx response bodies use an `error.code` and
`error.message`. Exact diagnostic wording, request logs, permissive development
CORS behavior, and the current `/metrics` JSON endpoint are experimental.

## Source configuration and secrets

The v0.1 YAML source fields and semantics documented by
[`source-geometry-model.md`](source-geometry-model.md) remain compatible across
v0.1 patch releases. Patch releases may add optional fields with defaults, but
will not rename a field, change an existing field's meaning, or silently accept
an invalid value. A future incompatible schema requires an explicitly
documented migration.

Committed catalogs must use `connection: ${DATABASE_URL}` rather than literal
credentials. Lucy resolves that exact placeholder from the process environment
at startup and fails with exit code 1 when it is absent. Other secret expansion
syntax is not supported in v0.1. `config/lucy.example.yaml` is the credential-free
reference catalog.

Full row validation, metadata-only startup validation, native-surface envelope
shortcuts, custom material behavior, and performance characteristics are not
compatibility guarantees. Invalid source geometry may still fail a request even
after metadata startup validation.

## Container contract

The production image is `ghcr.io/poorwym/lucy` and supports `linux/amd64` and
`linux/arm64`. It has:

- entrypoint `/usr/local/bin/lucy` and default command `serve`;
- default config `/etc/lucy/config.yaml` from `LUCY_CONFIG`;
- default bind `0.0.0.0:8080` from `LUCY_BIND` and exposed TCP port `8080`;
- a read-only config bind mount at `/etc/lucy/config.yaml`;
- `DATABASE_URL` injected at runtime, never baked into an image layer;
- optional `RUST_LOG` and `LUCY_LOG_FORMAT=json` logging controls;
- a `/health`-backed image health check;
- UID/GID `10001`, no root runtime, and graceful SIGTERM shutdown.

Override `LUCY_CONFIG`, `LUCY_BIND`, or the container command only when the
deployment also updates its mount, port, and health-check wiring consistently.

Images are immutable at full SemVer and `sha-<commit>` tags. The `latest` tag
may move only for a stable release. Release candidates must not move it.

## Standalone platforms

The v0.1 standalone archive matrix is:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

Windows, musl/static Linux, Kubernetes manifests, embedded viewers, and library
ABI stability are outside the v0.1 distribution contract.
