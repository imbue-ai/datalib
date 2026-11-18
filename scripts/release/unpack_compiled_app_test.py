"""The archive that reaches the job holding the signing key: what it may contain."""

from __future__ import annotations

import io
import stat
import tarfile
import tempfile
import unittest
from pathlib import Path

from unpack_compiled_app import unpack

GOOD = {
    "binaries/datalib-http": 0o755,
    "licenses/rust-crates.md": 0o644,
    "runtime/node/bin/node": 0o755,
    "target/release/datalib-tauri": 0o755,
}


def add_file(tar: tarfile.TarFile, name: str, mode: int = 0o644) -> None:
    info = tarfile.TarInfo(name)
    info.size = 2
    info.mode = mode
    tar.addfile(info, io.BytesIO(b"hi"))


def add_kind(tar: tarfile.TarFile, name: str, kind: bytes, link: str = "") -> None:
    info = tarfile.TarInfo(name)
    info.type = kind
    info.linkname = link
    tar.addfile(info)


class UnpackTest(unittest.TestCase):
    def setUp(self) -> None:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.dest = self.root / "checkout" / "datalib" / "tauri"
        self.dest.mkdir(parents=True)
        self.script = self.dest / "build-signed-app.sh"
        self.script.write_text("the real one")

    def run_with(self, add_extra=lambda tar: None, skip: str = "") -> list[str]:
        archive = self.root / "scratch" / "compiled.tar.gz"
        archive.parent.mkdir(exist_ok=True)
        with tarfile.open(archive, "w:gz") as tar:
            for name, mode in GOOD.items():
                if name != skip:
                    add_file(tar, name, mode)
            add_extra(tar)
        return unpack(archive, self.dest)

    def assert_refused(
        self, wanted: str, add_extra=lambda tar: None, skip: str = ""
    ) -> None:
        problems = self.run_with(add_extra, skip)
        self.assertTrue(any(wanted in p for p in problems), problems)
        self.assertEqual(
            sorted(p.name for p in self.dest.iterdir()), [self.script.name]
        )
        self.assertEqual(self.script.read_text(), "the real one")

    def test_the_expected_trees_land_in_place_with_their_modes(self) -> None:
        self.assertEqual(self.run_with(), [])
        for name, mode in GOOD.items():
            path = self.dest / name
            self.assertEqual(path.read_bytes(), b"hi")
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), mode, name)
        self.assertEqual(
            list((self.root / "scratch").iterdir()),
            [self.root / "scratch" / "compiled.tar.gz"],
        )

    def test_the_scripts_the_signing_job_runs_cannot_be_replaced(self) -> None:
        """The reported attack: an entry for each thing the next step runs."""
        for name in [
            "build-signed-app.sh",
            "node_modules/.bin/tauri",
            "sign-staged.sh",
        ]:
            with self.subTest(name=name):
                self.assert_refused(name, lambda tar, n=name: add_file(tar, n, 0o755))

    def test_a_path_out_of_a_tree_is_refused(self) -> None:
        for name in [
            "../build-signed-app.sh",
            "binaries/../build-signed-app.sh",
            "/tmp/evil",
            "./build-signed-app.sh",
            "binaries//x",
        ]:
            with self.subTest(name=name):
                self.assert_refused(name, lambda tar, n=name: add_file(tar, n))

    def test_anything_but_a_plain_file_or_directory_is_refused(self) -> None:
        for kind, link in [
            (tarfile.SYMTYPE, "../../build-signed-app.sh"),
            (tarfile.SYMTYPE, "node/bin/node"),
            (tarfile.LNKTYPE, "binaries/datalib-http"),
            (tarfile.FIFOTYPE, ""),
            (tarfile.CHRTYPE, ""),
        ]:
            with self.subTest(kind=kind, link=link):
                self.assert_refused(
                    "not a plain file or directory",
                    lambda tar, k=kind, l=link: add_kind(tar, "binaries/x", k, l),
                )

    def test_the_shell_binary_must_be_a_plain_file(self) -> None:
        self.assert_refused(
            "not a plain file or directory",
            lambda tar: add_kind(
                tar, "target/release/datalib-tauri", tarfile.SYMTYPE, "/bin/sh"
            ),
            skip="target/release/datalib-tauri",
        )

    def test_nothing_else_under_target_is_taken(self) -> None:
        self.assert_refused(
            "target/release/bundle/evil",
            lambda tar: add_file(tar, "target/release/bundle/evil"),
        )

    def test_a_name_twice_is_refused(self) -> None:
        self.assert_refused(
            "appears twice", lambda tar: add_file(tar, "binaries/datalib-http")
        )

    def test_a_missing_tree_is_refused(self) -> None:
        self.assert_refused("runtime: missing", skip="runtime/node/bin/node")
        self.assert_refused(
            "target/release/datalib-tauri: missing",
            skip="target/release/datalib-tauri",
        )

    def test_setuid_and_group_write_are_dropped(self) -> None:
        extra = lambda tar: add_file(tar, "binaries/setuid", 0o6777)
        self.assertEqual(self.run_with(extra), [])
        mode = stat.S_IMODE((self.dest / "binaries/setuid").stat().st_mode)
        self.assertEqual(mode, 0o755)

    def test_a_tree_already_in_the_destination_is_refused(self) -> None:
        (self.dest / "runtime").mkdir()
        problems = self.run_with()
        self.assertTrue(any("runtime: already in" in p for p in problems), problems)
        self.assertFalse((self.dest / "binaries").exists())


if __name__ == "__main__":
    unittest.main()
