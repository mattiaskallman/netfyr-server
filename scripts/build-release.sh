#!/usr/bin/env bash
# Build a reproducible NetFyr Server release archive for the current machine.
set -euo pipefail

cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n1)
[[ -n "$version" ]] || { echo "Could not read version from Cargo.toml" >&2; exit 1; }

case "${NETFYR_ARCH:-$(dpkg --print-architecture 2>/dev/null || uname -m)}" in
  amd64|x86_64) arch=amd64 ;;
  arm64|aarch64) arch=arm64 ;;
  *) echo "Unsupported architecture" >&2; exit 1 ;;
esac

cargo test --locked
cargo build --release --locked

binary_arch=$(file -b target/release/netfyr-server)
case "$arch:$binary_arch" in
  amd64:*x86-64*|arm64:*aarch64*) ;;
  *) echo "Built binary architecture does not match $arch: $binary_arch" >&2; exit 1 ;;
esac

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
package="$stage/netfyr-server-v${version}-linux-${arch}"
mkdir -p "$package/bin" "$package/web" "$package/deploy"

install -m 0755 target/release/netfyr-server "$package/bin/netfyr-server"
cp -a web/. "$package/web/"
find "$package/web" -type d -exec chmod 0755 {} +
find "$package/web" -type f -exec chmod 0644 {} +
install -m 0644 config.example.toml "$package/config.example.toml"
install -m 0644 deploy/netfyr.service "$package/deploy/netfyr.service"
install -m 0755 deploy/install-package.sh "$package/deploy/install-package.sh"
install -m 0755 deploy/update-package.sh "$package/deploy/update-package.sh"
install -m 0644 deploy/netfyr-release.func "$package/deploy/netfyr-release.func"
install -m 0644 LICENSE README.md "$package/"
printf '%s\n' "$version" >"$package/VERSION"

mkdir -p dist
archive="dist/netfyr-server-v${version}-linux-${arch}.tar.gz"
rm -f "$archive"

# SOURCE_DATE_EPOCH makes repeated builds from the same commit deterministic.
epoch=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}
TZ=UTC tar \
  --sort=name \
  --mtime="@$epoch" \
  --owner=0 --group=0 --numeric-owner \
  -C "$stage" \
  -czf "$archive" \
  "$(basename "$package")"

(
  cd dist
  sha256sum "$(basename "$archive")" >SHA256SUMS
)

printf 'Built %s\n' "$archive"
sha256sum "$archive"
