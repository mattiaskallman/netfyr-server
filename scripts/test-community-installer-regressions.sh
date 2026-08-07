#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)

# A mktemp path has no .caddyfile extension. Caddy therefore needs an explicit
# adapter or it attempts to decode the Caddyfile as JSON.
grep -Fq 'caddy validate --adapter caddyfile --config "$caddy_tmp"' \
  "$root/deploy/install-package.sh" || {
  echo "installer must validate temporary Caddy config with --adapter caddyfile" >&2
  exit 1
}

# The pinned Community Scripts framework currently trips over a multi-value
# var_tags default while loading saved advanced settings. Keep one safe app tag;
# the framework adds community-script itself.
if grep -Eq '^var_tags=.*;' "$root/community-scripts/ct/netfyr.sh"; then
  echo "Community Scripts default var_tags must not contain a semicolon list" >&2
  exit 1
fi

echo "Community installer regression tests: OK"
