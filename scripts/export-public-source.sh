#!/usr/bin/env bash
# Export a clean public source snapshot from the internal repository.
set -euo pipefail

cd "$(dirname "$0")/.."
destination=${1:-dist/public-source}
treeish=${NETFYR_PUBLIC_TREEISH:-HEAD}

rm -rf "$destination"
mkdir -p "$destination"
git archive --format=tar "$treeish" | tar -xf - -C "$destination"

[[ ! -e "$destination/STATUS.md" ]] || { echo "Internal STATUS.md leaked into export" >&2; exit 1; }
[[ ! -e "$destination/deploy/netfyr-ca-root.crt" ]] || { echo "Site CA leaked into export" >&2; exit 1; }

if find "$destination" -type f | grep -Ei '(^|/)([^/]*\.db(-shm|-wal)?|secrets\.(enc|key)|config\.toml|dev-config\.toml|[^/]*\.(pem|key|crt))$'; then
  echo "Private runtime material found in public export" >&2
  exit 1
fi

printf 'Public source snapshot exported to %s\n' "$destination"