#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
check="$repo_root/scripts/check-release-version.sh"
release_workflow="$repo_root/.github/workflows/release.yml"

"$check" v0.10.0

for invalid_tag in 0.10.0 v0.9.0 v0.10.0-rc.1 refs/tags/v0.10.0; do
  if "$check" "$invalid_tag" >/dev/null 2>&1; then
    echo "release version check accepted invalid tag: $invalid_tag" >&2
    exit 1
  fi
done

if "$check" >/dev/null 2>&1; then
  echo "release version check accepted a missing tag" >&2
  exit 1
fi

if ! grep -F -- "--notes-file RELEASE_NOTES.md" "$release_workflow" >/dev/null; then
  echo "release workflow does not publish RELEASE_NOTES.md" >&2
  exit 1
fi
