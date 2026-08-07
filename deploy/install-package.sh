#!/usr/bin/env bash
# Install an unpacked NetFyr release. Safe to run repeatedly.
set -euo pipefail

package_root=${1:-$(cd "$(dirname "$0")/.." && pwd)}
netfyr_ip=${2:-$(hostname -I | awk '{print $1}')}
prefix=${NETFYR_ROOT_PREFIX:-}
configure_caddy=${NETFYR_CONFIGURE_CADDY:-1}
skip_runtime=${NETFYR_SKIP_RUNTIME:-0}

path() { printf '%s%s' "$prefix" "$1"; }

valid_ipv4() {
  local ip=$1 octet
  local -a parts
  IFS=. read -r -a parts <<<"$ip"
  [[ ${#parts[@]} -eq 4 ]] || return 1
  for octet in "${parts[@]}"; do
    [[ "$octet" =~ ^[0-9]{1,3}$ ]] || return 1
    ((10#$octet <= 255)) || return 1
  done
}

[[ -f "$package_root/bin/netfyr-server" ]] || { echo "Missing release binary" >&2; exit 1; }
[[ -f "$package_root/web/index.html" ]] || { echo "Missing web interface" >&2; exit 1; }
[[ -f "$package_root/deploy/netfyr.service" ]] || { echo "Missing systemd unit" >&2; exit 1; }
[[ -f "$package_root/LICENSE" ]] || { echo "Missing licence" >&2; exit 1; }

if [[ -z "$prefix" && ${EUID:-$(id -u)} -ne 0 ]]; then
  echo "Run as root" >&2
  exit 1
fi

owner_args=()
if [[ -z "$prefix" ]]; then
  if ! id netfyr >/dev/null 2>&1; then
    useradd --system --no-create-home --shell /usr/sbin/nologin netfyr
  fi
  owner_args=(-o netfyr -g netfyr)
fi

install -d -m 0750 "${owner_args[@]}" "$(path /var/lib/netfyr)"
install -d -m 0755 "$(path /etc/netfyr)"
install -d -m 0755 "$(path /usr/local/bin)"
install -d -m 0755 "$(path /usr/local/share/netfyr/web)"
install -d -m 0755 "$(path /usr/local/share/doc/netfyr-server)"
install -d -m 0755 "$(path /etc/systemd/system)"

install -m 0755 "$package_root/bin/netfyr-server" "$(path /usr/local/bin/netfyr-server)"
find "$(path /usr/local/share/netfyr/web)" -mindepth 1 -delete
cp -a "$package_root/web/." "$(path /usr/local/share/netfyr/web/)"
find "$(path /usr/local/share/netfyr/web)" -type d -exec chmod 0755 {} +
find "$(path /usr/local/share/netfyr/web)" -type f -exec chmod 0644 {} +
install -m 0644 "$package_root/LICENSE" "$(path /usr/local/share/doc/netfyr-server/LICENSE)"
install -m 0644 "$package_root/deploy/netfyr.service" "$(path /etc/systemd/system/netfyr.service)"

if [[ ! -f "$(path /etc/netfyr/config.toml)" ]]; then
  if [[ -z "$prefix" ]]; then
    install -m 0640 -o root -g netfyr "$package_root/config.example.toml" "$(path /etc/netfyr/config.toml)"
  else
    install -m 0640 "$package_root/config.example.toml" "$(path /etc/netfyr/config.toml)"
  fi
fi

# Prefix mode is used by packaging tests and never touches the host runtime.
if [[ -n "$prefix" || "$skip_runtime" == 1 ]]; then
  echo "NetFyr files installed under ${prefix:-/}; runtime setup skipped"
  exit 0
fi

gid=$(id -g netfyr)
sysctl -w "net.ipv4.ping_group_range=$gid $gid" >/dev/null
printf 'net.ipv4.ping_group_range=%s %s\n' "$gid" "$gid" >/etc/sysctl.d/90-netfyr.conf

if [[ "$configure_caddy" == 1 ]]; then
  command -v caddy >/dev/null 2>&1 || { echo "Caddy is required for packaged HTTPS" >&2; exit 1; }
  valid_ipv4 "$netfyr_ip" || { echo "Invalid container IPv4 address: $netfyr_ip" >&2; exit 1; }
  caddy_tmp=$(mktemp)
  caddy_backup=$(mktemp)
  had_caddy_config=0
  cat >"$caddy_tmp" <<EOF
{
    local_certs
}

https://${netfyr_ip} {
    tls internal
    reverse_proxy 127.0.0.1:8080
}
EOF
  if ! caddy validate --adapter caddyfile --config "$caddy_tmp"; then
    rm -f "$caddy_tmp" "$caddy_backup"
    echo "Caddy configuration validation failed; existing configuration unchanged" >&2
    exit 1
  fi
  if [[ -f /etc/caddy/Caddyfile ]]; then
    cp -a /etc/caddy/Caddyfile "$caddy_backup"
    had_caddy_config=1
  fi
  install -m 0644 "$caddy_tmp" /etc/caddy/Caddyfile
  rm -f "$caddy_tmp"
fi

systemctl daemon-reload
systemctl enable -q netfyr
[[ "$configure_caddy" == 1 ]] && systemctl enable -q caddy
systemctl restart netfyr
if [[ "$configure_caddy" == 1 ]] && ! systemctl restart caddy; then
  if [[ "$had_caddy_config" == 1 ]]; then
    install -m 0644 "$caddy_backup" /etc/caddy/Caddyfile
  else
    rm -f /etc/caddy/Caddyfile
  fi
  systemctl restart caddy 2>/dev/null || true
  rm -f "$caddy_backup"
  echo "Caddy failed to start; previous configuration restored" >&2
  exit 1
fi
[[ "$configure_caddy" == 1 ]] && rm -f "$caddy_backup"

if [[ "$configure_caddy" == 1 ]]; then
  for _ in {1..20}; do
    curl -kfsS "https://${netfyr_ip}/api/health" >/dev/null && break
    sleep 1
  done
  curl -kfsS "https://${netfyr_ip}/api/health" >/dev/null
  ca=/var/lib/caddy/.local/share/caddy/pki/authorities/local/root.crt
  [[ -f "$ca" ]] && install -m 0644 "$ca" /root/netfyr-ca-root.crt
  echo "NetFyr: https://${netfyr_ip}"
  [[ -f /root/netfyr-ca-root.crt ]] && echo "Local CA: /root/netfyr-ca-root.crt"
else
  curl -fsS http://127.0.0.1:8080/api/health >/dev/null
fi

echo "First admin password: journalctl -u netfyr -n 50 | sed -n '/FIRST RUN/,+4p'"
