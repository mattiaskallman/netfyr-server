#!/usr/bin/env bash

# Copyright (c) 2021-2026 community-scripts ORG
# Author: Mattias Källman (mattiaskallman)
# License: MIT | https://github.com/community-scripts/ProxmoxVED/raw/main/LICENSE
# Source: https://github.com/mattiaskallman/netfyr-server

source /dev/stdin <<<"$FUNCTIONS_FILE_PATH"
color
verb_ip6
catch_errors
setting_up_container
network_check
update_os

NETFYR_SCRIPT_REF="v1.0.1"
NETFYR_RELEASE_FUNC_SHA256="cd553e32cddb5c69c4121d4528d9aeb99fafae1d504d49773b330735c06910bd"

msg_info "Installing dependencies"
$STD apt install -y \
  ca-certificates \
  caddy \
  curl \
  iputils-ping \
  jq
msg_ok "Installed dependencies"

msg_info "Downloading and verifying NetFyr"
release_func=$(mktemp)
curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
  "https://raw.githubusercontent.com/mattiaskallman/netfyr-server/${NETFYR_SCRIPT_REF}/deploy/netfyr-release.func" \
  -o "$release_func"
printf '%s  %s\n' "$NETFYR_RELEASE_FUNC_SHA256" "$release_func" | sha256sum -c - >/dev/null
source "$release_func"
rm -f "$release_func"
netfyr_fetch_release /opt/netfyr latest
msg_ok "Downloaded and verified NetFyr v${NETFYR_FETCHED_VERSION}"

msg_info "Installing NetFyr"
NETFYR_CONFIGURE_CADDY=1 /opt/netfyr/deploy/install-package.sh /opt/netfyr "$LOCAL_IP"
marker_tmp=$(mktemp /root/.netfyr.XXXXXX)
printf '%s\n' "$NETFYR_FETCHED_VERSION" >"$marker_tmp"
chmod 0600 "$marker_tmp"
mv "$marker_tmp" /root/.netfyr
msg_ok "Installed NetFyr v${NETFYR_FETCHED_VERSION}"

motd_ssh
customize
cleanup_lxc
