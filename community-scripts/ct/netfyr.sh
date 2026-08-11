#!/usr/bin/env bash
# Copyright (c) 2021-2026 community-scripts ORG
# Author: Mattias Källman (mattiaskallman)
# License: MIT | https://github.com/community-scripts/ProxmoxVED/raw/main/LICENSE
# Source: https://github.com/mattiaskallman/netfyr-server

NETFYR_CS_REF="4c9bb2636d0ebecef074c3661b170a3496d9cb37"
NETFYR_CS_BASE="https://raw.githubusercontent.com/community-scripts/ProxmoxVED/${NETFYR_CS_REF}"
NETFYR_BUILD_FUNC_SHA256="85908170cfb8d0dff244a354822b380be7ec335c792ea7fbe7a11bd2ec72cf00"
NETFYR_SCRIPT_REF="v1.2.1"
NETFYR_INSTALL_SHA256="fe663aeae5b1a50722894d439fce7f6aa2c554637af15ebc02edc8ccd4b2e20a"

# build.func sources additional helpers immediately, so pin its base URL
# before sourcing it. Otherwise those helpers would still come from main.
export COMMUNITY_SCRIPTS_URL="$NETFYR_CS_BASE"
export _CS_DEFAULT_URL="$NETFYR_CS_BASE"

if [[ -f "$(dirname "${BASH_SOURCE[0]}")/../misc/build.func" ]]; then
  source "$(dirname "${BASH_SOURCE[0]}")/../misc/build.func"
else
  _netfyr_build_func=$(mktemp)
  curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
    "${NETFYR_CS_BASE}/misc/build.func" -o "$_netfyr_build_func"
  printf '%s  %s\n' "$NETFYR_BUILD_FUNC_SHA256" "$_netfyr_build_func" | sha256sum -c - >/dev/null
  source "$_netfyr_build_func"
  rm -f "$_netfyr_build_func"
fi

# Standalone mode routes only NetFyr's installer to the immutable v1.2.1 tag.
eval "$(declare -f _cs_fetch_text | sed '1s/_cs_fetch_text/_netfyr_upstream_fetch_text/')"
_cs_fetch_text() {
  if [[ "$1" == "install/netfyr-install.sh" ]]; then
    local tmp
    tmp=$(mktemp)
    curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
      "https://raw.githubusercontent.com/mattiaskallman/netfyr-server/${NETFYR_SCRIPT_REF}/community-scripts/install/netfyr-install.sh" \
      -o "$tmp"
    printf '%s  %s\n' "$NETFYR_INSTALL_SHA256" "$tmp" | sha256sum -c - >/dev/null
    cat "$tmp"
    rm -f "$tmp"
  else
    _netfyr_upstream_fetch_text "$@"
  fi
}

APP="NetFyr"
var_tags="${var_tags:-monitoring}"
var_cpu="${var_cpu:-2}"
var_ram="${var_ram:-2048}"
var_disk="${var_disk:-8}"
var_os="${var_os:-debian}"
var_version="${var_version:-13}"
var_arm64="${var_arm64:-yes}"
var_unprivileged="${var_unprivileged:-1}"

header_info "$APP"
variables
color
catch_errors

_netfyr_valid_ipv4() {
  local ip=$1 octet
  local -a parts
  IFS=. read -r -a parts <<<"$ip"
  [[ ${#parts[@]} -eq 4 ]] || return 1
  for octet in "${parts[@]}"; do
    [[ "$octet" =~ ^[0-9]{1,3}$ ]] || return 1
    ((10#$octet <= 255)) || return 1
  done
}

function update_script() {
  header_info
  check_container_storage
  check_container_resources

  if [[ ! -d /opt/netfyr || ! -x /usr/local/bin/netfyr-server ]]; then
    msg_error "No ${APP} installation found!"
    exit 1
  fi

  LOCAL_IP=$(hostname -I | awk '{print $1}')
  if ! _netfyr_valid_ipv4 "$LOCAL_IP"; then
    msg_error "Could not determine a valid container IPv4 address"
    exit 1
  fi
  if ! curl --connect-timeout 2 --max-time 3 -kfsS "https://${LOCAL_IP}/api/health" >/dev/null; then
    msg_error "Existing NetFyr HTTPS health check failed; update aborted before changes"
    exit 1
  fi

  # Download, checksum and validate the complete archive before downtime begins.
  stage=$(mktemp -d /opt/netfyr-stage.XXXXXX)
  rmdir "$stage"
  source /opt/netfyr/deploy/netfyr-release.func
  if ! netfyr_fetch_release "$stage" latest; then
    rm -rf "$stage"
    msg_error "Could not download and verify the NetFyr release; existing service unchanged"
    exit 1
  fi

  current_version=$(cat /root/.netfyr 2>/dev/null || cat /opt/netfyr/VERSION 2>/dev/null || true)
  if [[ "$current_version" == "$NETFYR_FETCHED_VERSION" ]]; then
    rm -rf "$stage"
    msg_ok "NetFyr is already up-to-date (v${current_version})"
    exit 0
  fi

  msg_info "Installing verified NetFyr v${NETFYR_FETCHED_VERSION} transactionally"
  if ! "$stage/deploy/update-package.sh" "$stage" "$NETFYR_FETCHED_VERSION" "$LOCAL_IP"; then
    msg_error "Update failed; see rollback result above"
    exit 1
  fi
  msg_ok "Updated NetFyr to v${NETFYR_FETCHED_VERSION} successfully"
  exit 0
}

start
build_container
description

msg_ok "Completed successfully!\n"
echo -e "${CREATING}${GN}${APP} setup has been successfully initialized!${CL}"
echo -e "${INFO}${YW}Access it using the following URL:${CL}"
echo -e "${GATEWAY}${BGN}https://${IP}${CL}"
echo -e "${INFO}${YW}Local CA certificate inside the container: /root/netfyr-ca-root.crt${CL}"
