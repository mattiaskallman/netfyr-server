#!/usr/bin/env bash
# Verify and safely extract the structure and metadata of a NetFyr release archive.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
archive=${1:?Usage: verify-release.sh ARCHIVE}
[[ -f "$archive" ]] || { echo "Archive not found: $archive" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

archive_root=$(python3 - "$archive" "$tmp" <<'PY'
import os
import pathlib
import shutil
import stat
import sys
import tarfile

archive = sys.argv[1]
tmp = pathlib.Path(sys.argv[2])
snapshot = tmp / "snapshot.tar.gz"
extract_root = tmp / "extracted"
MAX_ARCHIVE_SIZE = 256 * 1024 * 1024
MAX_MEMBER_SIZE = 256 * 1024 * 1024
MAX_TOTAL_SIZE = 512 * 1024 * 1024
MAX_MEMBERS = 10_000

# Snapshotta från en O_NOFOLLOW-deskriptor till vår privata 0700-katalog.
# Validering och extraktion arbetar därefter endast mot denna oföränderliga
# privata kopia, så ett path-/inode-byte inte kan skapa TOCTOU.
flags = os.O_RDONLY | os.O_NOFOLLOW
fd = os.open(archive, flags)
try:
    source_stat = os.fstat(fd)
    if not stat.S_ISREG(source_stat.st_mode):
        raise SystemExit("Archive must be a regular file")
    if source_stat.st_size > MAX_ARCHIVE_SIZE:
        raise SystemExit("Archive is too large")
    with os.fdopen(fd, "rb", closefd=False) as source, snapshot.open("xb") as destination:
        copied = 0
        while True:
            chunk = source.read(1024 * 1024)
            if not chunk:
                break
            copied += len(chunk)
            if copied > MAX_ARCHIVE_SIZE:
                raise SystemExit("Archive grew beyond the size limit")
            destination.write(chunk)
        destination.flush()
        os.fsync(destination.fileno())
finally:
    os.close(fd)

def member_signature(member):
    return (
        member.name,
        "dir" if member.isdir() else "file",
        member.size,
        member.mode & 0o7777,
    )

seen = {}
root = None
total_size = 0
snapshot_fd = os.open(snapshot, os.O_RDONLY | os.O_NOFOLLOW)
try:
    with os.fdopen(snapshot_fd, "rb", closefd=False) as archive_file:
        # Streamläge cachar inga TarInfo i tf.members. Vi behåller bara en
        # begränsad lista av små metadata-tupler, inte TarInfo-objekten.
        members = []
        with tarfile.open(fileobj=archive_file, mode="r|gz") as tf:
            for member in tf:
                # Äldre Pythonversioner cachar även i r|gz. Släpp den aktuella
                # interna referensen direkt; vår egen lista innehåller bara metadata.
                tf.members.clear()
                if tf.members:
                    raise SystemExit("Internal error: streaming tar reader cached members")
                if len(members) >= MAX_MEMBERS:
                    raise SystemExit("Archive has too many members")
                path = pathlib.PurePosixPath(member.name)
                parts = path.parts
                if not parts or path.is_absolute() or any(part in ("", ".", "..") for part in parts):
                    raise SystemExit(f"Unsafe archive path: {member.name!r}")
                if root is None:
                    root = parts[0]
                elif parts[0] != root:
                    raise SystemExit("Archive must have exactly one root directory")
                normalized = str(path)
                if normalized in seen:
                    raise SystemExit(f"Duplicate archive member: {normalized}")
                if not (member.isdir() or member.isreg()):
                    raise SystemExit(f"Unsafe archive member type: {normalized}")
                if member.size < 0 or member.size > MAX_MEMBER_SIZE:
                    raise SystemExit(f"Archive member is too large: {normalized}")
                if member.isreg():
                    total_size += member.size
                    if total_size > MAX_TOTAL_SIZE:
                        raise SystemExit("Archive expands beyond the total size limit")
                seen[normalized] = "dir" if member.isdir() else "file"
                members.append(member_signature(member))

        if not root or seen.get(root) != "dir":
            raise SystemExit("Archive must have one explicit root directory")
        for normalized in seen:
            parent = pathlib.PurePosixPath(normalized).parent
            while str(parent) not in (".", root):
                if seen.get(str(parent)) == "file":
                    raise SystemExit(f"File used as directory: {parent}")
                parent = parent.parent

        # Läs om samma privata snapshot via samma O_NOFOLLOW-öppnade fd.
        # Varje medlem måste matcha den fullständigt validerade metadataföljden.
        archive_file.seek(0)
        extract_root.mkdir(mode=0o700)
        extracted = 0
        with tarfile.open(fileobj=archive_file, mode="r|gz") as tf:
            for member in tf:
                # Äldre Pythonversioner cachar även i r|gz. Släpp den aktuella
                # interna referensen direkt; vår egen lista innehåller bara metadata.
                tf.members.clear()
                if tf.members:
                    raise SystemExit("Internal error: streaming tar reader cached members")
                if extracted >= len(members) or member_signature(member) != members[extracted]:
                    raise SystemExit("Archive changed between validation and extraction")
                extracted += 1
                target = extract_root.joinpath(*pathlib.PurePosixPath(member.name).parts)
                if member.isdir():
                    target.mkdir(mode=0o755, parents=True, exist_ok=True)
                    continue
                target.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
                source = tf.extractfile(member)
                if source is None:
                    raise SystemExit(f"Could not read archive member: {member.name}")
                mode = 0o755 if member.mode & 0o111 else 0o644
                out_fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode)
                with source, os.fdopen(out_fd, "wb") as destination:
                    shutil.copyfileobj(source, destination, length=1024 * 1024)
                if target.stat().st_size != member.size:
                    raise SystemExit(f"Archive member size changed: {member.name}")
        if extracted != len(members):
            raise SystemExit("Archive changed between validation and extraction")
finally:
    os.close(snapshot_fd)

print(root)
PY
)

root="$tmp/extracted/$archive_root"

for required in \
  bin/netfyr-server \
  web/index.html \
  web/app.js \
  web/style.css \
  web/i18n.js \
  web/manifest.webmanifest \
  web/service-worker.js \
  web/refresh-coordinator.js \
  web/apple-touch-icon.png \
  web/favicon-32.png \
  web/favicon.ico \
  web/icon-192.png \
  web/icon-512.png \
  web/icon-maskable-512.png \
  config.example.toml \
  deploy/netfyr.service \
  deploy/install-package.sh \
  deploy/update-package.sh \
  deploy/netfyr-health.func \
  deploy/netfyr-release.func \
  LICENSE \
  README.md \
  VERSION; do
  [[ -f "$root/$required" ]] || { echo "Missing: $required" >&2; exit 1; }
done

[[ -x "$root/bin/netfyr-server" ]] || { echo "Server binary is not executable" >&2; exit 1; }
version=$(<"$root/VERSION")
[[ "$(basename "$root")" == "netfyr-server-v${version}-linux-"* ]] || {
  echo "Archive root and VERSION disagree" >&2
  exit 1
}
grep -qx 'GNU AFFERO GENERAL PUBLIC LICENSE' <(sed -n '1p' "$root/LICENSE")
grep -q "version = \"$version\"" "$repo_root/Cargo.toml"

printf 'Release archive OK: version=%s root=%s\n' "$version" "$(basename "$root")"
