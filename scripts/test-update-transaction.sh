#!/usr/bin/env bash
# Prefix-isolated failure injection tests for deploy/update-package.sh.
set -euo pipefail

cd "$(dirname "$0")/.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

setup_case() {
  local name=$1
  prefix="$work/$name/root"
  stage="$work/$name/stage"
  mkdir -p \
    "$prefix/opt/netfyr/deploy" \
    "$prefix/etc/netfyr" \
    "$prefix/var/lib/netfyr" \
    "$prefix/usr/local/bin" \
    "$prefix/usr/local/share/netfyr/web" \
    "$prefix/etc/systemd/system" \
    "$prefix/root" \
    "$stage/bin" "$stage/web" "$stage/deploy"

  printf '0.9.0\n' >"$prefix/opt/netfyr/VERSION"
  printf 'old-package\n' >"$prefix/opt/netfyr/package-marker"
  printf 'old-config\n' >"$prefix/etc/netfyr/config.toml"
  printf 'old-db\n' >"$prefix/var/lib/netfyr/netfyr.db"
  printf 'old-key\n' >"$prefix/var/lib/netfyr/secrets.key"
  printf 'old-web\n' >"$prefix/usr/local/share/netfyr/web/index.html"
  printf 'old-binary\n' >"$prefix/usr/local/bin/netfyr-server"
  chmod 0755 "$prefix/usr/local/bin/netfyr-server"
  printf 'old-unit\n' >"$prefix/etc/systemd/system/netfyr.service"
  printf '0.9.0\n' >"$prefix/root/.netfyr"

  printf '#!/usr/bin/env bash\necho new-binary\n' >"$stage/bin/netfyr-server"
  chmod 0755 "$stage/bin/netfyr-server"
  printf 'new-web\n' >"$stage/web/index.html"
  cp deploy/install-package.sh deploy/update-package.sh "$stage/deploy/"
  chmod 0755 "$stage/deploy/install-package.sh" "$stage/deploy/update-package.sh"
  cp deploy/netfyr-release.func "$stage/deploy/netfyr-release.func"
  cp deploy/netfyr.service "$stage/deploy/netfyr.service"
  cp config.example.toml "$stage/config.example.toml"
  cp LICENSE "$stage/LICENSE"
  printf '1.0.0\n' >"$stage/VERSION"
}

assert_old() {
  grep -qx '0.9.0' "$prefix/opt/netfyr/VERSION"
  grep -qx 'old-package' "$prefix/opt/netfyr/package-marker"
  grep -qx 'old-config' "$prefix/etc/netfyr/config.toml"
  grep -qx 'old-db' "$prefix/var/lib/netfyr/netfyr.db"
  grep -qx 'old-key' "$prefix/var/lib/netfyr/secrets.key"
  grep -qx 'old-web' "$prefix/usr/local/share/netfyr/web/index.html"
  grep -qx 'old-binary' "$prefix/usr/local/bin/netfyr-server"
  grep -qx 'old-unit' "$prefix/etc/systemd/system/netfyr.service"
  grep -qx '0.9.0' "$prefix/root/.netfyr"
  ! compgen -G "$prefix/opt/netfyr-backup.*" >/dev/null
}

assert_failed_and_rolled_back() {
  local point=$1
  setup_case "$point"
  if NETFYR_ROOT_PREFIX="$prefix" NETFYR_TEST_FAIL_AT="$point" \
    ./deploy/update-package.sh "$stage" 1.0.0 192.0.2.10 >/dev/null 2>&1; then
    echo "Injected failure '$point' unexpectedly succeeded" >&2
    exit 1
  fi
  assert_old
  test ! -e "$stage"
}

# Successful transaction preserves config/data and updates package/runtime files.
setup_case success
NETFYR_ROOT_PREFIX="$prefix" ./deploy/update-package.sh "$stage" 1.0.0 192.0.2.10 >/dev/null
grep -qx '1.0.0' "$prefix/opt/netfyr/VERSION"
grep -qx 'old-config' "$prefix/etc/netfyr/config.toml"
grep -qx 'old-db' "$prefix/var/lib/netfyr/netfyr.db"
grep -qx 'old-key' "$prefix/var/lib/netfyr/secrets.key"
grep -qx 'new-web' "$prefix/usr/local/share/netfyr/web/index.html"
grep -q 'new-binary' "$prefix/usr/local/bin/netfyr-server"
grep -qx '1.0.0' "$prefix/root/.netfyr"
! compgen -G "$prefix/opt/netfyr-backup.*" >/dev/null

for point in backup replace install health marker; do
  assert_failed_and_rolled_back "$point"
done

# If rollback itself fails, its unique cold backup must remain available.
setup_case rollback-retention
if NETFYR_ROOT_PREFIX="$prefix" NETFYR_TEST_FAIL_AT="health,rollback" \
  ./deploy/update-package.sh "$stage" 1.0.0 192.0.2.10 >/dev/null 2>&1; then
  echo "Rollback failure scenario unexpectedly succeeded" >&2
  exit 1
fi
compgen -G "$prefix/opt/netfyr-backup.*" >/dev/null

printf 'transaction tests: success + backup/replace/install/health/marker rollback + backup retention — OK\n'
