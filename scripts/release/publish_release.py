#!/usr/bin/env python3
"""Create a release, upload to it, check what GitHub holds, and publish it.

release.yml's last job, and the only one that can write to a release. It takes
each asset only from the artifact that should hold it, so a build's artifact
cannot carry a file named like another job's. What it uploads it then reads
back by digest, immediately before publishing and again after, because
publishing locks whatever is there.

    publish_release.py <tag> <artifacts dir> <dmg sha256>

<artifacts dir> holds one directory per artifact. GH_REPO names the repository.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import stat
import subprocess
import sys
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

TARBALL_TRIPLES = (
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
)
RUNTIME_TRIPLES = (
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
)
DMG = "Datalib-aarch64-apple-darwin.dmg"
IMAGE_DIGEST = "docker-image.digest"
IMAGE_DIGEST_SHAPE = re.compile(r"sha256:[0-9a-f]{64}\n?")
SHA256_SHAPE = re.compile(r"[0-9a-f]{64}")
AFTER_PUBLISH_TRIES = 5
AFTER_PUBLISH_WAIT_SECONDS = 15
UPLOAD_RETRY_WAIT_SECONDS = 30

Gh = Callable[..., str]


class Refused(Exception):
    pass


def with_sidecar(name: str) -> set[str]:
    return {name, f"{name}.sha256"}


def expected_artifacts() -> dict[str, set[str]]:
    """Each artifact the release takes, and exactly the files it may hold."""
    expected = {
        f"tarball-{triple}": with_sidecar(f"datalib-{triple}.tar.gz")
        for triple in TARBALL_TRIPLES
    }
    expected.update(
        {
            f"runtime-{triple}": with_sidecar(f"runtime-{triple}.tar.gz")
            for triple in RUNTIME_TRIPLES
        }
    )
    expected["runtime-x86_64-unknown-linux-gnu"] |= with_sidecar(
        "runtime-x86_64-unknown-linux-gnu-cuda.tar.gz"
    )
    expected["macos-app-dmg"] = with_sidecar(DMG)
    expected["docker-image-digest"] = {IMAGE_DIGEST}
    return expected


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def artifact_problems(found: dict[str, set[str]]) -> list[str]:
    """Differences between the artifacts downloaded and the ones expected."""
    expected = expected_artifacts()
    problems = [f"artifact {name}: missing" for name in sorted(expected.keys() - found)]
    problems += [
        f"artifact {name}: unexpected" for name in sorted(found.keys() - expected)
    ]
    for name in sorted(expected.keys() & found.keys()):
        problems += [
            f"artifact {name}: {file} missing"
            for file in sorted(expected[name] - found[name])
        ]
        problems += [
            f"artifact {name}: {file} unexpected"
            for file in sorted(found[name] - expected[name])
        ]
    return problems


def content_problems(
    assets: dict[str, Path], hashes: dict[str, str], dmg_sha256: str
) -> list[str]:
    """What is wrong inside the files: a sidecar, the verified dmg, the image digest."""
    problems: list[str] = []
    for name, path in assets.items():
        if not name.endswith(".sha256"):
            continue
        fields = path.read_text(errors="replace").split()
        recorded = fields[0] if fields else ""
        if recorded != hashes[name.removesuffix(".sha256")]:
            problems.append(f"{name}: says {recorded!r}, not the hash of what it names")
    if hashes[DMG] != dmg_sha256:
        problems.append(
            f"{DMG}: {hashes[DMG]} is not the dmg macos-app-verify checked ({dmg_sha256})"
        )
    if not IMAGE_DIGEST_SHAPE.fullmatch(
        assets[IMAGE_DIGEST].read_text(errors="replace")
    ):
        problems.append(f"{IMAGE_DIGEST}: not one image digest")
    return problems


def read_artifacts(root: Path) -> tuple[dict[str, Path], list[str]]:
    """The assets to publish by name, or every reason not to."""
    found = {
        artifact.name: {entry.name for entry in artifact.iterdir()}
        for artifact in root.iterdir()
        if artifact.is_dir()
    }
    problems = artifact_problems(found)
    problems += [
        f"{entry.name}: not an artifact"
        for entry in root.iterdir()
        if not entry.is_dir()
    ]
    if problems:
        return {}, problems
    assets = {
        file: root / artifact / file
        for artifact, files in expected_artifacts().items()
        for file in files
    }
    problems = [
        f"{name}: not a plain file"
        for name, path in assets.items()
        if not stat.S_ISREG(path.lstat().st_mode)
    ]
    return assets, problems


def held_problems(wanted: dict[str, str], release: dict[str, Any]) -> list[str]:
    """Differences between what we made and what GitHub says this release holds."""
    held = {asset["name"]: asset for asset in release["assets"]}
    problems = [f"{name}: not on the release" for name in sorted(wanted.keys() - held)]
    problems += [
        f"{name}: on the release, and we did not make it"
        for name in sorted(held.keys() - wanted)
    ]
    for name in sorted(wanted.keys() & held.keys()):
        digest = held[name].get("digest")
        if digest != f"sha256:{wanted[name]}":
            problems.append(
                f"{name}: GitHub holds {digest}, we made sha256:{wanted[name]}"
            )
        if held[name].get("state") != "uploaded":
            problems.append(f"{name}: state is {held[name].get('state')}")
    return problems


def clear_the_tag(gh: Gh, repo: str, tag: str) -> None:
    """Remove a draft a failed attempt left, or anyone else's: only ours gets published."""
    lines = gh(
        "api",
        "--paginate",
        f"repos/{repo}/releases",
        "--jq",
        ".[] | {id, tag_name, draft}",
    ).splitlines()
    on_tag = [r for r in map(json.loads, lines) if r["tag_name"] == tag]
    published = [r["id"] for r in on_tag if not r["draft"]]
    if published:
        raise Refused(
            f"{tag} already has a published release ({published}), and this job did not publish it"
        )
    for release in on_tag:
        print(f"Removing draft {release['id']} of {tag}")
        gh("api", "-X", "DELETE", f"repos/{repo}/releases/{release['id']}")


def upload(
    gh: Gh, tag: str, assets: dict[str, Path], pause: Callable[[float], None]
) -> None:
    """Twice: a slow upload that succeeded can still come back as a client error."""
    paths = [str(path) for path in assets.values()]
    try:
        gh("release", "upload", tag, "--clobber", *paths)
    except subprocess.CalledProcessError as error:
        print(f"Upload failed ({error}); once more", file=sys.stderr)
        pause(UPLOAD_RETRY_WAIT_SECONDS)
        gh("release", "upload", tag, "--clobber", *paths)


def read_release(gh: Gh, repo: str, release_id: int) -> dict[str, Any]:
    return json.loads(gh("api", f"repos/{repo}/releases/{release_id}"))


def after_publish_problems(
    gh: Gh, repo: str, tag: str, release_id: int, wanted: dict[str, str]
) -> list[str]:
    release = read_release(gh, repo, release_id)
    problems = held_problems(wanted, release)
    if release.get("draft"):
        problems.append("still a draft")
    if release.get("immutable") is not True:
        problems.append("not immutable: is the repository's setting still on?")
    latest = json.loads(gh("api", f"repos/{repo}/releases/latest")).get("tag_name")
    if latest != tag:
        problems.append(f"releases/latest is {latest}")
    return problems


def publish(
    gh: Gh,
    repo: str,
    tag: str,
    assets: dict[str, Path],
    wanted: dict[str, str],
    pause: Callable[[float], None] = time.sleep,
) -> None:
    clear_the_tag(gh, repo, tag)
    created = json.loads(
        gh(
            "api",
            "-X",
            "POST",
            f"repos/{repo}/releases",
            "-f",
            f"tag_name={tag}",
            "-F",
            "draft=true",
            "-f",
            "body=",
        )
    )
    release_id = created["id"]
    print(f"Created draft {release_id}; uploading {len(assets)} assets")
    upload(gh, tag, assets, pause)

    problems = held_problems(wanted, read_release(gh, repo, release_id))
    if problems:
        raise Refused(
            "the draft is not what this run made:\n  " + "\n  ".join(problems)
        )
    gh(
        "api",
        "-X",
        "PATCH",
        f"repos/{repo}/releases/{release_id}",
        "-F",
        "draft=false",
        "-f",
        "make_latest=true",
    )
    print(f"Published {tag}; reading it back")

    for attempt in range(1, AFTER_PUBLISH_TRIES + 1):
        problems = after_publish_problems(gh, repo, tag, release_id, wanted)
        if not problems:
            print(f"{tag} is published, immutable and holds what this run made")
            return
        if attempt < AFTER_PUBLISH_TRIES:
            pause(AFTER_PUBLISH_WAIT_SECONDS)
    raise Refused(
        f"{tag} IS PUBLISHED and is not as it should be. Look at it now:\n  "
        + "\n  ".join(problems)
    )


def run_gh(*args: str) -> str:
    return subprocess.run(
        ["gh", *args], check=True, stdout=subprocess.PIPE, text=True
    ).stdout


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    parser.add_argument("artifacts", type=Path)
    parser.add_argument("dmg_sha256")
    args = parser.parse_args()
    if not SHA256_SHAPE.fullmatch(args.dmg_sha256):
        print(f"ERROR: {args.dmg_sha256!r} is not a sha256", file=sys.stderr)
        return 1
    assets, problems = read_artifacts(args.artifacts)
    wanted = {name: sha256_of(path) for name, path in assets.items()}
    if not problems:
        problems = content_problems(assets, wanted, args.dmg_sha256)
    if problems:
        print("ERROR: not publishing; no release was created:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    try:
        publish(run_gh, os.environ["GH_REPO"], args.tag, assets, wanted)
    except Refused as refusal:
        print(f"ERROR: {refusal}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
