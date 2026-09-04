#!/usr/bin/env python3
"""Repo-hygiene lints that cannot run as Bazel tests.

Both checks here need to enumerate *every* file in the repo, which is
exactly what a Bazel sandbox exists to prevent, so neither can be a
`bazel test` target. They run instead from `bazel run //:precommit` and
as a plain step in `.github/workflows/test.yml`.

  1. `no-sandbox` tags in BUILD.bazel files must be allowlisted.
  2. Every first-party Python file must be reachable by the Bazel lint
     targets, so a new script can't silently escape ruff and pyright.

Check 1: why it exists
----------------------
`no-sandbox` opts a Bazel action out of the sandbox, so it runs
directly in `bazel-out/`. The action's working directory persists
across runs, which means stale state can leak between invocations —
the bug that bit us when doltlite's `backend_index.doltlite_db-wal` from a prior
genrule run got replayed on top of a fresh-looking `backend_index.doltlite_db`,
breaking the very first INSERT of the next run with
`UNIQUE constraint failed`.

The fix in each case is to either (a) sandbox the action, or
(b) explicitly wipe the working dir at the start of every run.
`no-sandbox` is the right tag in some legitimate cases (shelling out
to host tools that need the user's keychain / browser cache /
npm registry / etc.), but every use is a hand-wave we should be
intentional about.

How the allowlist works
-----------------------
The script asks git for every `BUILD.bazel` in the repo (tracked, plus
untracked-but-not-ignored so a staged new file still gets linted),
greps each for `"no-sandbox"`, counts the targets by package, and
compares against
`ALLOWED_NO_SANDBOX` below. A new `no-sandbox` outside the allowlist
fails the lint. A removal of an existing allowed entry also fails
(forcing the allowlist to be updated when usage genuinely changes).

When adding a new entry, document WHY in the dict value — that note
gets surfaced in the failure message if the entry is ever removed.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path

# Mapping of `<package>:<target-name>` → one-line rationale.
#
# Every entry here is a Bazel rule that legitimately needs to run
# unsandboxed. New additions require updating this dict AND landing
# the BUILD change in the same commit.
ALLOWED_NO_SANDBOX: dict[str, str] = {
    # Live API tests under `datalib/backend/etl/providers/*` —
    # tagged `manual`, never auto-run via `bazel test //...`. They
    # shell out to `latchkey`, which reads tokens from the host's
    # keychain / Secret Service — fundamentally non-hermetic.
    "datalib/backend/etl/providers/anthropic:anthropic_live": (
        "manual live test, latchkey needs host keychain"
    ),
    "datalib/backend/etl/providers/chatgpt:chatgpt_live": (
        "manual live test, latchkey needs host keychain"
    ),
    "datalib/backend/etl/providers/github:github_live": (
        "manual live test, latchkey needs host keychain"
    ),
    "datalib/backend/etl/providers/email:jmap_live": (
        "manual live test, latchkey needs host keychain"
    ),
    "datalib/backend/etl/providers/email:gmail_live": (
        "manual live test, latchkey needs host keychain"
    ),
    "datalib/backend/etl/providers/gitlab:gitlab_live": (
        "manual live test, latchkey needs host keychain"
    ),
    "datalib/backend/etl/providers/notion:notion_live": (
        "manual live test, latchkey needs host keychain"
    ),
    "datalib/backend/dag:manual_e2e_live_sync_golden": (
        "manual live golden, latchkey needs host keychain"
    ),
    # Wrappers that intentionally run against the source tree, not the
    # sandbox, so they can reuse .venv / node_modules / target / the
    # ms-playwright browser cache.
    "datalib/ui:e2e_test": (
        "shells out to host pnpm + reuses ~/Library/Caches/ms-playwright"
    ),
    # Applet coverage starts real applet processes and proxies to them
    # over loopback. The store semantics they sit on top of are unit
    # tested hermetically in datalib/backend/http/src/frontend.rs.
    "datalib/backend/http:applet_endpoint_test": (
        "starts applet subprocesses and binds loopback ports"
    ),
    "datalib/backend/http:applet_proxy_test": (
        "starts applet subprocesses and binds loopback ports"
    ),
}

# Regex matching tag-list entries that include `no-sandbox`. The tag
# may sit anywhere inside a `tags = [...]` list (any indentation,
# any neighbors). We match on a quoted string for robustness.
_NO_SANDBOX = re.compile(r'"no-sandbox"')

# Heuristic regex to pull the rule's `name = "..."` out of the
# containing rule block. We walk backward from each `no-sandbox` hit
# until we find a `name = "..."` line at lower indentation than the
# tag — that's the enclosing rule.
_RULE_NAME = re.compile(r'^\s*name\s*=\s*"([^"]+)"')


def _find_enclosing_rule_name(lines: list[str], tag_lineno: int) -> str | None:
    """Walk backwards from `tag_lineno` to find the rule's name."""
    for i in range(tag_lineno - 1, -1, -1):
        m = _RULE_NAME.match(lines[i])
        if m:
            return m.group(1)
    return None


def _git_ls_files(root: Path, pattern: str) -> list[str]:
    """`git ls-files` for `pattern`, repo-relative, or die with the reason.

    Tracked plus untracked-but-not-ignored, so a staged new file is
    linted before it is committed.

    Asking git rather than walking the filesystem is what keeps
    gitignored trees out of the results. In particular `.claude/` holds
    one full checkout per agent worktree, each with its own copy of every
    BUILD file in the repo — walking picked those up and reported ~8
    phantom labels per stale worktree, none of which can ever match the
    repo-relative allowlist keys.

    Surfacing git's stderr matters more than it looks. This used to
    `check=True` with the output captured and discarded, so when git
    refused to read the repo at all the caller saw a bare
    `CalledProcessError ... exit status 128` and nothing else — which is
    exactly how it failed in CI, where the job runs in a container as
    root against a checkout owned by the runner's uid and git reports
    "detected dubious ownership".
    """
    proc = subprocess.run(
        [
            "git",
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
            pattern,
        ],
        cwd=root,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise SystemExit(
            f"ERROR: `git ls-files {pattern}` failed in {root} "
            f"(exit {proc.returncode}):\n{proc.stderr.strip()}"
        )
    return [p for p in proc.stdout.split("\0") if p]


def _build_files(root: Path) -> list[Path]:
    """Every `BUILD.bazel` git knows about, as absolute paths."""
    return [root / p for p in _git_ls_files(root, "*BUILD.bazel")]


def _scan(root: Path) -> set[str]:
    """Return the set of `<package>:<name>` tagged `no-sandbox`."""
    found: set[str] = set()
    for build_file in _build_files(root):
        text = build_file.read_text()
        if "no-sandbox" not in text:
            continue
        lines = text.splitlines()
        package = build_file.parent.relative_to(root).as_posix()
        if package == ".":
            package = ""
        for i, line in enumerate(lines):
            if not _NO_SANDBOX.search(line):
                continue
            name = _find_enclosing_rule_name(lines, i)
            if name is None:
                print(
                    f"WARNING: {build_file}:{i + 1} has no-sandbox but no "
                    "enclosing rule name found; allowlist by hand.",
                    file=sys.stderr,
                )
                continue
            label = f"{package}:{name}" if package else f"//:{name}"
            found.add(label)
    return found


# --- Check 2: Python lint coverage -----------------------------------
#
# `//:python_sources` is the filegroup `//tools:ruff_test` and
# `//tools/lint:pyright_test` lint. Bazel's `glob` cannot cross a package
# boundary, so that filegroup is assembled by hand from these roots — and
# a `.py` added anywhere else would simply go unlinted, with both tests
# still green. That is the same "gate that cannot fail" shape that let
# pyright sit on a non-existent `schemas/` directory for months.
#
# Keep in sync with `//:python_sources` in BUILD.bazel and with
# `[tool.pyright] include` in pyproject.toml. Adding a root means editing
# all three; this check is what makes forgetting one an error.
PYTHON_LINT_ROOTS: tuple[str, ...] = ("scripts", "tests/fixtures", "tools")

# Vendored subtrees are upstream-owned — excluded from ruff via
# `[tool.ruff] extend-exclude` and from pyright via `[tool.pyright]
# exclude`, so they must be excluded here too or this check would demand
# coverage the lint config deliberately declines to provide.
VENDORED_PREFIXES: tuple[str, ...] = ("third-party/",)


def _tracked_python_files(root: Path) -> list[str]:
    """Every git-tracked `*.py`, repo-relative, vendored trees removed."""
    return [
        p for p in _git_ls_files(root, "*.py") if not p.startswith(VENDORED_PREFIXES)
    ]


def _check_python_coverage(root: Path) -> int:
    """Fail if any first-party `.py` sits outside PYTHON_LINT_ROOTS."""
    stray = [
        p
        for p in _tracked_python_files(root)
        if not p.startswith(tuple(f"{r}/" for r in PYTHON_LINT_ROOTS))
    ]
    if stray:
        print("ERROR: Python file(s) outside the Bazel lint roots:", file=sys.stderr)
        for path in sorted(stray):
            print(f"  - {path}", file=sys.stderr)
        print(
            "\nThese are linted by neither //tools:ruff_test nor "
            "//tools/lint:pyright_test.\nEither move them under one of "
            f"{list(PYTHON_LINT_ROOTS)}, or add the new root in all three "
            "places:\n"
            "  - PYTHON_LINT_ROOTS in scripts/lint_repo.py\n"
            "  - the `python_sources` filegroup in BUILD.bazel (plus a\n"
            "    per-package filegroup if the new root is its own package)\n"
            "  - `[tool.pyright] include` in pyproject.toml",
            file=sys.stderr,
        )
        return 1

    pyright_include = _pyright_include(root)
    if pyright_include != list(PYTHON_LINT_ROOTS):
        print(
            "ERROR: `[tool.pyright] include` in pyproject.toml is "
            f"{pyright_include}, but PYTHON_LINT_ROOTS is "
            f"{list(PYTHON_LINT_ROOTS)}.\nThey must match, or `bazel test` "
            "and `uv run pyright` check different files.",
            file=sys.stderr,
        )
        return 1

    print(f"OK: Python lint roots {list(PYTHON_LINT_ROOTS)} cover every tracked *.py.")
    return 0


def _pyright_include(root: Path) -> list[str]:
    with (root / "pyproject.toml").open("rb") as fh:
        return tomllib.load(fh).get("tool", {}).get("pyright", {}).get("include", [])


def _repo_root() -> Path:
    """The source tree to lint.

    Under `bazel run` this script executes out of a runfiles tree, so
    `__file__` points at a symlink farm rather than the checkout and
    `git ls-files` would find nothing. Bazel sets
    `BUILD_WORKSPACE_DIRECTORY` to the real workspace for exactly this
    case; prefer it, and fall back to the `__file__` walk so a direct
    `python3 scripts/lint_repo.py` still works.
    """
    if ws := os.environ.get("BUILD_WORKSPACE_DIRECTORY"):
        return Path(ws)
    return Path(__file__).resolve().parent.parent


def main() -> int:
    root = _repo_root()
    rc = _check_no_sandbox(root)
    rc |= _check_python_coverage(root)
    rc |= _check_module_lock_committed(root)
    return rc


# --- Check 3: MODULE.bazel.lock is committed -------------------------
#
# `.bazelrc` explains why local bazel runs `--lockfile_mode=update`
# (CI-only `error` would fail a dev's build before they could
# regenerate). The cost of that choice is that bazel repairs the lock
# *silently*: a green local `bazelisk test //...` proves nothing about
# whether the file on disk is the file in the commit.
#
# That gap has a specific bite. Resolving a merge that touches both
# `Cargo.lock` and `MODULE.bazel.lock` — take one side for the generated
# file, commit, run the gate — leaves bazel's repair as an uncommitted
# change *after* the merge commit, where nothing looks at it. CI then
# aborts during module resolution and never runs a test, so the failure
# arrives as "no test targets were found" rather than anything about
# lockfiles.
#
# This closes it from the other end. `bazel run //:lint_repo` is itself
# a bazel invocation, so module resolution has already rewritten the
# lock by the time this function runs — meaning a dirty file here means
# "bazel just repaired it, commit the result".
#
# No-op in CI: the job runs this against a fresh checkout (and with
# `--config=ci`, which aborts earlier anyway), so the file cannot be
# dirty there. This is purely a local guard.
LOCK = "MODULE.bazel.lock"


def _check_module_lock_committed(root: Path) -> int:
    proc = subprocess.run(
        ["git", "status", "--porcelain", "--", LOCK],
        cwd=root,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        # Don't turn a git problem into a lint failure — `_git_ls_files`
        # already fails loudly if git cannot read the repo at all.
        print(
            f"WARNING: could not check {LOCK} (git exit "
            f"{proc.returncode}): {proc.stderr.strip()}",
            file=sys.stderr,
        )
        return 0

    if not proc.stdout.strip():
        print(f"OK: {LOCK} matches the commit.")
        return 0

    print(
        f"ERROR: {LOCK} has uncommitted changes.\n\n"
        "  Bazel re-resolved the module graph and rewrote it (the local\n"
        "  `--lockfile_mode=update` default). CI runs `--lockfile_mode=error`\n"
        "  and will abort during module resolution -- before any test -- if\n"
        "  this file does not match its inputs.\n\n"
        "  Commit it:\n\n"
        f"      git add {LOCK} && git commit -m 'regenerate {LOCK}'\n\n"
        "  Expected after any dependency change, and after any merge that\n"
        "  touches both this file and datalib/backend/Cargo.lock. If a\n"
        "  version bump was involved, re-run the gate once more: the\n"
        "  crate_universe extension also rewrites Cargo.lock on the first\n"
        "  pass, which invalidates the hash it just recorded (see .bazelrc).",
        file=sys.stderr,
    )
    return 1


def _check_no_sandbox(root: Path) -> int:
    actual = _scan(root)
    allowed = set(ALLOWED_NO_SANDBOX)

    unexpected = actual - allowed
    missing = allowed - actual

    if not unexpected and not missing:
        print(f"OK: {len(actual)} `no-sandbox` rule(s), all allowlisted.")
        return 0

    if unexpected:
        print("ERROR: unexpected `no-sandbox` tag in:", file=sys.stderr)
        for label in sorted(unexpected):
            print(f"  - {label}", file=sys.stderr)
        print(
            "\nIf this rule genuinely needs to run unsandboxed, add it to "
            "ALLOWED_NO_SANDBOX in scripts/lint_repo.py with a one-"
            "line rationale. If it doesn't, drop the `no-sandbox` tag.",
            file=sys.stderr,
        )

    if missing:
        print(
            "\nERROR: allowlisted `no-sandbox` rule no longer present:", file=sys.stderr
        )
        for label in sorted(missing):
            rationale = ALLOWED_NO_SANDBOX.get(label, "<no rationale>")
            print(f"  - {label}  ({rationale})", file=sys.stderr)
        print(
            "\nIf the rule was renamed or removed intentionally, update "
            "ALLOWED_NO_SANDBOX in scripts/lint_repo.py to match.",
            file=sys.stderr,
        )

    return 1


if __name__ == "__main__":
    sys.exit(main())
