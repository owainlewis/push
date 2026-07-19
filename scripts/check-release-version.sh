#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <release-tag>" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
release_tag="$1"
package_version="$({
  python3 - "$repo_root/Cargo.toml" <<'PY'
import pathlib
import sys
import tomllib

manifest = pathlib.Path(sys.argv[1])
with manifest.open("rb") as file:
    document = tomllib.load(file)

version = document.get("package", {}).get("version")
if not isinstance(version, str) or not version:
    raise SystemExit(f"missing package.version in {manifest}")

print(version)
PY
})"
expected_tag="v${package_version}"

if [ "$release_tag" != "$expected_tag" ]; then
  echo "release tag '${release_tag}' does not match Cargo.toml package version '${package_version}' (expected '${expected_tag}')" >&2
  exit 1
fi

echo "release tag '${release_tag}' matches Cargo.toml package version '${package_version}'"
