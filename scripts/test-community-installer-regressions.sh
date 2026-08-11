#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)

grep -Fq 'source "$script_dir/netfyr-health.func"' "$root/deploy/update-package.sh" || {
  echo "transactional updater must use the bounded health helper" >&2
  exit 1
}
grep -Fq 'netfyr_wait_for_health 1 "$netfyr_ip"' "$root/deploy/update-package.sh" || {
  echo "transactional updater must verify local and HTTPS health with the helper" >&2
  exit 1
}
grep -Fq 'curl --connect-timeout 2 --max-time 3 -kfsS' "$root/community-scripts/ct/netfyr.sh" || {
  echo "Community update preflight curl must be bounded" >&2
  exit 1
}
grep -Fq '"$stage/deploy/update-package.sh" "$stage"' "$root/community-scripts/ct/netfyr.sh" || {
  echo "Community update must execute the verified staged updater" >&2
  exit 1
}
if grep -Fq 'for _ in {1..20}' "$root/deploy/update-package.sh"; then
  echo "Transactional rollback must not nest health retry loops" >&2
  exit 1
fi

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

# Exercise the real health helper with curl/sleep stubs. This proves local
# updates, HTTPS installs, startup retries, finite exhaustion and curl bounds.
[[ -f "$root/deploy/netfyr-health.func" ]] || {
  echo "missing health retry helper" >&2
  exit 1
}
# shellcheck source=../deploy/netfyr-health.func
source "$root/deploy/netfyr-health.func"

calls=0
https_calls=0
curl_mode=""
curl() {
  calls=$((calls + 1))
  [[ " $* " == *" --connect-timeout 2 "* ]] || {
    echo "curl missing --connect-timeout" >&2
    return 90
  }
  [[ " $* " == *" --max-time 3 "* ]] || {
    echo "curl missing --max-time" >&2
    return 91
  }
  case "$curl_mode" in
    local-retry)
      ((calls >= 3))
      ;;
    https-retry)
      if [[ "$*" == *"https://"* ]]; then
        https_calls=$((https_calls + 1))
        ((https_calls >= 3))
      else
        return 0
      fi
      ;;
    always-fail)
      return 1
      ;;
    *)
      echo "unknown curl test mode" >&2
      return 92
      ;;
  esac
}
sleep() { :; }

curl_mode=local-retry
calls=0
NETFYR_HEALTH_ATTEMPTS=3 netfyr_wait_for_health 0 192.0.2.10
[[ $calls -eq 3 ]] || { echo "local health retry count mismatch" >&2; exit 1; }

curl_mode=https-retry
calls=0
https_calls=0
NETFYR_HEALTH_ATTEMPTS=3 netfyr_wait_for_health 1 192.0.2.10
[[ $calls -eq 6 && $https_calls -eq 3 ]] || {
  echo "HTTPS health retry contract mismatch" >&2
  exit 1
}

curl_mode=always-fail
calls=0
if NETFYR_HEALTH_ATTEMPTS=3 netfyr_wait_for_health 0 192.0.2.10; then
  echo "exhausted health retries must fail" >&2
  exit 1
fi
[[ $calls -eq 3 ]] || { echo "health exhaustion must be finite" >&2; exit 1; }

echo "Community installer regression tests: OK"
