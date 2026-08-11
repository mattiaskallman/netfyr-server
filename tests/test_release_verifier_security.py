#!/usr/bin/env python3
"""Dynamiska säkerhetsregressioner för releaseverifieraren."""

import io
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
VERIFY = ROOT / "scripts" / "verify-release.sh"
ARCHIVE_ROOT = "netfyr-server-v1.2.2-linux-amd64"


class ReleaseVerifierSecurityTests(unittest.TestCase):
    def run_verify(self, archive: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(VERIFY), str(archive)],
            cwd=ROOT,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=10,
            check=False,
        )

    @staticmethod
    def add(tf: tarfile.TarFile, name: str, data: bytes = b"x", kind: str = "file", size=None):
        member = tarfile.TarInfo(name)
        if kind == "dir":
            member.type = tarfile.DIRTYPE
            member.size = 0
            tf.addfile(member)
        elif kind == "symlink":
            member.type = tarfile.SYMTYPE
            member.linkname = "/etc/passwd"
            member.size = 0
            tf.addfile(member)
        else:
            member.size = len(data) if size is None else size
            if size is None:
                source = io.BytesIO(data)
            else:
                class ZeroReader:
                    def __init__(self, remaining):
                        self.remaining = remaining

                    def read(self, amount=-1):
                        if self.remaining == 0:
                            return b""
                        amount = self.remaining if amount < 0 else min(amount, self.remaining)
                        self.remaining -= amount
                        return b"\0" * amount

                source = ZeroReader(size)
            tf.addfile(member, source)

    def test_traversal_symlink_duplicate_and_oversize_are_rejected(self):
        cases = {}
        with tempfile.TemporaryDirectory() as td:
            td = Path(td)
            for name in ("traversal", "symlink", "duplicate", "oversize"):
                archive = td / f"{name}.tar.gz"
                with tarfile.open(archive, "w:gz") as tf:
                    self.add(tf, ARCHIVE_ROOT, kind="dir")
                    if name == "traversal":
                        self.add(tf, f"{ARCHIVE_ROOT}/../../escape")
                    elif name == "symlink":
                        self.add(tf, f"{ARCHIVE_ROOT}/link", kind="symlink")
                    elif name == "duplicate":
                        self.add(tf, f"{ARCHIVE_ROOT}/file", b"a")
                        self.add(tf, f"{ARCHIVE_ROOT}/file", b"b")
                    else:
                        self.add(tf, f"{ARCHIVE_ROOT}/huge", size=256 * 1024 * 1024 + 1)
                result = self.run_verify(archive)
                cases[name] = (result.returncode, result.stdout)
            for name, (code, output) in cases.items():
                self.assertNotEqual(code, 0, f"{name} accepterades: {output}")
            self.assertFalse((Path("/tmp") / "escape").exists())

    def test_member_limit_is_enforced_during_iteration(self):
        with tempfile.TemporaryDirectory() as td:
            archive = Path(td) / "too-many.tar.gz"
            with tarfile.open(archive, "w:gz") as tf:
                self.add(tf, ARCHIVE_ROOT, kind="dir")
                for index in range(10_000):
                    self.add(tf, f"{ARCHIVE_ROOT}/d{index}", kind="dir")
            result = self.run_verify(archive)
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("too many members", result.stdout.lower())

    def test_archive_symlink_is_rejected_before_snapshot(self):
        with tempfile.TemporaryDirectory() as td:
            td = Path(td)
            real = td / "real.tar.gz"
            with tarfile.open(real, "w:gz") as tf:
                self.add(tf, ARCHIVE_ROOT, kind="dir")
            link = td / "link.tar.gz"
            link.symlink_to(real)
            result = self.run_verify(link)
            self.assertNotEqual(result.returncode, 0, result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
