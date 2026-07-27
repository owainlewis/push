#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
check="$repo_root/scripts/check-release-version.sh"

"$check" v0.9.0

for invalid_tag in 0.9.0 v0.8.2 v0.9.0-rc.1 refs/tags/v0.9.0; do
  if "$check" "$invalid_tag" >/dev/null 2>&1; then
    echo "release version check accepted invalid tag: $invalid_tag" >&2
    exit 1
  fi
done

if "$check" >/dev/null 2>&1; then
  echo "release version check accepted a missing tag" >&2
  exit 1
fi
