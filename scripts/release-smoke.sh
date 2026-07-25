#!/bin/sh
set -eu

if [ "$#" -ne 3 ]; then
    echo "usage: $0 <binary> <version> <sample-config>" >&2
    exit 2
fi

binary=$1
version=$2
sample_config=$3
port=${LUCY_SMOKE_PORT:-18080}

expected_version="lucy $version"
actual_version=$("$binary" --version)
if [ "$actual_version" != "$expected_version" ]; then
    echo "binary version mismatch: expected '$expected_version', got '$actual_version'" >&2
    exit 1
fi

temporary_directory=$(mktemp -d "${TMPDIR:-/tmp}/lucy-smoke.XXXXXX")
server_pid=

cleanup() {
    if [ -n "$server_pid" ] && kill -0 "$server_pid" 2>/dev/null; then
        kill -TERM "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    rm -rf "$temporary_directory"
}
trap cleanup EXIT HUP INT TERM

sed 's/startup: metadata/startup: none/' \
    "$sample_config" >"$temporary_directory/lucy.yaml"

"$binary" serve \
    --config "$temporary_directory/lucy.yaml" \
    --bind "127.0.0.1:$port" \
    >"$temporary_directory/server.log" 2>&1 &
server_pid=$!

attempt=0
until curl --fail --silent --show-error \
    "http://127.0.0.1:$port/health" \
    >"$temporary_directory/health.json"
do
    attempt=$((attempt + 1))
    if ! kill -0 "$server_pid" 2>/dev/null || [ "$attempt" -ge 30 ]; then
        cat "$temporary_directory/server.log" >&2
        echo "Lucy did not become ready" >&2
        exit 1
    fi
    sleep 1
done

curl --fail --silent --show-error \
    "http://127.0.0.1:$port/" \
    >"$temporary_directory/status.json"
curl --fail --silent --show-error \
    "http://127.0.0.1:$port/tileset.json" \
    >"$temporary_directory/tileset.json"

grep -q '"status":"ok"' "$temporary_directory/health.json"
grep -q "\"version\":\"$version\"" "$temporary_directory/status.json"
grep -q '"implicitTiling"' "$temporary_directory/tileset.json"

kill -TERM "$server_pid"
wait "$server_pid"
server_pid=

echo "Verified Lucy $version health, status, and tileset routes"
