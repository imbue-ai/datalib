#!/usr/bin/env python3
"""Point scripts/install.sh and docs/user/docker.md at one published release.

Both name a release by version and hash rather than following `latest`, which
anyone holding write access can move (docs/dev/security_posture.md). The
release procedure runs this once a release has published, and lands the result
in a PR of its own.

    python3 scripts/release/pin_release.py vX.Y.Z

Refuses a release that is a draft, is not immutable, or that no successful
release.yml run on this tag's commit built: during that run the tag has no
release, and a writer could publish their own on it.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

from publish_release import IMAGE_DIGEST, IMAGE_DIGEST_SHAPE, TARBALL_TRIPLES

REPO = "imbue-ai/datalib"
ROOT = Path(__file__).resolve().parents[2]
INSTALL = "scripts/install.sh"
DOCKER_DOC = "docs/user/docker.md"
RELEASE_WORKFLOW = ".github/workflows/release.yml"
VERSION_SHAPE = re.compile(r"v[0-9]+\.[0-9]+\.[0-9]+")
PINNED_IMAGE = re.compile(r"(ghcr\.io/imbue-ai/datalib@)sha256:[0-9a-f]{64}")
IMAGE_PINS_IN_DOCKER_DOC = 2


def sub_exactly(pattern: str, new: str, text: str, wanted: int = 1) -> str:
    result, count = re.subn(pattern, new, text, flags=re.MULTILINE)
    if count != wanted:
        raise ValueError(f"expected {wanted} of /{pattern}/, found {count}")
    return result


def pin_install(text: str, version: str, sha256_by_triple: dict[str, str]) -> str:
    text = sub_exactly(r'^PINNED_VERSION="[^"]*"$', f'PINNED_VERSION="{version}"', text)
    for triple in TARBALL_TRIPLES:
        text = sub_exactly(
            rf"^(\s*{re.escape(triple)}\) echo )\S+( ;;)$",
            rf"\g<1>{sha256_by_triple[triple]}\g<2>",
            text,
        )
    return text


def pin_docker_doc(text: str, image_digest: str) -> str:
    return sub_exactly(
        PINNED_IMAGE.pattern, rf"\g<1>{image_digest}", text, IMAGE_PINS_IN_DOCKER_DOC
    )


def release_problems(release: dict[str, Any]) -> list[str]:
    problems: list[str] = []
    if release.get("draft"):
        problems.append("it is a draft")
    if release.get("immutable") is not True:
        problems.append("it is not immutable, so its assets can still be replaced")
    return problems


def built_by_release_workflow(
    runs: list[dict[str, Any]], tag: str, commit: str
) -> bool:
    """Whether release.yml succeeded on a push of `tag` at the commit the tag names.

    The commit is what makes this mean anything. A branch can be named like a
    tag and carry an edited workflow, and its run reports the same name; the
    tag's own commit holds the real one, which publishes or fails.
    """
    return any(
        run.get("path") == RELEASE_WORKFLOW
        and run.get("event") == "push"
        and run.get("head_branch") == tag
        and run.get("head_sha") == commit
        and run.get("conclusion") == "success"
        for run in runs
    )


def tarball_hashes(release: dict[str, Any]) -> dict[str, str]:
    digests = {asset["name"]: asset.get("digest", "") for asset in release["assets"]}
    hashes: dict[str, str] = {}
    for triple in TARBALL_TRIPLES:
        name = f"datalib-{triple}.tar.gz"
        digest = digests.get(name, "")
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
            raise ValueError(f"{name}: no usable digest ({digest!r})")
        hashes[triple] = digest.removeprefix("sha256:")
    return hashes


def checked_image_digest(release: dict[str, Any], contents: bytes) -> str:
    """The image digest in `contents`, once it is shown to be the release's own asset."""
    held = next((a for a in release["assets"] if a["name"] == IMAGE_DIGEST), {})
    if held.get("digest") != f"sha256:{hashlib.sha256(contents).hexdigest()}":
        raise ValueError(
            f"{IMAGE_DIGEST}: what was downloaded is not what the release holds"
        )
    text = contents.decode()
    if not IMAGE_DIGEST_SHAPE.fullmatch(text):
        raise ValueError(f"{IMAGE_DIGEST}: not one image digest: {text!r}")
    return text.strip()


def gh_bytes(*args: str) -> bytes:
    return subprocess.run(["gh", *args], check=True, stdout=subprocess.PIPE).stdout


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version")
    args = parser.parse_args()
    if not VERSION_SHAPE.fullmatch(args.version):
        print(f"ERROR: {args.version!r} is not vX.Y.Z", file=sys.stderr)
        return 1
    release = json.loads(gh_bytes("api", f"repos/{REPO}/releases/tags/{args.version}"))
    problems = release_problems(release)
    commit = json.loads(gh_bytes("api", f"repos/{REPO}/commits/tags/{args.version}"))[
        "sha"
    ]
    runs = json.loads(
        gh_bytes(
            "api",
            "-X",
            "GET",
            f"repos/{REPO}/actions/workflows/{Path(RELEASE_WORKFLOW).name}/runs",
            "-f",
            f"branch={args.version}",
            "-f",
            f"head_sha={commit}",
            "-f",
            "event=push",
        )
    )["workflow_runs"]
    if not built_by_release_workflow(runs, args.version, commit):
        problems.append(
            f"no successful {RELEASE_WORKFLOW} run on the tag's commit {commit}, "
            "so someone other than the release may have published it"
        )
    if problems:
        print(
            f"ERROR: not pinning {args.version}: {'; '.join(problems)}", file=sys.stderr
        )
        return 1
    contents = gh_bytes(
        "release",
        "download",
        args.version,
        "--repo",
        REPO,
        "--pattern",
        IMAGE_DIGEST,
        "--output",
        "-",
    )
    image_digest = checked_image_digest(release, contents)

    install = ROOT / INSTALL
    install.write_text(
        pin_install(install.read_text(), args.version, tarball_hashes(release))
    )
    doc = ROOT / DOCKER_DOC
    doc.write_text(pin_docker_doc(doc.read_text(), image_digest))
    print(f"Pinned {INSTALL} and {DOCKER_DOC} to {args.version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
