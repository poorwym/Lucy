#!/bin/sh
set -eu

project_name="lucy-container-smoke-$$"
export POSTGRES_PORT=0
export LUCY_PORT=$((20000 + ($$ % 20000)))

cleanup() {
    docker compose --project-name "$project_name" --profile release down --volumes --remove-orphans
}
trap cleanup EXIT INT TERM

docker compose --project-name "$project_name" up --detach --build --wait postgres
docker compose --project-name "$project_name" exec --no-TTY postgres \
    psql -v ON_ERROR_STOP=1 -U lucy -d lucy -f - < fixtures/postgis/poc_buildings.sql
docker compose --project-name "$project_name" --profile release up \
    --detach --build --wait lucy-release

runtime_uid=$(docker compose --project-name "$project_name" --profile release \
    exec --no-TTY lucy-release id -u)
test "$runtime_uid" = "10001"
docker compose --project-name "$project_name" --profile release \
    exec --no-TTY lucy-release sh -c \
    'test ! -e /workspace && test ! -e /frontend && test ! -e /fixtures && ! command -v cargo && ! command -v rustc'

curl --fail --silent --show-error "http://127.0.0.1:${LUCY_PORT}/health" >/dev/null
curl --fail --silent --show-error \
    "http://127.0.0.1:${LUCY_PORT}/sources/sample_buildings/tileset.json" >/dev/null
curl --fail --silent --show-error \
    "http://127.0.0.1:${LUCY_PORT}/sources/sample_buildings/subtrees/0/0/0.subtree" >/dev/null
curl --fail --silent --show-error \
    "http://127.0.0.1:${LUCY_PORT}/sources/sample_buildings/content/0/0/0.glb" >/dev/null

echo "Lucy production container health, tileset, subtree, and GLB smoke test passed"
