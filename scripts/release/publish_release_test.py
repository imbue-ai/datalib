"""release.yml's last job against a stand-in for GitHub's release API.

The stand-in's answers were read from the real API (a draft's assets carry
`digest` and `state`, measured on v0.51.0's draft). Whether the real API takes
these calls is only shown by a tag's run.
"""

from __future__ import annotations

import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Callable
from pathlib import Path
from typing import Any

import publish_release
from publish_release import (
    DMG,
    IMAGE_DIGEST,
    Refused,
    attestation_checks,
    content_problems,
    expected_artifacts,
    publish,
    read_artifacts,
    sha256_of,
    subjects_problems,
    subjects_text,
)

REPO = "imbue-ai/datalib"
TAG = "v9.9.9"


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class FakeGitHub:
    """Holds releases as GitHub does and answers the calls the script makes."""

    def __init__(self) -> None:
        self.releases: dict[int, dict[str, Any]] = {}
        self.next_id = 1
        self.upload_failures = 0
        self.immutable_setting = True
        self.on_publish: Callable[[dict[str, Any]], None] = lambda release: None
        self.calls: list[tuple[str, ...]] = []

    def add(self, tag: str, draft: bool, assets: dict[str, bytes] | None = None) -> int:
        release_id = self.next_id
        self.next_id += 1
        self.releases[release_id] = {
            "id": release_id,
            "tag_name": tag,
            "draft": draft,
            "immutable": False,
            "assets": [self.asset(n, b) for n, b in (assets or {}).items()],
        }
        return release_id

    @staticmethod
    def asset(name: str, data: bytes) -> dict[str, Any]:
        return {"name": name, "digest": f"sha256:{sha(data)}", "state": "uploaded"}

    def __call__(self, *args: str) -> str:
        self.calls.append(args)
        if args[:2] == ("release", "upload"):
            return self.upload(args)
        if args[:2] == ("api", "--paginate"):
            return "\n".join(
                json.dumps({k: r[k] for k in ("id", "tag_name", "draft")})
                for r in self.releases.values()
            )
        if args[:3] == ("api", "-X", "POST"):
            return json.dumps(self.releases[self.add(TAG, True)])
        if args[:3] == ("api", "-X", "DELETE"):
            del self.releases[int(args[3].rsplit("/", 1)[1])]
            return ""
        if args[:3] == ("api", "-X", "PATCH"):
            release = self.releases[int(args[3].rsplit("/", 1)[1])]
            self.on_publish(release)
            release["draft"] = False
            release["immutable"] = self.immutable_setting
            return json.dumps(release)
        if args[1].endswith("/latest"):
            published = [r for r in self.releases.values() if not r["draft"]]
            return json.dumps(published[-1])
        return json.dumps(self.releases[int(args[1].rsplit("/", 1)[1])])

    def upload(self, args: tuple[str, ...]) -> str:
        if self.upload_failures:
            self.upload_failures -= 1
            raise subprocess.CalledProcessError(1, "gh")
        target = next(
            r for r in self.releases.values() if r["tag_name"] == args[2] and r["draft"]
        )
        for path in map(Path, args[4:]):
            target["assets"] = [a for a in target["assets"] if a["name"] != path.name]
            target["assets"].append(self.asset(path.name, path.read_bytes()))
        return ""


class ArtifactsCase(unittest.TestCase):
    def setUp(self) -> None:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        for artifact, files in expected_artifacts().items():
            (self.root / artifact).mkdir()
            for file in files:
                if file.endswith(".sha256"):
                    continue
                data = (
                    f"sha256:{'a' * 64}\n".encode()
                    if file == IMAGE_DIGEST
                    else file.encode()
                )
                (self.root / artifact / file).write_bytes(data)
                if f"{file}.sha256" in files:
                    (self.root / artifact / f"{file}.sha256").write_text(
                        f"{sha(data)}  {file}\n"
                    )
        self.dmg_sha256 = sha(DMG.encode())

    def problems(self) -> list[str]:
        assets, problems = read_artifacts(self.root)
        if problems:
            return problems
        hashes = {name: sha256_of(path) for name, path in assets.items()}
        return content_problems(assets, hashes, self.dmg_sha256)


class WhatIsTakenTest(ArtifactsCase):
    def test_the_expected_artifacts_pass(self) -> None:
        self.assertEqual(self.problems(), [])

    def test_every_asset_a_release_shipped_is_expected(self) -> None:
        """v0.50.0's twenty assets, read from GitHub, plus the image digest."""
        shipped = {
            f"{stem}{suffix}"
            for stem in [
                "Datalib-aarch64-apple-darwin.dmg",
                "datalib-aarch64-apple-darwin.tar.gz",
                "datalib-aarch64-unknown-linux-gnu.tar.gz",
                "datalib-aarch64-unknown-linux-musl.tar.gz",
                "datalib-x86_64-unknown-linux-gnu.tar.gz",
                "datalib-x86_64-unknown-linux-musl.tar.gz",
                "runtime-aarch64-apple-darwin.tar.gz",
                "runtime-aarch64-unknown-linux-gnu.tar.gz",
                "runtime-x86_64-unknown-linux-gnu-cuda.tar.gz",
                "runtime-x86_64-unknown-linux-gnu.tar.gz",
            ]
            for suffix in ["", ".sha256"]
        }
        expected = {f for files in expected_artifacts().values() for f in files}
        self.assertEqual(expected, shipped | {IMAGE_DIGEST})

    def test_a_build_cannot_carry_the_dmg_in_its_own_artifact(self) -> None:
        """A file named like another job's would otherwise replace it on the release."""
        (self.root / "tarball-x86_64-unknown-linux-gnu" / DMG).write_bytes(b"evil")
        self.assertEqual(
            self.problems(),
            [f"artifact tarball-x86_64-unknown-linux-gnu: {DMG} unexpected"],
        )

    def test_an_artifact_nothing_named_is_refused(self) -> None:
        (self.root / "tarball-evil").mkdir()
        self.assertEqual(self.problems(), ["artifact tarball-evil: unexpected"])

    def test_a_missing_artifact_or_file_is_refused(self) -> None:
        name = "datalib-aarch64-apple-darwin.tar.gz"
        (self.root / "tarball-aarch64-apple-darwin" / name).unlink()
        self.assertEqual(
            self.problems(), [f"artifact tarball-aarch64-apple-darwin: {name} missing"]
        )

    def test_a_link_in_place_of_an_asset_is_refused(self) -> None:
        path = self.root / "macos-app-dmg" / DMG
        path.unlink()
        path.symlink_to("/etc/passwd")
        self.assertEqual(self.problems(), [f"{DMG}: not a plain file"])

    def test_a_sidecar_that_disagrees_is_refused(self) -> None:
        name = "runtime-aarch64-apple-darwin.tar.gz.sha256"
        (self.root / "runtime-aarch64-apple-darwin" / name).write_text(
            "0" * 64 + "  x\n"
        )
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn(name, problems[0])

    def test_a_dmg_other_than_the_verified_one_is_refused(self) -> None:
        """An artifact can be replaced after macos-app-verify read it."""
        self.dmg_sha256 = "f" * 64
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("is not the dmg macos-app-verify checked", problems[0])

    def test_a_bad_image_digest_is_refused(self) -> None:
        (self.root / "docker-image-digest" / IMAGE_DIGEST).write_text("latest\n")
        self.assertEqual(self.problems(), [f"{IMAGE_DIGEST}: not one image digest"])


class AttestTest(ArtifactsCase):
    def setUp(self) -> None:
        super().setUp()
        self.assets, problems = read_artifacts(self.root)
        self.assertEqual(problems, [])
        self.hashes = {n: sha256_of(p) for n, p in self.assets.items()}

    def signed(self) -> str:
        return sha(subjects_text(self.assets, self.hashes).encode())

    def run_script(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, "-I", publish_release.__file__, *args],
            check=False,
            capture_output=True,
            text=True,
            env={},
        )

    def test_every_asset_but_the_sidecars_is_signed_once(self) -> None:
        text = subjects_text(self.assets, self.hashes)
        names = [line.split("  ")[1] for line in text.splitlines()]
        self.assertEqual(
            names, sorted(n for n in self.assets if not n.endswith(".sha256"))
        )
        self.assertIn(DMG, names)
        self.assertIn(IMAGE_DIGEST, names)

    def test_the_subjects_are_in_shasums_format(self) -> None:
        """The attest action reads `<hex>  <name>`: a space, then the text-mode flag."""
        for line in subjects_text(self.assets, self.hashes).splitlines():
            digest, name = line.split("  ")
            self.assertEqual(digest, sha256_of(self.assets[name]))

    def test_what_was_signed_publishes(self) -> None:
        self.assertEqual(subjects_problems(self.assets, self.hashes, self.signed()), [])

    def test_an_asset_replaced_after_attest_is_refused(self) -> None:
        """An artifact can be uploaded again after `attest` read it; a job output cannot."""
        signed = self.signed()
        name = "datalib-x86_64-unknown-linux-gnu.tar.gz"
        self.assets[name].write_bytes(b"evil")
        Path(f"{self.assets[name]}.sha256").write_text(f"{sha(b'evil')}  {name}\n")
        hashes = {n: sha256_of(p) for n, p in self.assets.items()}
        self.assertEqual(content_problems(self.assets, hashes, self.dmg_sha256), [])
        problems = subjects_problems(self.assets, hashes, signed)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("not the ones the attest job signed", problems[0])

    def test_each_check_names_this_workflow_tag_and_commit(self) -> None:
        env = {
            "GITHUB_REPOSITORY": REPO,
            "GITHUB_REF": f"refs/tags/{TAG}",
            "GITHUB_SHA": "c" * 40,
        }
        checks = attestation_checks(self.assets, "ghcr.io/imbue-ai/datalib", env)
        files = [
            str(self.assets[n])
            for n in sorted(self.assets)
            if not n.endswith(".sha256")
        ]
        image = f"oci://ghcr.io/imbue-ai/datalib@sha256:{'a' * 64}"
        self.assertEqual([check[2] for check in checks], [*files, image, image])
        self.assertIn("--bundle-from-oci", checks[-1])
        for check in checks:
            with self.subTest(subject=check[2]):
                self.assertEqual(check[:2], ["attestation", "verify"])
                flags = check[3:]
                for flag, value in [
                    ("--repo", REPO),
                    ("--signer-workflow", f"{REPO}/.github/workflows/release.yml"),
                    ("--source-ref", f"refs/tags/{TAG}"),
                    ("--source-digest", "c" * 40),
                ]:
                    self.assertEqual(flags[flags.index(flag) + 1], value)
                self.assertIn("--deny-self-hosted-runners", flags)

    def test_the_command_line_prints_the_subjects_and_refuses_other_bytes(self) -> None:
        """What the two jobs run; publish stops before it reaches GitHub."""
        printed = self.run_script("subjects", str(self.root), self.dmg_sha256)
        self.assertEqual(printed.returncode, 0, printed.stderr)
        self.assertEqual(printed.stdout, subjects_text(self.assets, self.hashes))
        refused = self.run_script(
            "publish", TAG, str(self.root), self.dmg_sha256, "0" * 64
        )
        self.assertEqual(refused.returncode, 1)
        self.assertIn("not the ones the attest job signed", refused.stderr)

    def test_the_command_line_refuses_a_bad_dmg_hash(self) -> None:
        refused = self.run_script("subjects", str(self.root), "latest")
        self.assertEqual(refused.returncode, 1)
        self.assertIn("is not a sha256", refused.stderr)


class PublishTest(ArtifactsCase):
    def setUp(self) -> None:
        super().setUp()
        self.assets, problems = read_artifacts(self.root)
        self.assertEqual(problems, [])
        self.wanted = {n: sha256_of(p) for n, p in self.assets.items()}
        self.github = FakeGitHub()
        self.waits: list[float] = []

    def run_publish(self) -> None:
        publish(self.github, REPO, TAG, self.assets, self.wanted, self.waits.append)

    def on_tag(self) -> list[dict[str, Any]]:
        return [r for r in self.github.releases.values() if r["tag_name"] == TAG]

    def test_it_publishes_whole_immutable_and_latest(self) -> None:
        self.run_publish()
        (release,) = self.on_tag()
        self.assertFalse(release["draft"])
        self.assertTrue(release["immutable"])
        self.assertEqual({a["name"] for a in release["assets"]}, set(self.wanted))
        patch = next(c for c in self.github.calls if c[:3] == ("api", "-X", "PATCH"))
        self.assertIn("make_latest=true", patch)

    def test_a_draft_already_on_the_tag_is_removed_with_what_it_held(self) -> None:
        """A failed attempt's, or a planted one with an extra asset that publishing would lock."""
        planted = self.github.add(TAG, True, {"evil": b"evil"})
        self.run_publish()
        self.assertNotIn(planted, self.github.releases)
        (release,) = self.on_tag()
        self.assertNotIn("evil", {a["name"] for a in release["assets"]})

    def test_another_tags_draft_is_left_alone(self) -> None:
        other = self.github.add("v1.0.0", True)
        self.run_publish()
        self.assertIn(other, self.github.releases)

    def test_a_release_already_published_on_the_tag_is_refused(self) -> None:
        self.github.add(TAG, False, {"evil": b"evil"})
        with self.assertRaisesRegex(Refused, "already has a published release"):
            self.run_publish()
        self.assertEqual(len(self.on_tag()), 1)

    def test_a_failed_upload_is_tried_once_more(self) -> None:
        self.github.upload_failures = 1
        self.run_publish()
        self.assertEqual(self.waits, [30])

    def test_a_second_failed_upload_publishes_nothing(self) -> None:
        self.github.upload_failures = 2
        with self.assertRaises(subprocess.CalledProcessError):
            self.run_publish()
        self.assertTrue(self.on_tag()[0]["draft"])

    def test_an_asset_swapped_before_publishing_stops_it(self) -> None:
        real_upload = self.github.upload

        def upload_then_swap(args: tuple[str, ...]) -> str:
            result = real_upload(args)
            self.on_tag()[0]["assets"][0]["digest"] = f"sha256:{'0' * 64}"
            return result

        self.github.upload = upload_then_swap  # type: ignore[method-assign]
        with self.assertRaisesRegex(Refused, "the draft is not what this run made"):
            self.run_publish()
        self.assertTrue(self.on_tag()[0]["draft"])

    def test_an_asset_added_before_publishing_stops_it(self) -> None:
        real_upload = self.github.upload

        def upload_then_add(args: tuple[str, ...]) -> str:
            result = real_upload(args)
            self.on_tag()[0]["assets"].append(FakeGitHub.asset("evil", b"evil"))
            return result

        self.github.upload = upload_then_add  # type: ignore[method-assign]
        with self.assertRaisesRegex(Refused, "evil: on the release"):
            self.run_publish()

    def test_a_swap_in_the_moment_before_publishing_is_reported(self) -> None:
        def swap(release: dict[str, Any]) -> None:
            release["assets"][0]["digest"] = f"sha256:{'0' * 64}"

        self.github.on_publish = swap
        with self.assertRaisesRegex(Refused, "IS PUBLISHED and is not as it should be"):
            self.run_publish()

    def test_a_release_that_did_not_lock_is_reported(self) -> None:
        """The setting lives in GitHub, where nothing else would notice it went off."""
        self.github.immutable_setting = False
        with self.assertRaisesRegex(Refused, "not immutable"):
            self.run_publish()
        self.assertEqual(self.waits, [15, 15, 15, 15])


if __name__ == "__main__":
    unittest.main()
