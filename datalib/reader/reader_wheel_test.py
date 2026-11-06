"""The wheel make_reader_wheel.py writes, built around this host's library and
unpacked the way an installer does. What it cannot show, a real libc under a
real installer, is scripts/release/check_reader_wheels.sh's."""

from __future__ import annotations

import base64
import email.parser
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path

MAKE_WHEEL = Path(sys.argv[1]).resolve()
MODULE = Path(sys.argv[2]).resolve()
README = Path(sys.argv[3]).resolve()
LICENSE = Path(sys.argv[4]).resolve()
DOLTLITE_LICENSE = Path(sys.argv[5]).resolve()
DOLTLITE_APACHE = Path(sys.argv[6]).resolve()
LIBRARY = Path(sys.argv[7]).resolve()
COPTS = Path(sys.argv[8]).resolve()
CLI = Path(sys.argv[9]).resolve()
DOC = Path(sys.argv[10]).resolve()
STEP_DOC = Path(sys.argv[11]).resolve()

VERSION = "1.2.3"


def make_wheel(out_dir: Path, target: str = "manylinux", arch: str = "x86_64") -> Path:
    done = subprocess.run(
        [
            sys.executable,
            "-I",
            str(MAKE_WHEEL),
            f"--version={VERSION}",
            f"--target={target}",
            f"--arch={arch}",
            f"--module={MODULE}",
            f"--library={LIBRARY}",
            f"--readme={README}",
            f"--license=LICENSE={LICENSE}",
            f"--license=doltlite/LICENSE.md={DOLTLITE_LICENSE}",
            f"--license=doltlite/APACHE_LICENSE={DOLTLITE_APACHE}",
            f"--out-dir={out_dir}",
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    return Path(done.stdout.strip())


class TheDoc(unittest.TestCase):
    def test_carries_the_module_word_for_word(self) -> None:
        """doltlite.md hands the module out too, for a program that installs nothing."""
        blocks = re.findall(r"```python\n(.*?)```", DOC.read_text(), flags=re.DOTALL)
        found = [b for b in blocks if b.startswith("# datalib_reader.py")]
        self.assertEqual(found, [MODULE.read_text()])

    def test_the_step_example_names_the_module(self) -> None:
        self.assertIn("from datalib_reader import Store", STEP_DOC.read_text())


class TheFlags(unittest.TestCase):
    def test_are_the_engines_with_the_escaping_off(self) -> None:
        copts = COPTS.read_text().splitlines()
        self.assertIn("-DDOLTLITE_PROLLY=1", copts)
        self.assertIn("-O2", copts)
        version = [c for c in copts if c.startswith("-DDOLTLITE_VERSION=")]
        self.assertEqual(len(version), 1)
        self.assertRegex(version[0], r'^-DDOLTLITE_VERSION="\d+\.\d+\.\d+"$')


class TheWheel(unittest.TestCase):
    """One wheel for every test here: each takes seconds on a busy machine."""

    dist_info = f"imbue_datalib_reader-{VERSION}.dist-info"
    dir: Path
    wheel: Path
    zip: zipfile.ZipFile

    @classmethod
    def setUpClass(cls) -> None:
        tmp = tempfile.TemporaryDirectory()
        cls.addClassCleanup(tmp.cleanup)
        cls.dir = Path(tmp.name)
        cls.wheel = make_wheel(cls.dir)
        cls.zip = zipfile.ZipFile(cls.wheel)
        cls.addClassCleanup(cls.zip.close)

    def test_is_named_for_both_spellings_of_the_glibc_floor(self) -> None:
        self.assertEqual(
            self.wheel.name,
            f"imbue_datalib_reader-{VERSION}-py3-none-"
            "manylinux_2_17_x86_64.manylinux2014_x86_64.whl",
        )
        tags = self.zip.read(f"{self.dist_info}/WHEEL").decode()
        self.assertIn("Tag: py3-none-manylinux_2_17_x86_64\n", tags)
        self.assertIn("Tag: py3-none-manylinux2014_x86_64\n", tags)
        self.assertIn("Root-Is-Purelib: false\n", tags)

    def test_a_musl_wheel_says_musl(self) -> None:
        wheel = make_wheel(self.dir, target="musllinux", arch="aarch64")
        self.assertEqual(
            wheel.name,
            f"imbue_datalib_reader-{VERSION}-py3-none-musllinux_1_2_aarch64.whl",
        )

    def test_a_mac_wheel_says_the_oldest_macos_the_build_targets(self) -> None:
        """stage_reader_wheels.sh holds the library to the same 11.0."""
        wheel = make_wheel(self.dir, target="macos", arch="arm64")
        self.assertEqual(
            wheel.name,
            f"imbue_datalib_reader-{VERSION}-py3-none-macosx_11_0_arm64.whl",
        )

    def test_record_lists_every_file_with_its_hash_and_size(self) -> None:
        record_name = f"{self.dist_info}/RECORD"
        record = self.zip.read(record_name).decode().splitlines()
        listed = {}
        for line in record:
            name, digest, size = line.split(",")
            listed[name] = (digest, size)
        self.assertEqual(set(listed), set(self.zip.namelist()))
        self.assertEqual(listed.pop(record_name), ("", ""))
        for name, (digest, size) in listed.items():
            content = self.zip.read(name)
            want = base64.urlsafe_b64encode(hashlib.sha256(content).digest())
            self.assertEqual(digest, f"sha256={want.decode().rstrip('=')}", name)
            self.assertEqual(int(size), len(content), name)

    def test_metadata_names_the_package_and_its_licenses(self) -> None:
        raw = self.zip.read(f"{self.dist_info}/METADATA").decode()
        message = email.parser.Parser().parsestr(raw)
        self.assertEqual(message["Name"], "imbue-datalib-reader")
        self.assertEqual(message["Version"], VERSION)
        self.assertEqual(message["Requires-Python"], ">=3.9")
        self.assertIn("Apache-2.0", message["License-Expression"])
        files = message.get_all("License-File") or []
        self.assertEqual(
            files, ["LICENSE", "doltlite/LICENSE.md", "doltlite/APACHE_LICENSE"]
        )
        for name in files:
            self.assertIn(f"{self.dist_info}/licenses/{name}", self.zip.namelist())
        self.assertEqual(message.get_payload(), README.read_text())

    def test_the_same_inputs_give_the_same_bytes(self) -> None:
        again = self.dir / "again"
        again.mkdir()
        self.assertEqual(make_wheel(again).read_bytes(), self.wheel.read_bytes())

    def test_the_library_is_executable_and_the_rest_is_not(self) -> None:
        for info in self.zip.infolist():
            mode = (info.external_attr >> 16) & 0o777
            want = 0o755 if info.filename.endswith(LIBRARY.name) else 0o644
            self.assertEqual(mode, want, info.filename)


class Installed(unittest.TestCase):
    """Unpacked into a directory on the path, with nothing else to find."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        self.site = self.dir / "site"
        with zipfile.ZipFile(make_wheel(self.dir)) as wheel:
            wheel.extractall(self.site)
        self.store = self.dir / "entities.doltlite_db"
        subprocess.run(
            [
                str(CLI),
                str(self.store),
                (
                    "CREATE TABLE t(id TEXT PRIMARY KEY);"
                    "INSERT INTO t VALUES ('a');"
                    "SELECT dolt_commit('-A', '-m', 'added a');"
                ),
            ],
            check=True,
            capture_output=True,
        )

    def read(self, path: Path) -> subprocess.CompletedProcess[str]:
        code = (
            "import datalib_reader as r\n"
            "print(r._lib._name)\n"
            f"print(r.Store({str(self.store)!r}).query('SELECT id FROM t'))\n"
        )
        env = {
            "PATH": str(path),
            "PYTHONPATH": str(self.site),
            "TMPDIR": str(self.dir),
        }
        return subprocess.run(
            [sys.executable, "-c", code],
            check=False,
            capture_output=True,
            text=True,
            env=env,
        )

    def test_reads_with_the_library_it_carries(self) -> None:
        done = self.read(self.dir)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(
            done.stdout.splitlines(),
            [str(self.site / "datalib_reader" / LIBRARY.name), "[('a',)]"],
        )

    def test_its_own_library_wins_over_a_release_install(self) -> None:
        """A program pinned to a wheel gets that wheel's library whatever is on
        the PATH. The library beside this shell cannot load, so a lookup that
        reached it would fail."""
        tree = self.dir / "datalib"
        (tree / "lib").mkdir(parents=True)
        shutil.copy(CLI, tree / "datalib-doltlite")
        (tree / "lib" / LIBRARY.name).write_text("not a library")
        done = self.read(tree)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn(str(self.site), done.stdout.splitlines()[0])


if __name__ == "__main__":
    os.environ.pop("PYTHONPATH", None)
    unittest.main(argv=sys.argv[:1])
