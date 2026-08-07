#!/usr/bin/env bash
# Transactionally replace an installed NetFyr package with a verified staging tree.
set -Eeuo pipefail

stage=${1:?verified staging directory required}
new_version=${2:?new version required}
netfyr_ip=${3:-}
prefix=${NETFYR_ROOT_PREFIX:-}

if [[ -n "$prefix" ]]; then
  [[ "$prefix" == /* && "$prefix" != / ]] || { echo "Invalid test root prefix" >&2; exit 1; }
fi
path() { printf '%s%s' "$prefix" "$1"; }

package=$(path /opt/netfyr)
config=$(path /etc/netfyr)
data=$(path /var/lib/netfyr)
web=$(path /usr/local/share/netfyr/web)
binary=$(path /usr/local/bin/netfyr-server)
unit=$(path /etc/systemd/system/netfyr.service)
marker=$(path /root/.netfyr)

[[ -d "$stage" && "$stage" != / && "$stage" != "$package" ]] || { echo "Invalid staging directory" >&2; exit 1; }
[[ -x "$stage/bin/netfyr-server" ]] || { echo "Staged binary missing" >&2; exit 1; }
[[ -x "$stage/deploy/install-package.sh" ]] || { echo "Staged installer missing" >&2; exit 1; }
[[ "$(<"$stage/VERSION")" == "$new_version" ]] || { echo "Staged version mismatch" >&2; exit 1; }
[[ "$new_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$ ]] || { echo "Invalid version" >&2; exit 1; }

fail_at() {
  [[ -n "$prefix" && ",${NETFYR_TEST_FAIL_AT:-}," == *",$1,"* ]]
}

service_stop() {
  [[ -n "$prefix" ]] || systemctl stop netfyr
}
service_start() {
  [[ -n "$prefix" ]] || systemctl start netfyr
}
health_check() {
  if [[ -n "$prefix" ]]; then
    ! fail_at health
  else
    curl -fsS http://127.0.0.1:8080/api/health >/dev/null &&
      curl -kfsS "https://${netfyr_ip}/api/health" >/dev/null
  fi
}

backup=""
rollback_required=0
service_stopped=0

restore_previous() {
  local failed=0
  if fail_at rollback; then
    return 1
  fi
  set +e
  service_stop >/dev/null 2>&1
  rm -rf "$package" "$config" "$data" "$web"
  rm -f "$binary" "$unit" "$marker"
  cp -a "$backup/package" "$package" || failed=1
  cp -a "$backup/config" "$config" || failed=1
  cp -a "$backup/data" "$data" || failed=1
  cp -a "$backup/web" "$web" || failed=1
  cp -a "$backup/netfyr-server" "$binary" || failed=1
  cp -a "$backup/netfyr.service" "$unit" || failed=1
  [[ ! -f "$backup/version-marker" ]] || cp -a "$backup/version-marker" "$marker" || failed=1
  if [[ -z "$prefix" ]]; then
    systemctl daemon-reload || failed=1
  fi
  service_start || failed=1
  if [[ -z "$prefix" ]]; then
    for _ in {1..20}; do
      health_check && break
      sleep 1
    done
    health_check || failed=1
  fi
  set -e
  return "$failed"
}

on_exit() {
  local rc=$?
  local restored=0
  trap - EXIT INT TERM
  if [[ "$rollback_required" == 1 ]]; then
    echo "NetFyr update failed; restoring previous installation" >&2
    if restore_previous; then
      restored=1
      echo "Previous NetFyr installation restored and healthy" >&2
    else
      echo "CRITICAL: rollback failed; backup retained at $backup" >&2
    fi
  elif [[ "$service_stopped" == 1 ]]; then
    if service_start; then
      restored=1
    else
      echo "CRITICAL: existing NetFyr service could not be restarted" >&2
    fi
  else
    restored=1
  fi
  [[ ! -e "$stage" ]] || rm -rf "$stage"
  if [[ "$restored" == 1 && -n "$backup" ]]; then
    rm -rf "$backup"
  fi
  exit "$rc"
}

trap on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

service_stop
service_stopped=1
backup=$(mktemp -d "$(path /opt/netfyr-backup.XXXXXX)")
if fail_at backup; then
  echo "Injected backup failure" >&2
  exit 1
fi
if ! {
  cp -a "$package" "$backup/package" &&
    cp -a "$config" "$backup/config" &&
    cp -a "$data" "$backup/data" &&
    cp -a "$web" "$backup/web" &&
    cp -a "$binary" "$backup/netfyr-server" &&
    cp -a "$unit" "$backup/netfyr.service"
}; then
  echo "Could not create a complete cold backup" >&2
  exit 1
fi
if [[ -f "$marker" ]] && ! cp -a "$marker" "$backup/version-marker"; then
  echo "Could not back up version marker" >&2
  exit 1
fi

rollback_required=1
rm -rf "$package"
mv "$stage" "$package"
stage=""

if fail_at replace; then
  echo "Injected replacement failure" >&2
  exit 1
fi

if [[ -n "$prefix" ]]; then
  if fail_at install || ! NETFYR_ROOT_PREFIX="$prefix" NETFYR_SKIP_RUNTIME=1 \
    "$package/deploy/install-package.sh" "$package"; then
    echo "Package installation failed" >&2
    exit 1
  fi
else
  if ! NETFYR_CONFIGURE_CADDY=0 "$package/deploy/install-package.sh" "$package"; then
    echo "Package installation failed" >&2
    exit 1
  fi
fi

if ! health_check; then
  echo "Health check failed after update" >&2
  exit 1
fi
if fail_at marker; then
  echo "Injected marker failure" >&2
  exit 1
fi

marker_tmp=$(mktemp "$(path /root/.netfyr.XXXXXX)")
printf '%s\n' "$new_version" >"$marker_tmp"
chmod 0600 "$marker_tmp"
mv "$marker_tmp" "$marker"

rollback_required=0
service_stopped=0
trap - EXIT INT TERM
rm -rf "$backup"
printf 'NetFyr updated transactionally to v%s\n' "$new_version"
