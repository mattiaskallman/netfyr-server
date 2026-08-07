#!/usr/bin/env bash
# Verify the structure and metadata of a NetFyr release archive.
set -euo pipefail

archive=${1:?Usage: verify-release.sh ARCHIVE}
[[ -f "$archive" ]] || { echo "Archive not found: $archive" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

tar -xzf "$archive" -C "$tmp"
mapfile -t roots < <(find "$tmp" -mindepth 1 -maxdepth 1 -type d)
[[ ${#roots[@]} -eq 1 ]] || { echo "Archive must have exactly one root directory" >&2; exit 1; }
root=${roots[0]}

for required in \
  bin/netfyr-server \
  web/index.html \
  web/app.js \
  web/style.css \
  web/i18n.js \
  config.example.toml \
  deploy/netfyr.service \
  deploy/install-package.sh \
  deploy/update-package.sh \
  deploy/netfyr-release.func \
  LICENSE \
  README.md \
  VERSION; do
  [[ -f "$root/$required" ]] || { echo "Missing: $required" >&2; exit 1; }
done

[[ -x "$root/bin/netfyr-server" ]] || { echo "Server binary is not executable" >&2; exit 1; }
version=$(<"$root/VERSION")
[[ "$(basename "$root")" == "netfyr-server-v${version}-linux-"* ]] || {
  echo "Archive root and VERSION disagree" >&2
  exit 1
}
grep -qx 'GNU AFFERO GENERAL PUBLIC LICENSE' <(sed -n '1p' "$root/LICENSE")
grep -q "version = \"$version\"" Cargo.toml

printf 'Release archive OK: version=%s root=%s\n' "$version" "$(basename "$root")"
