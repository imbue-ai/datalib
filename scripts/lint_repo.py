#!/usr/bin/env python3
"""Repo-hygiene lints that cannot run as Bazel tests.

These checks need to look at the repo as a whole — every tracked file,
or git's own view of it — which is exactly what a Bazel sandbox exists
to prevent, so none of them can be a `bazel test` target. They run
instead from `bazel run //:precommit` and as a plain step in
`.github/workflows/test.yml`.

  1. `no-sandbox` tags in BUILD.bazel files must be allowlisted.
  2. Every first-party Python file must be reachable by the Bazel lint
     targets, so a new script can't silently escape ruff and pyright.
  3. MODULE.bazel.lock must match the commit, because bazel repairs it
     silently and CI aborts on it.
  4. Render code must not read a doltlite content table unpinned, since
     an unpinned read returns uncommitted rows once producers stream.
  5. Render code must not open a doltlite store writably: `open` writes on
     the way in, and the render step does not own the store it reads.

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
    "datalib/backend/etl/providers/claude:claude_live": (
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
    rc |= _check_unpinned_render_reads(root)
    rc |= _check_render_opens_read_only(root)
    return rc


# --- Check 5: render must not open a store writably ------------------
#
# `doltlite_raw::open` is not a read: it seals a dirty working tree into a
# rescue commit, reconciles the schema, and commits with `-Am`, which takes
# whatever else was dirty with it. The download step owns the raw store and
# wants all three. Render only reads it, and once downloads commit
# incrementally, a render that opens this way seals the downloader's
# half-written batch on its behalf -- which pinning cannot protect against,
# because the torn rows are then genuinely committed.
#
# `open_reader` is the read path. This keeps render on it.
_WRITABLE_OPEN = re.compile(
    r"\b(?:RawDb|BlobCas|dr|doltlite_raw|datalib_etl::doltlite_raw)::open\("
)


def _check_render_opens_read_only(root: Path) -> int:
    bad: list[str] = []
    for rel in _render_sources(root):
        text = (root / rel).read_text(encoding="utf-8", errors="replace")
        # Test modules build the stores they then read, so they need `open`.
        body = text.split("#[cfg(test)]")[0]
        for lineno, line in enumerate(body.splitlines(), 1):
            if _WRITABLE_OPEN.search(line):
                bad.append(f"{rel}:{lineno}: {line.strip()}")
    if not bad:
        print("OK: no render path opens a doltlite store writably.")
        return 0
    print("ERROR: render code opens a doltlite store writably:", file=sys.stderr)
    for b in bad:
        print(f"  - {b}", file=sys.stderr)
    print(
        "\n`open` rescue-commits, reconciles the schema and commits with -Am --\n"
        "three writes to a store the render step does not own. Use the\n"
        "read-only path instead: `open_reader`.\n"
        "See docs/dev/streaming_steps_plan.md.",
        file=sys.stderr,
    )
    return 1


# --- Check 4: unpinned content reads in render code ------------------
#
# A plain `SELECT` against a doltlite store reads its working set, which
# is shared across processes and holds rows a writer has not committed.
# That is harmless today because a render step only runs after its
# download step has exited, and it stops being harmless the moment the
# scheduler is allowed to start a consumer early -- the consumer gets a
# *torn* view, part of one commit and part of a batch still being
# written, with no error anywhere.
#
# The fix is to read the pinned view instead: `datalib_etl::pin::install_views`
# creates a `pinned_<table>` view over `dolt_at_<table>(...)` once per
# connection, so a query changes from `FROM users` to `FROM pinned_users`
# and nothing else. The fix is across ~50 sites in ten crates, and one
# missed site is a silent data bug -- so this is a ratchet rather than a
# review question. See `docs/dev/streaming_steps_plan.md`.
#
# `EXPECTED_UNPINNED_READS` is the baseline being worked off. Numbers may
# only go down; a file that reaches zero comes out of the dict. Both
# directions fail, so the sweep cannot stall silently and new code cannot
# quietly add a site.
EXPECTED_UNPINNED_READS: dict[str, int] = {
    "datalib/backend/etl/providers/beeper/src/render/parse.rs": 6,
    "datalib/backend/etl/providers/chatgpt/src/render/parse.rs": 4,
    "datalib/backend/etl/providers/claude/src/render/parse.rs": 1,
    "datalib/backend/etl/providers/email/src/render/parse.rs": 8,
    "datalib/backend/etl/providers/google_takeout/src/render.rs": 1,
    "datalib/backend/etl/providers/signal/src/render/parse.rs": 6,
    "datalib/backend/etl/providers/slack/src/render/parse.rs": 9,
    "datalib/backend/etl/providers/sms_backup_restore/src/render.rs": 1,
    "datalib/backend/etl/providers/whatsapp/src/render/parse.rs": 7,
    "datalib/backend/etl/providers/yolink/src/render/parse.rs": 5,
}

# `pinned_` is the whole point: a view over `dolt_at_<table>`, so reading it
# is reading committed state. `dolt_*` are the history vtabs (already
# committed-only), and `pragma_*` / `sqlite_*` are engine tables with no
# working set of their own.
_PINNED_OK_PREFIXES = ("pinned_", "dolt_", "pragma_", "sqlite_")

# A table named directly after FROM or JOIN. A `{placeholder}` does not
# match (it starts with `{`), which is what makes a swept site invisible
# here, and neither does `FROM (` for a subquery.
_TABLE_READ = re.compile(r"\b(?:FROM|JOIN)\s+([a-z_][a-z0-9_]*)")


def _render_sources(root: Path) -> list[str]:
    return [
        p
        for p in _git_ls_files(root, "datalib/backend/etl/providers")
        if "/src/render" in p and p.endswith(".rs")
    ]


def _unpinned_reads(root: Path, rel: str) -> list[tuple[int, str]]:
    out: list[tuple[int, str]] = []
    text = (root / rel).read_text(encoding="utf-8", errors="replace")
    for lineno, line in enumerate(text.splitlines(), 1):
        for table in _TABLE_READ.findall(line):
            if not table.startswith(_PINNED_OK_PREFIXES):
                out.append((lineno, table))
    return out


def _check_unpinned_render_reads(root: Path) -> int:
    actual = {
        rel: len(hits)
        for rel in _render_sources(root)
        if (hits := _unpinned_reads(root, rel))
    }
    if actual == EXPECTED_UNPINNED_READS:
        total = sum(actual.values())
        print(f"OK: {total} unpinned render read(s), matching the baseline.")
        return 0

    added = {
        rel: n for rel, n in actual.items() if n > EXPECTED_UNPINNED_READS.get(rel, 0)
    }
    fixed = {
        rel: n for rel, n in EXPECTED_UNPINNED_READS.items() if n > actual.get(rel, 0)
    }

    if added:
        print("ERROR: unpinned content read(s) added in render code:", file=sys.stderr)
        for rel in sorted(added):
            was = EXPECTED_UNPINNED_READS.get(rel, 0)
            print(f"  - {rel}: {was} -> {added[rel]}", file=sys.stderr)
            for lineno, table in _unpinned_reads(root, rel):
                print(f"      {rel}:{lineno}: {table}", file=sys.stderr)
        print(
            "\nA plain SELECT reads doltlite's working set, so it can return\n"
            "rows the producer has not committed. Read the pinned view instead:\n"
            "`FROM pinned_<table>`, with `datalib_etl::pin::install_views` called\n"
            "once where the store is opened for reading.\n"
            "See docs/dev/streaming_steps_plan.md.",
            file=sys.stderr,
        )

    if fixed:
        print(
            "\nERROR: unpinned read(s) fixed without updating the baseline:",
            file=sys.stderr,
        )
        for rel in sorted(fixed):
            print(
                f"  - {rel}: {fixed[rel]} -> {actual.get(rel, 0)}",
                file=sys.stderr,
            )
        print(
            "\nGood news, but the ratchet has to move with it. Update\n"
            "EXPECTED_UNPINNED_READS in scripts/lint_repo.py (drop the entry\n"
            "entirely when it reaches zero).",
            file=sys.stderr,
        )

    return 1


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
