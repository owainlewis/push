#!/usr/bin/env sh
set -eu

repo="owainlewis/push"
bin_dir="${BIN_DIR:-$HOME/.local/bin}"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "push install: missing required command: $1" >&2
    exit 1
  }
}

need curl
need tar

os="$(uname -s)"
arch="$(uname -m)"

case "$os:$arch" in
  Darwin:arm64) target="aarch64-apple-darwin" ;;
  Darwin:x86_64) target="x86_64-apple-darwin" ;;
  Linux:x86_64) target="x86_64-unknown-linux-gnu" ;;
  Linux:aarch64|Linux:arm64) target="aarch64-unknown-linux-gnu" ;;
  *)
    echo "push install: unsupported platform $os/$arch" >&2
    exit 1
    ;;
esac

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

api="https://api.github.com/repos/$repo/releases/latest"
asset_url="$(
  curl -fsSL "$api" \
    | sed -n 's/.*"browser_download_url": "\(.*push-v[^"]*-'"$target"'\.tar\.gz\)".*/\1/p' \
    | head -n 1
)"

if [ -z "$asset_url" ]; then
  echo "push install: no release asset found for $target" >&2
  exit 1
fi

echo "Downloading $asset_url"
curl -fsSL "$asset_url" -o "$tmp/push.tar.gz"
tar -xzf "$tmp/push.tar.gz" -C "$tmp"

mkdir -p "$bin_dir"
find "$tmp" -type f -name push -perm -111 -exec cp {} "$bin_dir/push" \;
chmod +x "$bin_dir/push"

echo "Installed push to $bin_dir/push"
case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) echo "Add $bin_dir to PATH to run push from any shell." ;;
esac
