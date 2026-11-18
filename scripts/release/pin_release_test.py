"""What the release procedure writes into install.sh and the Docker page, and what they hold now."""

from __future__ import annotations

import hashlib
import os
import re
import tempfile
import unittest
from pathlib import Path

from pin_release import (
    DEVCONTAINER_PINS,
    DOCKER_DOC,
    INSTALL,
    PINNED_DEVCONTAINER,
    PINNED_IMAGE,
    RELEASE_WORKFLOW,
    built_by_release_workflow,
    checked_image_digest,
    devcontainer_digest,
    pin_devcontainer,
    pin_docker_doc,
    pin_install,
    release_problems,
    tarball_hashes,
    unattested,
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

    def test_ci_names_its_image_by_digest_only(self) -> None:
        """A push to main runs in this image holding the write key, and a tag can be moved."""
        for rel, wanted in DEVCONTAINER_PINS.items():
            with self.subTest(rel):
                images = [
                    line
                    for line in read(rel).splitlines()
                    if line.strip().startswith("image:")
                    and "datalib_devcontainer" in line
                ]
                self.assertEqual(len(images), wanted)
                for line in images:
                    self.assertRegex(line, PINNED_DEVCONTAINER)
                    self.assertNotIn("latest", line)


class RewriteTest(unittest.TestCase):
    def test_pinning_ci_moves_every_mention_and_nothing_else(self) -> None:
        for rel, wanted in DEVCONTAINER_PINS.items():
            with self.subTest(rel):
                before = read(rel)
                after = pin_devcontainer(before, IMAGE, wanted)
                self.assertEqual(after.count(IMAGE), wanted)
                self.assertEqual(
                    PINNED_DEVCONTAINER.sub("", before),
                    PINNED_DEVCONTAINER.sub("", after),
                )

    def test_a_workflow_with_a_pin_missing_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            pin_devcontainer(
                "image: ghcr.io/imbue-ai/datalib_devcontainer:latest\n", IMAGE, 1
            )

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

    def test_a_release_needs_a_successful_run_on_its_tags_commit(self) -> None:
        """A writer can publish on a new tag while its run builds, and that run then fails."""
        tag, commit = "v9.9.9", "a" * 40
        good = {
            "path": RELEASE_WORKFLOW,
            "event": "push",
            "head_branch": tag,
            "head_sha": commit,
            "conclusion": "success",
        }
        self.assertTrue(built_by_release_workflow([good], tag, commit))
        self.assertFalse(built_by_release_workflow([], tag, commit))
        for name, different in {
            "failed, as when publish_release.py finds their release": {
                "conclusion": "failure"
            },
            "still running": {"conclusion": None},
            "a branch named like the tag, with its own workflow": {
                "head_sha": "b" * 40
            },
            "another tag's run": {"head_branch": "v9.9.8"},
            "another workflow": {"path": ".github/workflows/theirs.yml"},
            "not a push": {"event": "workflow_dispatch"},
        }.items():
            with self.subTest(name):
                run = good | different
                self.assertFalse(built_by_release_workflow([run], tag, commit))
                self.assertTrue(built_by_release_workflow([run, good], tag, commit))

    def test_ci_moves_only_to_the_image_tagged_for_this_release(self) -> None:
        other = "sha256:" + "d" * 64
        versions = [
            {"name": other, "metadata": {"container": {"tags": ["9.9.8"]}}},
            {"name": IMAGE, "metadata": {"container": {"tags": ["9.9.9", "latest"]}}},
            {"name": "sha256:" + "e" * 64},
        ]
        self.assertEqual(devcontainer_digest(versions, "v9.9.9"), IMAGE)
        with self.assertRaises(ValueError):
            devcontainer_digest(versions, "v9.9.7")
        with self.assertRaises(ValueError):
            devcontainer_digest(
                versions
                + [{"name": other, "metadata": {"container": {"tags": ["9.9.9"]}}}],
                "v9.9.9",
            )

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
        with self.assertRaises(ValueError):
            checked_image_digest({"assets": []}, contents)


class FakeGh:
    """Serves each tarball's bytes, and says yes to verifying only what release.yml attested."""

    def __init__(self, tag: str, commit: str) -> None:
        self.tag = tag
        self.commit = commit
        self.served = {
            f"datalib-{triple}.tar.gz": triple.encode() for triple in TARBALL_TRIPLES
        }
        self.attested = {hashlib.sha256(b).hexdigest() for b in self.served.values()}
        self.image_attested = True
        self.verified: list[str] = []

    def hashes(self) -> dict[str, str]:
        return {
            triple: hashlib.sha256(self.served[f"datalib-{triple}.tar.gz"]).hexdigest()
            for triple in TARBALL_TRIPLES
        }

    def __call__(self, *args: str) -> bool:
        if args[:2] == ("release", "download"):
            name = args[args.index("--pattern") + 1]
            if name not in self.served:
                return False
            directory = Path(args[args.index("--dir") + 1])
            (directory / name).write_bytes(self.served[name])
            return True
        assert args[:2] == ("attestation", "verify"), args
        subject, flags = args[2], args[3:]
        self.verified.append(subject)
        identity = (
            flags[flags.index("--signer-workflow") + 1]
            == f"imbue-ai/datalib/{RELEASE_WORKFLOW}"
            and flags[flags.index("--source-ref") + 1] == f"refs/tags/{self.tag}"
            and flags[flags.index("--source-digest") + 1] == self.commit
        )
        if subject.startswith("oci://"):
            return identity and self.image_attested
        digest = hashlib.sha256(Path(subject).read_bytes()).hexdigest()
        return identity and digest in self.attested


class AttestedTest(unittest.TestCase):
    TAG = "v9.9.9"
    COMMIT = "a" * 40

    def setUp(self) -> None:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.directory = Path(temp.name)
        self.gh = FakeGh(self.TAG, self.COMMIT)
        self.hashes = self.gh.hashes()

    def problems(self) -> list[str]:
        return unattested(
            self.gh, self.TAG, self.COMMIT, self.hashes, IMAGE, self.directory
        )

    def test_attested_tarballs_and_image_pin(self) -> None:
        self.assertEqual(self.problems(), [])
        self.assertEqual(len(self.gh.verified), len(TARBALL_TRIPLES) + 1)
        self.assertEqual(
            self.gh.verified[-1], f"oci://ghcr.io/imbue-ai/datalib@{IMAGE}"
        )

    def test_a_tarball_nobody_attested_is_refused(self) -> None:
        """A writer's own release on a tag, or one built by any other workflow."""
        name = f"datalib-{TARBALL_TRIPLES[0]}.tar.gz"
        self.gh.attested.discard(self.hashes[TARBALL_TRIPLES[0]])
        self.assertEqual(
            self.problems(),
            [f"{name}: no attestation from {RELEASE_WORKFLOW} at {self.TAG}"],
        )

    def test_an_attestation_for_another_tag_or_commit_is_refused(self) -> None:
        for field, value in {"tag": "v9.9.8", "commit": "b" * 40}.items():
            with self.subTest(field):
                gh = FakeGh(self.TAG, self.COMMIT)
                setattr(gh, field, value)
                self.gh = gh
                self.assertEqual(len(self.problems()), len(TARBALL_TRIPLES) + 1)

    def test_bytes_other_than_the_pinned_ones_are_never_verified(self) -> None:
        """Verifying another file would vouch for bytes install.sh does not name."""
        name = f"datalib-{TARBALL_TRIPLES[1]}.tar.gz"
        self.gh.served[name] = b"attested, but not what the release reports"
        self.gh.attested.add(hashlib.sha256(self.gh.served[name]).hexdigest())
        self.assertEqual(
            self.problems(),
            [f"{name}: what was downloaded is not what the release holds"],
        )
        self.assertNotIn(str(self.directory / name), self.gh.verified)

    def test_an_image_nobody_attested_is_refused(self) -> None:
        self.gh.image_attested = False
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn(IMAGE, problems[0])

    def test_a_tarball_that_will_not_download_is_refused(self) -> None:
        name = f"datalib-{TARBALL_TRIPLES[2]}.tar.gz"
        del self.gh.served[name]
        self.assertEqual(self.problems(), [f"{name}: could not be downloaded"])


if __name__ == "__main__":
    unittest.main()
