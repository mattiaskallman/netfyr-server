#!/usr/bin/env bash
# Require two clean builds in the same pinned environment to be byte-identical.
set -euo pipefail

cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n1)
case "$(dpkg --print-architecture 2>/dev/null || uname -m)" in
  amd64|x86_64) arch=amd64 ;;
  arm64|aarch64) arch=arm64 ;;
  *) echo "Unsupported architecture" >&2; exit 1 ;;
esac
archive="dist/netfyr-server-v${version}-linux-${arch}.tar.gz"
first=$(mktemp)
trap 'rm -f "$first"' EXIT

# Keep one epoch for both builds. In a Git checkout this is the commit time;
# exported source snapshots fall back to the invocation time.
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}
export SOURCE_DATE_EPOCH

cargo clean
./scripts/build-release.sh
cp "$archive" "$first"
first_hash=$(sha256sum "$first" | awk '{print $1}')

cargo clean
./scripts/build-release.sh
second_hash=$(sha256sum "$archive" | awk '{print $1}')

if ! cmp -s "$first" "$archive"; then
  echo "Release is not reproducible in this build environment" >&2
  echo "first:  $first_hash" >&2
  echo "second: $second_hash" >&2
  exit 1
fi

printf 'Reproducibility check passed: %s\n' "$second_hash"
