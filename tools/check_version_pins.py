#!/usr/bin/env python3
"""Assert that every duplicated version pin in the workspace agrees.

A "pin family" is one upstream version that is written down in more than
one file. Each family lists its sites as (path, regex); the regex must
have exactly one capture group, and every site's capture must come out
equal. That is the whole mechanism.

Why a table and not another script
----------------------------------
Pins accumulated faster than checks did. Before this, each family that
got checked at all got its own bespoke `tools/check_*.sh` — so adding a
pin meant writing a script, and the ones nobody wrote a script for
(latchkey, the doltlite CLI, Playwright) simply went unchecked. Several
were only discovered by grepping for version strings.

With a table, extending the guard is adding four lines. That is the
difference between a check that keeps up with the repo and one that
doesn't.

`FAMILIES` below is also the map: where each version is written down,
why the copies have to agree, and which one to change first. That is
deliberately here rather than in a doc under `docs/dev/`. A doc listing
pins rots — it has no way to notice when a pin moves — whereas a wrong
path or pattern here fails the test on the next run. Read this file when
you need to know where a version lives.

Deliberately NOT in scope
-------------------------
* Single-site pins. A version written down once cannot drift, so
  `bazel_dep` versions, the doltlite archive sha256s and friends have
  nothing to check. If one of them ever gains a second home, that is
  exactly when it earns a family here.
* Properties that need something built or run. `//third-party/doltlite:
  cli_version_test` runs the CLI and compares what it reports. Text
  comparison can't do that, and it stays where it is. There used to be a
  second one here, `//third-party/qmd/runtime:node_abi_test`, which
  compared a live Node's ABI against the `node-v<abi>` in better-sqlite3
  prebuilt URLs; qmd 2.8.3 moved to better-sqlite3 13, whose prebuilts
  ship inside the npm tarball and are selected by platform rather than
  by ABI, so both the URLs and the test are gone.
* `.devcontainer/Dockerfile`'s qmd and latchkey: it inherits both from
  the prod image via `FROM ghcr.io/imbue-ai/datalib:${PROD_IMAGE_TAG}`,
  so it has no pin of its own to drift.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# One entry per (file, pattern) that spells out a family's version.
# `pattern` must contain exactly one capture group, and must capture the
# comparable token — see the node-major family for why that matters.
Site = tuple[str, str]


class Family:
    def __init__(self, name: str, why: str, canonical: str, sites: list[Site]):
        self.name = name
        self.why = why
        # Repo-relative path of the site to change FIRST when bumping.
        # Named so a failure can tell you which way to reconcile.
        self.canonical = canonical
        self.sites = sites


FAMILIES: list[Family] = [
    Family(
        name="qmd",
        why=(
            "qmd is installed and invoked from several places that must "
            "agree, or first-run behavior silently diverges between dev "
            "and prod: the Dockerfile bakes one version into the image "
            "while the qmd step invokes another, leaving the baked model "
            "layer unused. A bump once updated one of two same-named Rust "
            "constants and missed the other, so search ran qmd 2.1.0 "
            "against a 2.5.3-built index for six weeks."
        ),
        canonical="datalib/backend/unified_index/src/qmd/mod.rs",
        sites=[
            (
                "datalib/backend/unified_index/src/qmd/mod.rs",
                r'^pub const DEFAULT_QMD_VERSION: &str = "([^"]+)"',
            ),
            ("tests/fixtures/BUILD.bazel", r'^QMD_VERSION = "([^"]+)"'),
            ("datalib/docker/Dockerfile", r"^ARG QMD_VERSION=(\S+)"),
            # The vendored upstream snapshot, read by
            # //tools:qmd_model_cache_path_test for the cache-path check.
            ("third-party/qmd/package.json", r'"version"\s*:\s*"([^"]+)"'),
            # The Bazel-managed package tree that replaced `npx -y`.
            (
                "third-party/qmd/runtime/package.json",
                r'"@tobilu/qmd"\s*:\s*"([^"]+)"',
            ),
        ],
    ),
    Family(
        name="latchkey",
        why=(
            "The Rust constant is what the shipped binaries spawn; the "
            "Dockerfile is what the image bakes. If they disagree the "
            "image warms a version the runtime never invokes, and the "
            "container silently fetches a different latchkey at run time."
        ),
        canonical="datalib/backend/core/src/node_runtime.rs",
        sites=[
            (
                "datalib/backend/core/src/node_runtime.rs",
                r'^pub const LATCHKEY_VERSION: &str = "([^"]+)"',
            ),
            ("datalib/docker/Dockerfile", r"^ARG LATCHKEY_VERSION=(\S+)"),
        ],
    ),
    Family(
        name="doltlite",
        why=(
            "The amalgamation and autoconf archives must come from one "
            "release (:cli_version_test enforces that by running the built "
            "CLI). This adds the two pins that check cannot see: the "
            "Starlark constant it is handed, and the CLI version the prod "
            "image installs from apt."
        ),
        canonical="third-party/doltlite/BUILD.bazel",
        sites=[
            ("third-party/doltlite/BUILD.bazel", r'^DOLTLITE_VERSION = "([^"]+)"'),
            ("MODULE.bazel", r"doltlite-amalgamation-([\d.]+)\.zip"),
            ("MODULE.bazel", r"doltlite-autoconf-([\d.]+)\.tar\.gz"),
            ("datalib/docker/Dockerfile", r"^ARG DOLTLITE_CLI_VERSION=(\S+)"),
        ],
    ),
    Family(
        name="pyright",
        why=(
            "Two packagings of the same tool. `uv run pyright` resolves "
            "the PyPI wrapper from requirements.txt; "
            "//tools/lint:pyright_test runs the npm package. If they "
            "drift, a local run and CI typecheck with different pyright "
            "versions and disagree about what compiles."
        ),
        canonical="requirements.txt",
        sites=[
            ("requirements.txt", r"^pyright==([^\s\\]+)"),
            ("tools/lint/package.json", r'"pyright"\s*:\s*"([^"]+)"'),
        ],
    ),
    Family(
        name="playwright",
        why=(
            "The devcontainer preinstalls the browser bundle for a "
            "specific Playwright version. If the UI lockfile resolves a "
            "different one, `//datalib/ui:e2e_test` runs a Playwright "
            "whose browsers were never staged and fails on a cold "
            "container with a download attempt."
        ),
        canonical="datalib/ui/pnpm-lock.yaml",
        sites=[
            (".devcontainer/Dockerfile", r"^ARG PLAYWRIGHT_VERSION=(\S+)"),
            ("datalib/ui/pnpm-lock.yaml", r"^  '@playwright/test@([\d.]+)':"),
        ],
    ),
    Family(
        name="node-major",
        why=(
            "Three Node runtimes are in play — the one Tauri bundles into "
            "the .app, the one the prod image installs, and the one "
            "rules_js resolves for the build. They need not be identical "
            "patch releases, but a major-version split would put the "
            "shipped app and the image on different N-API ABIs, which is "
            "what decides whether a native module loads."
        ),
        canonical="datalib/tauri/stage-runtime.sh",
        sites=[
            # Capture only the major from each, since that is the part
            # that has to agree.
            ("datalib/tauri/stage-runtime.sh", r'^NODE_VERSION="v(\d+)\.'),
            ("datalib/docker/Dockerfile", r"^ARG NODE_MAJOR=(\d+)"),
        ],
    ),
]


def _scan(root: Path, path: str, pattern: str) -> list[str]:
    """Every capture of `pattern` in `path`. Missing file is an error."""
    f = root / path
    if not f.is_file():
        raise SystemExit(
            f"ERROR: {path} does not exist.\n"
            "A pin site moved or was deleted. Update FAMILIES in "
            "tools/check_version_pins.py."
        )
    rx = re.compile(pattern, re.MULTILINE)
    if rx.groups != 1:
        raise SystemExit(
            f"ERROR: pattern for {path} has {rx.groups} capture groups, need 1:"
            f"\n  {pattern}"
        )
    return rx.findall(f.read_text())


def _check(root: Path, fam: Family) -> str | None:
    """None if the family agrees, else a rendered failure message."""
    found: list[tuple[str, str, str]] = []  # (path, pattern, value)
    for path, pattern in fam.sites:
        matches = _scan(root, path, pattern)
        if not matches:
            return (
                f"[{fam.name}] no match in {path} for:\n"
                f"    {pattern}\n"
                "The file changed shape, so this pin is no longer being "
                "checked — which is worse than a mismatch, because it fails "
                "silently. Fix the pattern in tools/check_version_pins.py."
            )
        # A site is allowed to spell its version more than once (the
        # doltlite URLs do), but those repeats must agree with each other.
        if len(set(matches)) != 1:
            joined = ", ".join(sorted(set(matches)))
            return f"[{fam.name}] {path} disagrees with itself: {joined}"
        found.append((path, pattern, matches[0]))

    values = {v for _, _, v in found}
    if len(values) == 1:
        return None

    canonical = next((v for p, _, v in found if p == fam.canonical), None)
    lines = [
        f"[{fam.name}] pins disagree: {', '.join(sorted(values))}",
        "",
        fam.why,
        "",
    ]
    for path, _, value in found:
        mark = "  <- canonical, change this first" if path == fam.canonical else ""
        lines.append(f"    {value:<12} {path}{mark}")
    if canonical is not None:
        lines += ["", f"Reconcile the others to {canonical!r}."]
    return "\n".join(lines)


def main() -> int:
    # Two ways in: as a bazel test (cwd is the runfiles root) or by hand
    # from anywhere in the tree.
    root = Path.cwd()
    if not (root / "MODULE.bazel").is_file():
        root = Path(__file__).resolve().parent.parent

    failures = [msg for fam in FAMILIES if (msg := _check(root, fam))]
    if failures:
        sys.stderr.write("\n\n".join(failures) + "\n")
        return 1

    sites = sum(len(f.sites) for f in FAMILIES)
    print(f"OK: {len(FAMILIES)} pin families agree across {sites} sites.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
