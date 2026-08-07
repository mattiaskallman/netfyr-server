#!/usr/bin/env bash
# Offline failure-injection tests for deploy/netfyr-release.func.
set -euo pipefail

cd "$(dirname "$0")/.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
root="netfyr-server-v1.0.0-linux-amd64"
mkdir -p "$work/package/$root/bin" "$work/package/$root/web" "$work/package/$root/deploy"
printf '#!/usr/bin/env bash\nexit 0\n' >"$work/package/$root/bin/netfyr-server"
chmod 0755 "$work/package/$root/bin/netfyr-server"
printf '<!doctype html>\n' >"$work/package/$root/web/index.html"
printf '#!/usr/bin/env bash\nexit 0\n' >"$work/package/$root/deploy/install-package.sh"
chmod 0755 "$work/package/$root/deploy/install-package.sh"
cp deploy/netfyr-release.func "$work/package/$root/deploy/netfyr-release.func"
printf '1.0.0\n' >"$work/package/$root/VERSION"
tar -C "$work/package" -czf "$work/safe.tar.gz" "$root"

make_metadata() {
  local archive=$1
  local filename="netfyr-server-v1.0.0-linux-amd64.tar.gz"
  printf '%s  %s\n' "$(sha256sum "$archive" | awk '{print $1}')" "$filename" >"$work/SHA256SUMS"
  jq -n --arg archive "https://example.invalid/$filename" \
    --arg sums "https://example.invalid/SHA256SUMS" \
    '{tag_name:"v1.0.0",assets:[{name:"netfyr-server-v1.0.0-linux-amd64.tar.gz",browser_download_url:$archive},{name:"SHA256SUMS",browser_download_url:$sums}]}' \
    >"$work/release.json"
}

source deploy/netfyr-release.func
_netfyr_download() {
  local url=$1 destination=$2
  case "$url" in
    *api.github.com*) cp "$work/release.json" "$destination" ;;
    */SHA256SUMS) cp "$work/SHA256SUMS" "$destination" ;;
    *.tar.gz) cp "$NETFYR_TEST_ARCHIVE" "$destination" ;;
    *) echo "Unexpected test URL: $url" >&2; return 1 ;;
  esac
}

# Valid fixture.
make_metadata "$work/safe.tar.gz"
NETFYR_TEST_ARCHIVE="$work/safe.tar.gz"
netfyr_fetch_release "$work/valid-target" latest
test "$NETFYR_FETCHED_VERSION" = 1.0.0
test -x "$work/valid-target/bin/netfyr-server"

# Tampered bytes with the original checksum must fail and leave no target.
cp "$work/safe.tar.gz" "$work/tampered.tar.gz"
printf 'tamper' >>"$work/tampered.tar.gz"
NETFYR_TEST_ARCHIVE="$work/tampered.tar.gz"
if netfyr_fetch_release "$work/tampered-target" latest 2>/dev/null; then
  echo "Tampered release was accepted" >&2
  exit 1
fi
test ! -e "$work/tampered-target"

# A correctly checksummed archive with a traversal member must still fail.
mkdir "$work/evil"
printf 'escape\n' >"$work/evil/payload"
tar -C "$work/evil" --transform='s|payload|../escape|' -czf "$work/unsafe.tar.gz" payload
make_metadata "$work/unsafe.tar.gz"
NETFYR_TEST_ARCHIVE="$work/unsafe.tar.gz"
if netfyr_fetch_release "$work/unsafe-target" latest 2>/dev/null; then
  echo "Unsafe release was accepted" >&2
  exit 1
fi
test ! -e "$work/unsafe-target"
test ! -e "$work/escape"

# Even with a valid checksum, special filesystem nodes are forbidden.
mkdir "$work/special"
mkfifo "$work/special/fifo"
tar -C "$work/special" -czf "$work/special.tar.gz" fifo
make_metadata "$work/special.tar.gz"
NETFYR_TEST_ARCHIVE="$work/special.tar.gz"
if netfyr_fetch_release "$work/special-target" latest 2>/dev/null; then
  echo "Special-node release was accepted" >&2
  exit 1
fi
test ! -e "$work/special-target"

printf 'release downloader tests: success, checksum/traversal/special-node rejection — OK\n'
