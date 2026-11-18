#!/usr/bin/env python3
"""Delete old devcontainer images from GHCR, and never one a workflow names.

Run by devcontainer.yml after it pushes. GHCR keeps one "version" per
manifest, and a push makes three: the tagged image index, and as untagged
versions its amd64 image and its build provenance. Kept:

- every version tagged with a release (`X.Y.Z`) or `latest`;
- every version whose digest a workflow names (`test.yml`, `cargo-update.yml`
  pin CI's image by digest);
- the newest `KEEP_BUILDS` versions tagged only `sha-…`;
- every manifest a kept index lists, so a kept image can still be pulled;
- any untagged version younger than `GRACE`, which may belong to a push still
  in flight on another ref.

Everything else goes: older `sha-` builds, and the untagged manifests no kept
index lists. Deciding is `plan`, a pure function; the rest asks GHCR and the
registry and deletes. A kept index whose manifest cannot be read stops the run
before anything is deleted.

    python3 scripts/prune_devcontainer.py [--dry-run]

Needs `gh` signed in with `packages: write` on the org's package, and `docker`
logged in to ghcr.io.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path

ORG = "imbue-ai"
PACKAGE = "datalib_devcontainer"
IMAGE = f"ghcr.io/{ORG}/{PACKAGE}"
KEEP_BUILDS = 6
GRACE = timedelta(hours=6)
RELEASE_TAG = re.compile(r"^(latest|[0-9]+\.[0-9]+\.[0-9]+)$")
PINNED = re.compile(rf"{PACKAGE}[^\n]*?@(sha256:[0-9a-f]{{64}})")
WORKFLOWS = Path(__file__).resolve().parents[1] / ".github" / "workflows"


@dataclass(frozen=True)
class Version:
    id: int
    digest: str
    created_at: datetime
    tags: tuple[str, ...]


def version_of(entry: dict) -> Version:
    """One entry of GHCR's package versions API."""
    tags = entry.get("metadata", {}).get("container", {}).get("tags", [])
    return Version(
        id=entry["id"],
        digest=entry["name"],
        created_at=datetime.fromisoformat(entry["created_at"]),
        tags=tuple(tags),
    )


def pinned_digests(workflow_texts: list[str]) -> set[str]:
    return {d for text in workflow_texts for d in PINNED.findall(text)}


def plan(
    versions: list[Version],
    pinned: set[str],
    children: dict[str, list[str]],
    now: datetime,
) -> list[Version]:
    """The versions to delete. `children` maps an index's digest to the
    manifests it lists; it must hold every tagged version and every pinned
    one present."""
    tagged = [v for v in versions if v.tags]
    builds = sorted(
        (
            v
            for v in tagged
            if not any(RELEASE_TAG.match(t) for t in v.tags) and v.digest not in pinned
        ),
        key=lambda v: (v.created_at, v.id),
        reverse=True,
    )
    old_builds = {v.id for v in builds[KEEP_BUILDS:]}
    kept_tops = {v.digest for v in tagged if v.id not in old_builds}
    kept_tops |= pinned & {v.digest for v in versions}
    kept = kept_tops | pinned
    for digest in kept_tops:
        kept.update(children[digest])
    return [
        v
        for v in versions
        if v.digest not in kept and (v.tags or now - v.created_at > GRACE)
    ]


def gh_json(path: str) -> list | dict:
    out = subprocess.run(
        ["gh", "api", "-H", "Accept: application/vnd.github+json", path],
        check=True,
        stdout=subprocess.PIPE,
    ).stdout
    return json.loads(out)


def all_versions() -> list[Version]:
    versions: list[Version] = []
    page = 1
    while True:
        batch = gh_json(
            f"orgs/{ORG}/packages/container/{PACKAGE}/versions?per_page=100&page={page}"
        )
        assert isinstance(batch, list)
        versions += [version_of(e) for e in batch]
        if len(batch) < 100:
            return versions
        page += 1


def listed_manifests(digest: str) -> list[str]:
    """The manifests an image index lists; none for a single image."""
    raw = subprocess.run(
        ["docker", "buildx", "imagetools", "inspect", "--raw", f"{IMAGE}@{digest}"],
        check=True,
        stdout=subprocess.PIPE,
    ).stdout
    return [m["digest"] for m in json.loads(raw).get("manifests", [])]


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Delete old devcontainer images, never a pinned one."
    )
    parser.add_argument("--dry-run", action="store_true", help="say what would go")
    args = parser.parse_args()

    pinned = pinned_digests([p.read_text() for p in sorted(WORKFLOWS.glob("*.yml"))])
    if not pinned:
        sys.exit(f"no {PACKAGE} digest found in {WORKFLOWS}; refusing to prune blind")
    versions = all_versions()
    present = {v.digest for v in versions}
    for digest in sorted(pinned - present):
        print(
            f"warning: {IMAGE}@{digest} is pinned but not in the registry",
            file=sys.stderr,
        )
    children = {
        v.digest: listed_manifests(v.digest)
        for v in versions
        if v.tags or v.digest in pinned
    }
    doomed = plan(versions, pinned, children, datetime.now(UTC))
    assert not {v.digest for v in doomed} & pinned
    print(f"{len(versions)} versions, {len(pinned)} pinned, deleting {len(doomed)}")
    for v in doomed:
        label = ",".join(v.tags) or "untagged"
        print(f"{'would delete' if args.dry_run else 'deleting'} {v.digest} ({label})")
        if not args.dry_run:
            subprocess.run(
                [
                    "gh",
                    "api",
                    "-X",
                    "DELETE",
                    f"orgs/{ORG}/packages/container/{PACKAGE}/versions/{v.id}",
                ],
                check=True,
            )
    return 0


if __name__ == "__main__":
    sys.exit(main())
