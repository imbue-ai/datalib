#!/usr/bin/env python3
"""Assert `stage-runtime.sh`'s Node digests match what nodejs.org publishes.

`datalib/tauri/stage-runtime.sh` bundles a Node runtime into the desktop
app. It pins `NODE_VERSION` plus a sha256 per platform, and verifies the
tarball against that digest at build time — so a stale digest is not a
security hole, it is a hard build failure.

Why this test exists
--------------------
Nothing else covers it. The Tauri app is driven by `cargo tauri` /
`pnpm`, not Bazel, so `bazel test //...` never runs `stage-runtime.sh`;
the first thing to notice a bad pin is `release.yml`'s macos-app job,
*after* a tag has been pushed. That is exactly how v0.30.0 shipped
broken: NODE_VERSION moved v22.23.1 -> v22.23.2 in a dependency bump and
the four digests did not follow, so every macOS build died with
`sha256 mismatch for node-v22.23.2-darwin-arm64.tar.gz` — three retries,
then a failed release leg, on a green-looking merge.

The file's own header says "Bumping NODE_VERSION means re-pinning ALL
FOUR digests" and gives the curl one-liner. That comment was correct and
was still missed, which is the argument for a test rather than a louder
comment.

Why it needs the network
------------------------
The bug is "the pins disagree with upstream", and upstream is the only
thing that can settle it. A stale set of digests is perfectly
self-consistent, so no amount of in-repo cross-checking (the
`check_version_pins.py` model) can see it.

Fetching `SHASUMS256.txt` from the same origin as the tarballs adds
nothing against a compromised origin — but that is not this test's job,
and `stage-runtime.sh`'s header already makes that argument for why the
digests are pinned in the first place. This test answers a different
question: do the pins describe the version we claim to ship?
"""

from __future__ import annotations

import re
import sys
import urllib.request
from pathlib import Path

STAGE_RUNTIME = "datalib/tauri/stage-runtime.sh"
SHASUMS_URL = "https://nodejs.org/dist/{version}/SHASUMS256.txt"
TIMEOUT_S = 30


def _read_pins(script: Path) -> tuple[str, list[tuple[str, str]]]:
    """`(NODE_VERSION, [(platform, sha256), ...])` from stage-runtime.sh.

    The platform/digest pairs come from a `case` block, so they are
    matched as adjacent lines rather than parsed as shell.
    """
    text = script.read_text()

    m = re.search(r'^NODE_VERSION="([^"]+)"', text, re.MULTILINE)
    if not m:
        raise SystemExit(
            f"ERROR: no NODE_VERSION in {STAGE_RUNTIME}.\n"
            "The pin moved or changed shape; update this test's pattern."
        )
    version = m.group(1)

    pairs = re.findall(
        r'^\s*node_platform="([^"]+)"\s*\n\s*node_sha256="([0-9a-f]{64})"',
        text,
        re.MULTILINE,
    )
    if not pairs:
        raise SystemExit(
            f"ERROR: no node_platform/node_sha256 pairs in {STAGE_RUNTIME}.\n"
            "The `case` block changed shape, so this test is no longer "
            "checking anything — which is worse than a mismatch."
        )
    return version, pairs


def _fetch_shasums(version: str) -> dict[str, str]:
    """`{filename: sha256}` from nodejs.org for `version`."""
    url = SHASUMS_URL.format(version=version)
    try:
        with urllib.request.urlopen(url, timeout=TIMEOUT_S) as resp:
            body = resp.read().decode("utf-8")
    except Exception as exc:
        raise SystemExit(f"ERROR: could not fetch {url}: {exc}") from exc

    out: dict[str, str] = {}
    for line in body.splitlines():
        parts = line.split()
        if len(parts) == 2:
            out[parts[1]] = parts[0]
    return out


def main() -> int:
    root = Path.cwd()
    if not (root / "MODULE.bazel").is_file():
        root = Path(__file__).resolve().parent.parent

    script = root / STAGE_RUNTIME
    if not script.is_file():
        raise SystemExit(f"ERROR: {STAGE_RUNTIME} does not exist.")

    version, pairs = _read_pins(script)
    published = _fetch_shasums(version)

    failures: list[str] = []
    for platform, pinned in pairs:
        name = f"node-{version}-{platform}.tar.gz"
        upstream = published.get(name)
        if upstream is None:
            failures.append(
                f"  {platform:<14} nodejs.org publishes no {name}\n"
                f"  {'':<14} (is {version} a real release?)"
            )
        elif upstream != pinned:
            failures.append(
                f"  {platform:<14} pinned   {pinned}\n  {'':<14} upstream {upstream}"
            )

    if failures:
        sys.stderr.write(
            f"Node digests in {STAGE_RUNTIME} disagree with nodejs.org "
            f"for {version}:\n\n" + "\n".join(failures) + "\n\n"
            "Bumping NODE_VERSION means re-pinning ALL FOUR digests:\n"
            f"  curl -fsSL {SHASUMS_URL.format(version=version)} \\\n"
            "    | grep -E '(darwin-(arm64|x64)|linux-(arm64|x64))\\.tar\\.gz$'\n"
        )
        return 1

    print(f"OK: {len(pairs)} Node digests match nodejs.org for {version}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
