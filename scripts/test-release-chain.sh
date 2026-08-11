#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n1)
[[ -n "$version" ]] || { echo "Could not read Cargo version" >&2; exit 1; }
tag="v${version}"

install_ref=$(sed -n 's/^NETFYR_SCRIPT_REF="\([^"]*\)"/\1/p' community-scripts/install/netfyr-install.sh)
wrapper_ref=$(sed -n 's/^NETFYR_SCRIPT_REF="\([^"]*\)"/\1/p' community-scripts/ct/netfyr.sh)
[[ "$install_ref" == "$tag" ]] || { echo "Installer ref $install_ref != $tag" >&2; exit 1; }
[[ "$wrapper_ref" == "$tag" ]] || { echo "Wrapper ref $wrapper_ref != $tag" >&2; exit 1; }

declared_helper=$(sed -n 's/^NETFYR_RELEASE_FUNC_SHA256="\([^"]*\)"/\1/p' community-scripts/install/netfyr-install.sh)
actual_helper=$(sha256sum deploy/netfyr-release.func | cut -d' ' -f1)
[[ "$declared_helper" == "$actual_helper" ]] || { echo "Release helper hash mismatch" >&2; exit 1; }

declared_installer=$(sed -n 's/^NETFYR_INSTALL_SHA256="\([^"]*\)"/\1/p' community-scripts/ct/netfyr.sh)
actual_installer=$(sha256sum community-scripts/install/netfyr-install.sh | cut -d' ' -f1)
[[ "$declared_installer" == "$actual_installer" ]] || { echo "Installer hash mismatch" >&2; exit 1; }

actual_wrapper=$(sha256sum community-scripts/ct/netfyr.sh | cut -d' ' -f1)
grep -Fq "netfyr-server/${tag}/community-scripts/ct/netfyr.sh" README.md || { echo "README tag mismatch" >&2; exit 1; }
grep -Fq "$actual_wrapper" README.md || { echo "README wrapper hash mismatch" >&2; exit 1; }
grep -Fq "badge/version-${version}-" README.md || { echo "README badge version mismatch" >&2; exit 1; }
grep -Fq "alt=\"Version ${version}\"" README.md || { echo "README badge label mismatch" >&2; exit 1; }
grep -Fq "immutable ${tag} tag" community-scripts/ct/netfyr.sh || { echo "Wrapper comment version mismatch" >&2; exit 1; }
grep -Fq "id=\"version\">${tag}</span>" web/index.html || { echo "Web version mismatch" >&2; exit 1; }

echo "Release chain $tag: OK"
