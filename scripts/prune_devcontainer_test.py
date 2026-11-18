"""What the devcontainer prune deletes, and what it must never delete."""

from __future__ import annotations

import os
import unittest
from datetime import UTC, datetime, timedelta
from pathlib import Path

from prune_devcontainer import (
    GRACE,
    KEEP_BUILDS,
    Version,
    pinned_digests,
    plan,
    version_of,
)

NOW = datetime(2026, 11, 18, 10, 30, tzinfo=UTC)


def digest(name: str) -> str:
    return "sha256:" + name.encode().hex().ljust(64, "0")[:64]


class Registry:
    """Versions as GHCR lists them: a push is a tagged index and its two
    untagged manifests, the image and its provenance."""

    def __init__(self) -> None:
        self.versions: list[Version] = []
        self.children: dict[str, list[str]] = {}

    def push(self, name: str, tags: tuple[str, ...], age: timedelta) -> str:
        at = NOW - age
        index = digest(name)
        kids = [digest(f"{name}/image"), digest(f"{name}/provenance")]
        for kid in kids:
            self.versions.append(Version(len(self.versions) + 1, kid, at, ()))
        self.versions.append(Version(len(self.versions) + 1, index, at, tags))
        self.children[index] = kids
        return index

    def deleted(self, pinned: set[str]) -> set[str]:
        return {v.digest for v in plan(self.versions, pinned, self.children, NOW)}


class PlanTest(unittest.TestCase):
    def test_a_pinned_release_image_survives_two_newer_pushes(self) -> None:
        """The v0.55.0 tag's devcontainer run deleted v0.54.1's image, which
        test.yml pinned: the old prune kept the newest six *versions*, and
        two pushes are six."""
        r = Registry()
        builds = [
            r.push(f"build-{i}", (f"sha-{i:012x}",), timedelta(days=20 + i))
            for i in range(8)
        ]
        pinned = r.push("v0.54.1", ("sha-b6a9e11bcddc", "0.54.1"), timedelta(days=5))
        r.push("v0.55.0-first", ("sha-1dd82603a7c2",), timedelta(minutes=50))
        newest = r.push(
            "v0.55.0", ("sha-645e6e14f4e5", "0.55.0", "latest"), timedelta(minutes=1)
        )

        gone = r.deleted({pinned})

        self.assertNotIn(pinned, gone)
        self.assertFalse(set(r.children[pinned]) & gone, "the pinned image's manifests")
        self.assertNotIn(newest, gone)
        self.assertFalse(set(r.children[newest]) & gone)
        # Eight old builds and the first v0.55.0 push are nine `sha-` builds;
        # the oldest three go, with their manifests.
        oldest = builds[-3:]
        for index in oldest:
            self.assertIn(index, gone)
            self.assertTrue(set(r.children[index]) <= gone)
        self.assertEqual(len(gone), 3 * 3)

    def test_a_release_tag_is_kept_unpinned_and_however_old(self) -> None:
        r = Registry()
        release = r.push("v0.40.0", ("sha-aaaaaaaaaaaa", "0.40.0"), timedelta(days=400))
        for i in range(KEEP_BUILDS + 2):
            r.push(f"build-{i}", (f"sha-{i:012x}",), timedelta(days=i))
        gone = r.deleted(set())
        self.assertNotIn(release, gone)
        self.assertFalse(set(r.children[release]) & gone)

    def test_a_pinned_build_is_kept_past_the_newest_few(self) -> None:
        r = Registry()
        pinned = r.push("old", ("sha-0123456789ab",), timedelta(days=90))
        for i in range(KEEP_BUILDS + 1):
            r.push(f"build-{i}", (f"sha-{i:012x}",), timedelta(days=i))
        self.assertNotIn(pinned, r.deleted({pinned}))
        self.assertIn(pinned, r.deleted(set()))

    def test_untagged_versions_of_a_push_in_flight_are_left_alone(self) -> None:
        r = Registry()
        young = Version(999, digest("in-flight"), NOW - GRACE / 2, ())
        stale = Version(998, digest("orphan"), NOW - GRACE * 2, ())
        r.versions += [young, stale]
        gone = r.deleted(set())
        self.assertNotIn(young.digest, gone)
        self.assertIn(stale.digest, gone)

    def test_a_ghcr_entry_reads_as_a_version(self) -> None:
        v = version_of(
            {
                "id": 7,
                "name": digest("x"),
                "created_at": "2026-11-18T10:30:20Z",
                "metadata": {"container": {"tags": ["0.55.0", "latest"]}},
            }
        )
        self.assertEqual(v.tags, ("0.55.0", "latest"))
        self.assertEqual(v.created_at, datetime(2026, 11, 18, 10, 30, 20, tzinfo=UTC))
        self.assertEqual(
            version_of(
                {
                    "id": 8,
                    "name": "n",
                    "created_at": "2026-11-18T10:30:20Z",
                    "metadata": {},
                }
            ).tags,
            (),
        )


class PinnedTest(unittest.TestCase):
    def test_the_workflows_pins_are_found(self) -> None:
        """Read from the checked-in workflows, so a pin written in a new shape
        fails here rather than going unprotected."""
        root = Path(os.environ.get("TEST_SRCDIR", "")) / "_main"
        if not (root / ".github").is_dir():
            root = Path(__file__).resolve().parents[1]
        texts = [
            (root / ".github" / "workflows" / name).read_text()
            for name in ("test.yml", "cargo-update.yml")
        ]
        found = [pinned_digests([t]) for t in texts]
        for name, digests in zip(("test.yml", "cargo-update.yml"), found):
            self.assertEqual(len(digests), 1, f"{name}: {digests}")
        self.assertEqual(found[0], found[1], "both name the same image")


if __name__ == "__main__":
    unittest.main()
