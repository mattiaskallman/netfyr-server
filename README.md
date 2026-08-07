<h1 align="center">NetFyr Server</h1>

<p align="center">
  Network monitoring as a service — quiet until it matters.
</p>

<p align="center">
  <img src="https://img.shields.io/badge/version-1.0.0-2f855a" alt="Version 1.0.0">
  <img src="https://img.shields.io/badge/license-AGPL--3.0-blue" alt="License: AGPL-3.0">
  <img src="https://img.shields.io/badge/platform-Debian%2013-lightgrey" alt="Platform: Debian 13">
  <img src="https://img.shields.io/badge/built%20with-Rust%20%2B%20Axum-orange" alt="Built with Rust and Axum">
</p>

NetFyr Server watches devices and services on the local network, damps flapping
so a single dropped reply never wakes anyone, and raises alarms over webhook,
e-mail, MQTT and SMS. It is designed for sites where monitoring runs around the
clock and the operator should only be interrupted when something actually needs
attention.

The backend is Rust, Axum and SQLite. The web interface is plain HTML, CSS and
JavaScript without CDN or build-time network dependencies, making NetFyr suited
to air-gapped installations.

## Features

- **Monitoring** — ICMP, TCP and HTTP probes with per-device intervals
- **Flap damping** — hysteresis on both down and recovery transitions
- **Maintenance windows** — one-off or recurring, per group or device
- **Dependency damping** — alarms behind a failed uplink are held back
- **Deferred delivery** — suppressed alarms are delivered when suppression lifts
- **Alarm channels** — webhook, SMTP, MQTT and Teltonika SMS
- **SMS escalation** — one recipient at a time with acknowledgement sessions
- **Statistics** — availability, latency and incidents over 24 h to 180 days
- **Users and roles** — admin/user separation enforced in the backend
- **Audit trail** — security-relevant and mutating actions are traceable
- **Swedish and English** — personal UI language and shared engine language
- **Watchdog** — optional TCP heartbeat for an external supervision device

## Quick installation on Proxmox VE

The Community Scripts-compatible installer creates a Debian 13 LXC and installs
NetFyr with a dedicated service account, systemd hardening and HTTPS through
Caddy's local CA.

When the installer has been accepted upstream, the canonical command will use
`community-scripts/ProxmoxVE`. Until then, use the files under
`community-scripts/` from this repository or its ProxmoxVED test branch.

```bash
(
  set -euo pipefail
  script=$(mktemp)
  trap 'rm -f "$script"' EXIT
  curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
    https://raw.githubusercontent.com/mattiaskallman/netfyr-server/v1.0.0/community-scripts/ct/netfyr.sh \
    -o "$script"
  printf '%s  %s\n' \
    36918923838dd35e4693ce2137f5f8770e37518076c214dece3f0618fab3c059 \
    "$script" | sha256sum -c -
  bash "$script"
)
```

The checksum intentionally pins the initial root-level bootstrap. Do not bypass
it by piping a changing branch directly into a shell.

The generated local CA certificate must be trusted once on each client. The
installer prints both its location and the NetFyr URL.

## Build from source

Requires the pinned Rust toolchain from `rust-toolchain.toml`. The checked-in
`Cargo.lock` fixes dependency resolution.

```bash
cargo test --locked
cargo build --release --locked
```

Run for development with a local configuration:

```bash
cargo run -- --config ./dev-config.toml
```

Verify the health endpoint:

```bash
curl -s http://127.0.0.1:8080/api/health
```

## Release archive

`scripts/build-release.sh` builds the current architecture with deterministic
archive metadata. `scripts/check-reproducible-release.sh` performs two clean
builds in the same pinned environment and requires byte-identical archives:

```bash
./scripts/build-release.sh
sha256sum -c dist/SHA256SUMS
./scripts/check-reproducible-release.sh
```

A release archive contains the server binary, web interface, example
configuration, systemd unit, README, version file and full AGPL licence.
GitHub Actions builds both `amd64` and `arm64` assets when a `v*` tag is pushed.

## Production layout

| Path | Contents |
|---|---|
| `/usr/local/bin/netfyr-server` | Server binary |
| `/usr/local/share/netfyr/web/` | Web interface |
| `/usr/local/share/doc/netfyr-server/LICENSE` | Licence text |
| `/etc/netfyr/config.toml` | Installation-specific configuration |
| `/var/lib/netfyr/netfyr.db` | SQLite database |
| `/var/lib/netfyr/secrets.enc` | Encrypted secrets |
| `/var/lib/netfyr/secrets.key` | Local encryption key |

Configuration and data are never included in release archives and are not
overwritten by upgrades.

## First start

If the database has no users, NetFyr creates `admin` with a generated one-time
password. Read it locally from the service journal:

```bash
journalctl -u netfyr -n 50 | sed -n '/FÖRSTA KÖRNINGEN/,+4p'
```

The password must be changed at first login.

## Security model

- Passwords are hashed with Argon2id.
- Only SHA-256 hashes of session tokens are stored.
- Session cookies are `HttpOnly`, `SameSite=Strict` and `Secure` under HTTPS.
- Roles are enforced by server middleware, never by hidden UI controls.
- Secrets are encrypted locally and their values are never returned by the API.
- The systemd service runs as the unprivileged `netfyr` account with a restricted
  filesystem and no raw-socket capability.
- Caddy terminates HTTPS; NetFyr itself listens only on `127.0.0.1:8080` in the
  packaged installation.

NetFyr supports secure operation but cannot by itself make an organisation
compliant with GDPR, NIS2 or other regulation. Deployment, access control,
backups, retention decisions and incident procedures remain the operator's
responsibility.

## Updates and backups

The Community Scripts updater downloads to a separate staging directory and
verifies the release against its published `SHA256SUMS` before stopping NetFyr.
It then takes a unique cold backup of configuration, database, encryption key,
installed package, binary, web assets, systemd unit and version marker. The new
version marker is written only after local and HTTPS health checks pass. A
download, install or health-check failure restores and verifies the previous
installation; a backup is retained if rollback itself cannot be verified.

For ordinary backups, stop the service or use SQLite's online backup facility;
do not copy a live database file without its WAL state.

## Licence

[GNU Affero General Public License v3.0 only](LICENSE) (`AGPL-3.0-only`).

You may use, study, modify and redistribute NetFyr. If you distribute a modified
version, or let others use a modified version over a network, the corresponding
source must be offered under the same licence as required by AGPL section 13.
Private modifications used only by you do not trigger that distribution duty.

This is a simplified summary, not legal advice. The full `LICENSE` text governs.

Pull requests are welcome. Contributions are released under AGPL-3.0-only.

## Copyright

Copyright © Mattias Källman
