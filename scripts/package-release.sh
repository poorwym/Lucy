#!/bin/sh
set -eu

if [ "$#" -ne 4 ]; then
  echo "usage: $0 <version> <target> <binary> <output-directory>" >&2
  exit 2
fi

version=$1
target=$2
binary=$3
output_directory=$4

if ! printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "version must be a stable SemVer without a leading v: $version" >&2
  exit 2
fi

if [ ! -x "$binary" ]; then
  echo "Lucy binary is missing or not executable: $binary" >&2
  exit 1
fi

expected_version="lucy $version"
actual_version=$("$binary" --version)
if [ "$actual_version" != "$expected_version" ]; then
  echo "binary version mismatch: expected '$expected_version', got '$actual_version'" >&2
  exit 1
fi

script_directory=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repository_root=$(CDPATH='' cd -- "$script_directory/.." && pwd)
archive_root="lucy-v${version}-${target}"
archive_name="${archive_root}.tar.gz"

mkdir -p "$output_directory"
output_directory=$(CDPATH='' cd -- "$output_directory" && pwd)
staging_directory=$(mktemp -d "${TMPDIR:-/tmp}/lucy-release.XXXXXX")

cleanup() {
  rm -rf "$staging_directory"
}
trap cleanup EXIT HUP INT TERM

mkdir "$staging_directory/$archive_root"
cp "$binary" "$staging_directory/$archive_root/lucy"
cp "$repository_root/LICENSE" "$staging_directory/$archive_root/LICENSE"
cp "$repository_root/README.md" "$staging_directory/$archive_root/README.md"
cp "$repository_root/config/lucy.example.yaml" \
  "$staging_directory/$archive_root/lucy.example.yaml"
chmod 0755 "$staging_directory/$archive_root/lucy"
chmod 0644 \
  "$staging_directory/$archive_root/LICENSE" \
  "$staging_directory/$archive_root/README.md" \
  "$staging_directory/$archive_root/lucy.example.yaml"

tar -czf "$output_directory/$archive_name" \
  -C "$staging_directory" "$archive_root"

if command -v sha256sum >/dev/null 2>&1; then
  (
    cd "$output_directory"
    sha256sum "$archive_name" >"$archive_name.sha256"
    sha256sum --check "$archive_name.sha256"
  )
else
  (
    cd "$output_directory"
    shasum -a 256 "$archive_name" >"$archive_name.sha256"
    shasum -a 256 --check "$archive_name.sha256"
  )
fi

echo "Packaged $output_directory/$archive_name"
