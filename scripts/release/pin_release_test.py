"""What the release procedure writes into install.sh and the Docker page, and what they hold now."""

from __future__ import annotations

import hashlib
import os
import re
import unittest
from pathlib import Path

from pin_release import (
    DOCKER_DOC,
    INSTALL,
    PINNED_IMAGE,
    checked_image_digest,
    pin_docker_doc,
    pin_install,
    release_problems,
    tarball_hashes,
)
from publish_release import IMAGE_DIGEST, TARBALL_TRIPLES

WORKSPACE = Path(os.environ["TEST_SRCDIR"]) / os.environ["TEST_WORKSPACE"]
HASHES = {triple: f"{index:064x}" for index, triple in enumerate(TARBALL_TRIPLES)}
IMAGE = "sha256:" + "c" * 64


def read(path: str) -> str:
    return (WORKSPACE / path).read_text()


class TheTreeTest(unittest.TestCase):
    def test_install_sh_pins_a_version_and_a_hash_for_every_tarball(self) -> None:
        text = read(INSTALL)
        self.assertRegex(text, r'(?m)^PINNED_VERSION="v[0-9]+\.[0-9]+\.[0-9]+"$')
        for triple in TARBALL_TRIPLES:
            self.assertRegex(text, rf"(?m)^\s*{triple}\) echo [0-9a-f]{{64}} ;;$")

    def test_install_sh_never_names_a_moving_download(self) -> None:
        code = [
            line
            for line in read(INSTALL).splitlines()
            if not line.lstrip().startswith("#")
        ]
        self.assertEqual([line for line in code if "releases/latest" in line], [])

    def test_the_docker_page_names_the_image_by_digest_only(self) -> None:
        """A tag can be moved by anyone who can run a workflow here; a digest cannot."""
        text = read(DOCKER_DOC)
        self.assertEqual(len(PINNED_IMAGE.findall(text)), 2)
        self.assertNotIn("ghcr.io/imbue-ai/datalib:", text)
        self.assertIsNone(re.search(r"ghcr\.io/imbue-ai/datalib\s", text))


class RewriteTest(unittest.TestCase):
    def test_pinning_install_sh_changes_only_the_pins(self) -> None:
        before = read(INSTALL)
        after = pin_install(before, "v9.9.9", HASHES)
        self.assertIn('PINNED_VERSION="v9.9.9"', after)
        for triple, value in HASHES.items():
            self.assertRegex(after, rf"(?m)^\s*{triple}\) echo {value} ;;$")
        changed = [
            (old, new)
            for old, new in zip(before.splitlines(), after.splitlines())
            if old != new
        ]
        self.assertLessEqual(len(changed), 1 + len(TARBALL_TRIPLES))
        self.assertEqual(len(before.splitlines()), len(after.splitlines()))

    def test_a_script_the_pins_cannot_be_found_in_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            pin_install("#!/bin/sh\n", "v9.9.9", HASHES)

    def test_pinning_the_docker_page_moves_both_mentions(self) -> None:
        after = pin_docker_doc(read(DOCKER_DOC), IMAGE)
        self.assertEqual(after.count(f"ghcr.io/imbue-ai/datalib@{IMAGE}"), 2)

    def test_a_page_missing_a_mention_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            pin_docker_doc("no image here\n", IMAGE)


class WhichReleasesTest(unittest.TestCase):
    def test_only_a_published_immutable_release_can_be_pinned(self) -> None:
        self.assertEqual(release_problems({"draft": False, "immutable": True}), [])
        self.assertEqual(len(release_problems({"draft": True, "immutable": True})), 1)
        self.assertEqual(len(release_problems({"draft": False, "immutable": False})), 1)
        self.assertEqual(len(release_problems({})), 1)

    def test_a_tarball_without_a_digest_is_refused(self) -> None:
        assets = [
            {"name": f"datalib-{t}.tar.gz", "digest": f"sha256:{HASHES[t]}"}
            for t in TARBALL_TRIPLES
        ]
        self.assertEqual(tarball_hashes({"assets": assets}), HASHES)
        assets[0]["digest"] = ""
        with self.assertRaises(ValueError):
            tarball_hashes({"assets": assets})

    def test_the_image_digest_must_be_the_releases_own_asset(self) -> None:
        contents = f"{IMAGE}\n".encode()
        good = f"sha256:{hashlib.sha256(contents).hexdigest()}"
        release = {"assets": [{"name": IMAGE_DIGEST, "digest": good}]}
        self.assertEqual(checked_image_digest(release, contents), IMAGE)
        with self.assertRaises(ValueError):
            checked_image_digest(release, b"sha256:" + b"d" * 64 + b"\n")


if __name__ == "__main__":
    unittest.main()
