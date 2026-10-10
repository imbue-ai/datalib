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
  7. Every target tagged `manual` must be named by a `build_test` in the
     same package, because `bazel test //...` never builds a `manual`
     target and one can stop compiling in silence.
  8. A provider that keeps a sync cursor must record the config the
     cursor was taken under, or widening that config is a silent no-op.
  9. The README's source grid and docs/user/getting_your_data.md name
     every source type, link to each other, and stay alphabetical.
 10. No workflow step initializes an empty bash array: expanding one
     is "unbound variable" on the macOS runners' bash 3.2.
 11. No doc, config or script tells anyone to run `npx -y latchkey` or
     `npx -y @tobilu/qmd` without a version: bare `npx -y` resolves
     `latest` at run time, and those two commands are the ones a
     person pastes a live session cookie into.
 12. Every sqlx SQLite pool turns off `idle_timeout` and `max_lifetime`,
     and none is built by a shortcut that takes sqlx's defaults: either
     setting gives the pool a maintenance task, and sqlx 0.9's can spin
     forever and hang the process at exit.
 13. Every source icon is one file in datalib/ui/src/assets/ that both
     catalogs name alike, that shows on the light and the dark theme,
     and that the README's source grid uses rather than a copy.
 14. No first-party Rust outside a test renames a temp file into place
     by hand: `datalib_runtime::atomic` is the one write-then-rename.
 15. Every crate datalib/backend/Cargo.toml lists is named by some
     BUILD.bazel, so the list cannot keep a crate nothing links.

Checks 4, 5 and 6 — a render read must be pinned, a reader must not
open writably, a download takes its store rather than opening one —
were regexes standing in for types. The types exist now: a writer's
handle holds the file's lock for its life, so a second open is refused
rather than colliding later; `open_reader` opens one commit, detached
and read-only, and hands back a `Reader`. See
datalib/backend/etl/README.md, "Connection pools".

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
    # Runs docs/user/docker.md against a published image: it needs the
    # host's docker daemon and a registry pull, and is `manual`.
    "datalib/docker:doc_test": "manual doc test, needs the host docker daemon",
    "datalib/backend/dag:manual_e2e_live_sync_golden": (
        "manual live golden, latchkey needs host keychain"
    ),
    # Wrappers that intentionally run against the source tree, not the
    # sandbox, so they can reuse .venv / node_modules / target / the
    # ms-playwright browser cache.
    "datalib/ui:e2e_test": (
        "shells out to host pnpm + reuses ~/Library/Caches/ms-playwright"
    ),
    "datalib/ui:e2e_auth_test": (
        "manual; reuses ~/Library/Caches/ms-playwright and the host's browser, "
        "which latchkey's own ensure-browser finds"
    ),
    # Applet coverage starts real applet processes and proxies to them
    # over loopback. The store semantics they sit on top of are unit
    # tested hermetically in datalib/backend/http/src/frontend.rs.
    "datalib/backend/http:applet_tests": (
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


# The trailing `#[cfg(test)] mod tests` block, cut off. Only a module at
# column 0 counts: an inline `#[cfg(test)]` on one helper method would
# otherwise hide everything after it from the check.
_TEST_MODULE = re.compile(r"^#\[cfg\(test\)\]\s*\n\s*mod\b", re.MULTILINE)


def _without_test_module(text: str) -> str:
    m = _TEST_MODULE.search(text)
    return text if m is None else text[: m.start()]


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
    rc |= _check_manual_targets_still_build(root)
    rc |= _check_source_grid(root)
    rc |= _check_workflows_no_empty_arrays(root)
    rc |= _check_no_floating_npx(root)
    rc |= _check_pools_never_recycle(root)
    rc |= _check_icons(root)
    rc |= _check_no_hand_rolled_atomic_write(root)
    rc |= _check_cargo_manifest_crates_used(root)
    rc |= _check_bound_lists_are_chunked(root)
    rc |= _check_try_get_ok_is_flattened(root)
    rc |= _check_no_playback_set_var(root)
    return rc


# --- Check 10: no empty bash arrays in workflow steps -------------------
#
# A `run:` step is bash with `set -e` on every runner, and the macOS
# runners' bash is 3.2, where `"${arr[@]}"` on an empty array is an
# "unbound variable" under `set -u` (4.4 fixed it). The idiom that
# trips it is always the same three lines — `arr=()`, a conditional
# append, the expansion — and the first of them is the one a regex can
# see. v0.35.0's `runtime` job failed its mac leg on exactly this, on
# its first run. Two branches, or a scalar, say the same thing.
_EMPTY_ARRAY = re.compile(r"^\s*[A-Za-z_][A-Za-z0-9_]*=\(\s*\)\s*(#.*)?$")


def _check_workflows_no_empty_arrays(root: Path) -> int:
    hits: list[str] = []
    for rel in _git_ls_files(root, ".github/workflows/*.yml"):
        for lineno, line in enumerate((root / rel).read_text().splitlines(), 1):
            if _EMPTY_ARRAY.match(line):
                hits.append(f"  {rel}:{lineno}: {line.strip()}")
    if not hits:
        print("OK: no workflow step initializes an empty bash array.")
        return 0
    print(
        "ERROR: a workflow step initializes an empty bash array:\n\n"
        + "\n".join(hits)
        + "\n\n  Expanding it (\"${arr[@]}\") is 'unbound variable' under `set -u`\n"
        "  on the macOS runners' bash 3.2. Write two branches, or a scalar.",
        file=sys.stderr,
    )
    return 1


# --- Check 9: the README's source grid matches the docs ---------------
#
# The README shows one cell per source, each linking to its section of
# docs/user/getting_your_data.md; that doc has one `## ` section per
# source, opening with its `type = "…"`. Both are hand-kept, and the
# markdown table the grid replaced had quietly dropped three sources.
# So: every type all_sources.toml or the wizard's catalog knows has a
# section, every section has a cell, every cell's anchor and image
# resolve, and both lists are alphabetical.
_GRID_CELL = re.compile(
    r'<a href="docs/user/getting_your_data\.md#([^"]+)">(.*?)<br><b>(.*?)</b></a>',
    re.DOTALL,
)
_GRID_IMAGE = re.compile(r'(?:src|srcset)="([^"]+)"')
_DOC_TYPE_LINE = re.compile(r'^`type = "([a-z_]+)"`', re.MULTILINE)
_CATALOG_TYPE = re.compile(r'\btype: "([a-z_]+)"')


def _github_slug(heading: str) -> str:
    """GitHub's anchor for a heading: lowercase, punctuation dropped,
    spaces to hyphens. Backticks count as punctuation."""
    text = heading.strip().lower()
    text = re.sub(r"[^\w\s-]", "", text)
    return re.sub(r"\s", "-", text)


def _readme_grid(root: Path) -> str:
    text = (root / "README.md").read_text(encoding="utf-8")
    start = text.index("## Supported data sources")
    end = text.index("\n## ", start + 1)
    return text[start:end]


def _doc_sections(root: Path) -> list[tuple[str, str, list[str]]]:
    """(heading, slug, types declared in that section), in file order."""
    text = (root / "docs/user/getting_your_data.md").read_text(encoding="utf-8")
    sections: list[tuple[str, str, list[str]]] = []
    for chunk in re.split(r"^## ", text, flags=re.MULTILINE)[1:]:
        heading, _, body = chunk.partition("\n")
        sections.append((heading, _github_slug(heading), _DOC_TYPE_LINE.findall(body)))
    return sections


def _known_source_types(root: Path) -> set[str]:
    with open(root / "docs/user/config_examples/all_sources.toml", "rb") as fh:
        groups = tomllib.load(fh).get("groups", [])
    types = {g["type"] for g in groups if "type" in g}
    catalog = (root / "datalib/ui/src/config/catalog.ts").read_text(encoding="utf-8")
    types.update(_CATALOG_TYPE.findall(catalog))
    return types


def _check_source_grid(root: Path) -> int:
    bad: list[str] = []
    grid = _readme_grid(root)
    cells = _GRID_CELL.findall(grid)
    sections = _doc_sections(root)
    slugs = {slug for _, slug, _ in sections}

    for anchor, _, label in cells:
        if anchor not in slugs:
            bad.append(
                f'README cell "{label}" links to #{anchor}, which is not a heading'
            )
    for path in _GRID_IMAGE.findall(grid):
        if not (root / path).is_file():
            bad.append(f"README grid image {path} does not exist")

    linked = {anchor for anchor, _, _ in cells}
    declared: dict[str, str] = {}
    for heading, slug, types in sections:
        if not types:
            bad.append(
                f'getting_your_data.md "## {heading}" opens with no `type = "…"` line'
            )
        for t in types:
            declared[t] = slug
        if types and slug not in linked:
            bad.append(
                f'getting_your_data.md "## {heading}" has no cell in the README grid'
            )
    for t in sorted(_known_source_types(root) - set(declared)):
        bad.append(f"source type `{t}` has no section in getting_your_data.md")

    labels = [re.sub(r"&amp;", "&", label) for _, _, label in cells]
    if labels != sorted(labels, key=str.casefold):
        bad.append("README grid cells are not in alphabetical order")
    headings = [h for h, _, _ in sections]
    if headings != sorted(headings, key=str.casefold):
        bad.append("getting_your_data.md sections are not in alphabetical order")

    if not bad:
        print(
            f"OK: README grid has {len(cells)} sources, each with a section in "
            "getting_your_data.md, and every known type is among them."
        )
        return 0
    print(
        "ERROR: the README's source grid and getting_your_data.md disagree:",
        file=sys.stderr,
    )
    for b in bad:
        print(f"  - {b}", file=sys.stderr)
    print(
        "\nA source is a cell in README.md § Supported data sources, linking to\n"
        "`docs/user/getting_your_data.md#<slug>`, and a `## <Name>` section there\n"
        'opening with a `type = "<type>"` line. Add both, in alphabetical order.',
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


# --- Check 7: `manual` targets must still be built somewhere ---------
#
# `manual` keeps a target out of `//...` expansion. That is what the live
# tests want -- they talk to third-party services with the host's
# credentials, and CI must never run them. But it also means CI never
# *builds* them, so one can stop compiling and nothing says so.
#
# `gmail_live` did exactly that: it broke when `FetchOptions` gained its
# non-defaultable `db` field and stayed broken until someone tried to run
# it, at which point the Gmail bug it should have been guarding had
# already shipped.
#
# `build_test` (bazel_skylib) builds a target and runs nothing, and a
# `manual` dependency is still built when something depends on it --
# the tag only affects target-pattern expansion. So each package with a
# `manual` target carries a `build_test` naming it, and this check is
# what keeps that pairing from drifting.
_MANUAL_TAG = re.compile(r'^\s*"manual"\s*,?\s*$|tags\s*=\s*\[[^\]]*"manual"')
_BUILD_TEST_LOCAL_TARGET = re.compile(r'"(:[A-Za-z0-9_.+-]+)"')


def _build_test_targets(text: str) -> set[str]:
    """Same-package labels named by any `build_test(...)` in `text`."""
    out: set[str] = set()
    for m in re.finditer(r"\bbuild_test\s*\(", text):
        depth, i = 0, m.end() - 1
        while i < len(text):
            if text[i] == "(":
                depth += 1
            elif text[i] == ")":
                depth -= 1
                if depth == 0:
                    break
            i += 1
        block = text[m.end() : i]
        out.update(t[1:] for t in _BUILD_TEST_LOCAL_TARGET.findall(block))
    return out


def _manual_targets(root: Path) -> tuple[set[str], set[str]]:
    """`(covered, uncovered)` sets of `//<package>:<name>` tagged `manual`."""
    covered: set[str] = set()
    uncovered: set[str] = set()
    for build_file in _build_files(root):
        text = build_file.read_text()
        if '"manual"' not in text:
            continue
        lines = text.splitlines()
        package = build_file.parent.relative_to(root).as_posix()
        built = _build_test_targets(text)
        for i, line in enumerate(lines):
            if not _MANUAL_TAG.search(line):
                continue
            name = _find_enclosing_rule_name(lines, i)
            if name is None:
                print(
                    f"WARNING: {build_file}:{i + 1} is tagged manual but no "
                    "enclosing rule name was found; check it by hand.",
                    file=sys.stderr,
                )
                continue
            label = f"//{package}:{name}"
            (covered if name in built else uncovered).add(label)
    return covered, uncovered


def _check_manual_targets_still_build(root: Path) -> int:
    covered, uncovered = _manual_targets(root)
    if not uncovered:
        print(f"OK: {len(covered)} `manual` target(s), all covered by a build_test.")
        return 0

    print(
        "ERROR: `manual` target with nothing to build it, so it can rot unnoticed:",
        file=sys.stderr,
    )
    for label in sorted(uncovered):
        print(f"  - {label}", file=sys.stderr)
    print(
        "\nAdd it to the `build_test` in its own BUILD.bazel (creating one "
        "if the package has none):\n\n"
        '    load("@bazel_skylib//rules:build_test.bzl", "build_test")\n\n'
        "    build_test(\n"
        '        name = "manual_targets_build",\n'
        '        targets = [":<the target>"],\n'
        "    )\n",
        file=sys.stderr,
    )
    return 1


# --- Check 11: no floating `npx -y latchkey` / `npx -y @tobilu/qmd` ----
#
# The code pins both (`LATCHKEY_VERSION`, `DEFAULT_QMD_VERSION` in
# datalib/backend/runtime), the installer puts a `latchkey` launcher on
# the PATH, and the app renders the pinned form in every hint it shows.
# The docs were the one place the version floated (#140): `npx -y
# latchkey auth set … $(pbpaste)` hands a session cookie to whatever npm
# served as `latest` that minute. So the only spellings allowed in prose
# are the bare launcher (`latchkey …`) or a pinned `npx -y latchkey@<v>`.
#
# The name must be followed by whitespace to count: a mention in
# backticks (`npx -y latchkey` in a sentence about the rule) is prose
# about the command, not the command.
_FLOATING_NPX = re.compile(r"npx\s+(?:-y\s+)?(?:latchkey|@tobilu/qmd)(?=\s)")
_NPX_LINT_SUFFIXES = (
    ".md",
    ".toml",
    ".yaml",
    ".yml",
    ".sh",
    ".py",
    ".vue",
    ".ts",
    "Dockerfile",
)


def _check_no_floating_npx(root: Path) -> int:
    hits: list[str] = []
    for rel in _git_ls_files(root, "."):
        if rel.startswith("third-party/") or not rel.endswith(_NPX_LINT_SUFFIXES):
            continue
        try:
            text = (root / rel).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        for lineno, line in enumerate(text.splitlines(), 1):
            if _FLOATING_NPX.search(line):
                hits.append(f"  {rel}:{lineno}: {line.strip()}")
    if not hits:
        print("OK: no floating `npx -y latchkey` / `npx -y @tobilu/qmd`.")
        return 0
    print(
        "ERROR: a command that resolves `latest` from npm at run time:\n\n"
        + "\n".join(hits)
        + "\n\n  Write `latchkey …` (the launcher the installer puts on the PATH)\n"
        "  or pin it: `npx -y latchkey@<LATCHKEY_VERSION>`. The pins live in\n"
        "  datalib/backend/runtime/src/node_runtime.rs and runtime/src/qmd.rs.",
        file=sys.stderr,
    )
    return 1


# --- Check 12: every sqlx pool is built with recycling off ---------------
#
# A pool with `idle_timeout` or `max_lifetime` set — sqlx's defaults —
# spawns a maintenance task that loops `for _ in 0..pool.num_idle()`.
# In sqlx 0.9.0 that counter is an unsigned atomic that `release`
# increments only after it has handed the permit back, so a concurrent
# `acquire` can decrement it first and wrap it to `usize::MAX`. A
# maintenance task that reads it then never leaves its poll, and the
# runtime's drop waits on that worker forever: the `render_contract_test`
# exit stall (Sep 2026, datalib/backend/etl/README.md "Connection
# pools"). With both settings off, and no `min_connections`, sqlx spawns
# no task at all. A doltlite pool needs them off for its own reason too.
#
# The builder is read from `SqlitePoolOptions::new()` to the statement's
# `;`, which covers the one-chain form and the `let mut options = …;`
# form alike.
_POOL_BUILDER = re.compile(r"\b\w*PoolOptions(?:::<[^>]*>)?::new\(\)")
_POOL_SHORTCUT = re.compile(
    r"\b\w*Pool(?:::<[^>]*>)?::connect(?:_with|_lazy|_lazy_with)?\("
)
_RECYCLING_OFF = ("idle_timeout(None)", "max_lifetime(None)")


def _check_pools_never_recycle(root: Path) -> int:
    hits: list[str] = []
    for rel in _git_ls_files(root, "datalib/*.rs"):
        text = (root / rel).read_text(encoding="utf-8")
        for m in _POOL_BUILDER.finditer(text):
            end = text.find(";", m.end())
            builder = text[m.start() : end if end != -1 else len(text)]
            if not all(s in builder for s in _RECYCLING_OFF):
                lineno = text.count("\n", 0, m.start()) + 1
                hits.append(
                    f"  {rel}:{lineno}: a pool without idle_timeout(None) and max_lifetime(None)"
                )
        for m in _POOL_SHORTCUT.finditer(text):
            lineno = text.count("\n", 0, m.start()) + 1
            hits.append(
                f"  {rel}:{lineno}: {m.group(0)} builds a pool with sqlx's defaults"
            )
    if not hits:
        print("OK: every sqlx pool is built with recycling off.")
        return 0
    print(
        "ERROR: a sqlx pool with a maintenance task:\n\n"
        + "\n".join(hits)
        + "\n\n  Build it with `SqlitePoolOptions::new()` and chain\n"
        "  `.idle_timeout(None).max_lifetime(None)`. See lint_repo.py check 12.",
        file=sys.stderr,
    )
    return 1


# --- Check 13: one icon file per mark, for both themes and both places --
#
# An icon is a file in datalib/ui/src/assets/, named by its stem. Four
# things used to drift from it by hand: the Rust catalog the rows are
# built from and the browser's catalog the wizard reads (each names
# icons for the same types), the README's source grid, and the dark-theme
# copies the grid kept for solid-black marks. The copies are gone: a mark
# that would vanish on one theme flips its own fill under
# `prefers-color-scheme`. So: every icon a catalog names is a file; the
# two catalogs agree type for type; every SVG shows on each background it
# is drawn on; the grid uses the app's files, and docs/assets/ never holds
# a copy of one.
_ICON_DIR = "datalib/ui/src/assets"
_RUST_CATALOG = "datalib/backend/columns/src/source_catalog.rs"
_RUST_ENTRY = re.compile(
    r'\be\(\s*"([a-z_]+)",\s*(?:None|Some\("([a-z_]+)"\)),\s*"[^"]*",'
    r'\s*(?:None|Some\("([a-z_]+)"\)),?\s*\)'
)
_TS_FIELD = re.compile(r'^    (type|variantKey|icon): "([a-z_]+)",$')
_HEX = r"#[0-9A-Fa-f]{6}\b|#[0-9A-Fa-f]{3}\b"
_SVG_FILL = re.compile(r'\b(?:fill|stroke)="(' + _HEX + r')"')
_SVG_DARK = re.compile(
    r"@media\s*\(prefers-color-scheme:\s*dark\)\s*\{(.*?\})\s*\}", re.DOTALL
)
_CSS_FILL = re.compile(r"\b(?:fill|stroke):\s*(" + _HEX + ")")
_APP_BG = re.compile(r"--datalib-bg:\s*(#[0-9A-Fa-f]{6});")
# GitHub's own dark background; its light one is the app's white.
_GITHUB_DARK_BG = "#0d1117"
# A filled brand mark in its own colour clears this easily (WhatsApp's
# green on white is 2.0); black on the dark theme is 1.0.
_MIN_CONTRAST = 1.5


def _luminance(hex_colour: str) -> float:
    h = hex_colour.lstrip("#")
    if len(h) == 3:
        h = "".join(c * 2 for c in h)

    def channel(c: str) -> float:
        v = int(c, 16) / 255
        return v / 12.92 if v <= 0.03928 else ((v + 0.055) / 1.055) ** 2.4

    r, g, b = (channel(h[i : i + 2]) for i in (0, 2, 4))
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def _contrast(a: str, b: str) -> float:
    la, lb = sorted((_luminance(a), _luminance(b)), reverse=True)
    return (la + 0.05) / (lb + 0.05)


def _svg_palettes(svg: str) -> tuple[set[str], set[str]]:
    """The colours an SVG draws with on a light page and on a dark one.
    A dark-mode rule replaces the attribute colours rather than adding
    to them, which is how every mark here uses one."""
    dark_rule = _SVG_DARK.search(svg)
    light = set(_SVG_FILL.findall(svg))
    dark = set(_CSS_FILL.findall(dark_rule.group(1))) if dark_rule else light
    return light, dark


def _rust_catalog_icons(root: Path) -> dict[tuple[str, str | None], str | None]:
    text = (root / _RUST_CATALOG).read_text(encoding="utf-8")
    return {(t, v or None): icon or None for t, v, icon in _RUST_ENTRY.findall(text)}


def _ts_catalog_icons(root: Path) -> dict[tuple[str, str | None], str | None]:
    text = (root / "datalib/ui/src/config/catalog.ts").read_text(encoding="utf-8")
    body = text[text.index("export const CATALOG") :]
    body = body[: body.index("\n];")]
    out: dict[tuple[str, str | None], str | None] = {}
    for entry in re.split(r"^  \{$", body, flags=re.MULTILINE)[1:]:
        fields: dict[str, str] = {}
        for line in entry.splitlines():
            if m := _TS_FIELD.match(line):
                fields[m.group(1)] = m.group(2)
        out[(fields["type"], fields.get("variantKey"))] = fields.get("icon")
    return out


def _check_icons(root: Path) -> int:
    bad: list[str] = []
    icon_dir = root / _ICON_DIR
    files = {p.stem: p for p in icon_dir.iterdir() if p.suffix in (".svg", ".png")}

    rust = _rust_catalog_icons(root)
    ts = _ts_catalog_icons(root)
    if not rust or not ts:
        bad.append(f"read no entries from {_RUST_CATALOG} or catalog.ts")
    for where, catalog in ((_RUST_CATALOG, rust), ("catalog.ts", ts)):
        for (t, v), icon in sorted(catalog.items(), key=str):
            if icon and icon not in files:
                bad.append(
                    f"{where}: {t}/{v} names icon `{icon}`, not a file in {_ICON_DIR}/"
                )
    for key in sorted(ts, key=str):
        if key not in rust:
            bad.append(f"catalog.ts has {key[0]}/{key[1]}, which {_RUST_CATALOG} lacks")
        elif ts[key] != rust[key]:
            bad.append(
                f"{key[0]}/{key[1]}: catalog.ts says `{ts[key]}`, "
                f"{_RUST_CATALOG} says `{rust[key]}`"
            )

    app_bgs = _APP_BG.findall(
        (root / "datalib/ui/src/theme.css").read_text(encoding="utf-8")
    )
    if len(app_bgs) < 2:
        bad.append("theme.css no longer sets --datalib-bg for a light and a dark theme")
    else:
        backgrounds = [
            ("the light theme", app_bgs[0], False),
            ("the app's dark theme", app_bgs[1], True),
            ("GitHub's dark theme", _GITHUB_DARK_BG, True),
        ]
        for stem, path in sorted(files.items()):
            if path.suffix != ".svg":
                continue
            light, dark = _svg_palettes(path.read_text(encoding="utf-8"))
            if not light:
                bad.append(
                    f"{path.name} draws with no #hex fill this check can measure"
                )
                continue
            for name, bg, is_dark in backgrounds:
                palette = dark if is_dark else light
                best = max(_contrast(c, bg) for c in palette)
                if best < _MIN_CONTRAST:
                    bad.append(
                        f"{path.name} all but vanishes on {name} ({bg}): "
                        f"best contrast {best:.2f}, under {_MIN_CONTRAST}"
                    )

    grid = _readme_grid(root)
    if "<picture" in grid or "srcset" in grid:
        bad.append(
            "the README grid picks an image per theme; the icon should do that itself"
        )
    for rel in _GRID_IMAGE.findall(grid):
        if not rel.startswith((f"{_ICON_DIR}/", "docs/assets/")):
            bad.append(
                f"README grid image {rel} is in neither {_ICON_DIR}/ nor docs/assets/"
            )
    for path in (root / "docs/assets").iterdir():
        if (stem := path.stem.removesuffix("_dark")) in files:
            bad.append(
                f"docs/assets/{path.name} copies {files[stem].relative_to(root)}"
            )

    if not bad:
        print(
            f"OK: {len(files)} icons, each named alike by both catalogs, "
            "visible on both themes, and used by the README as they are."
        )
        return 0
    print("ERROR: the source icons have drifted:", file=sys.stderr)
    for b in bad:
        print(f"  - {b}", file=sys.stderr)
    print(
        f"\nAn icon is one file in {_ICON_DIR}/, named by its stem, drawn for both\n"
        "themes; see the README.md beside it.",
        file=sys.stderr,
    )
    return 1


# --- Check 14: one write-then-rename -----------------------------------
#
# Writing to a temp file and renaming it over the real one was hand-rolled
# nine times (#995). Five used a fixed temp name, so two writers on one
# path shared a temp file: one rename found it gone, and a reader could
# see one writer's bytes cut into the other's. Only two fsynced.
# `datalib_runtime::atomic` takes a fresh temp name per write and fsyncs
# before the rename. The signal is the rename itself, of a variable named
# for a temp file. Tests may still stage a file by hand.
_HAND_ROLLED_SWAP = re.compile(r"\bfs::rename\(\s*&?\w*(?:tmp|temp)\w*", re.IGNORECASE)

# Renames of a temp that is not a file of bytes, with the reason.
_SWAP_ALLOWED: dict[str, str] = {
    "datalib/backend/etl/src/blob_cas.rs": (
        "the temp is a SQLite database another connection fills through "
        "ATTACH; there are no bytes to hand to atomic::write"
    ),
}


def _check_no_hand_rolled_atomic_write(root: Path) -> int:
    hits: list[str] = []
    for rel in _git_ls_files(root, "datalib/*.rs"):
        if "/tests/" in rel or rel in _SWAP_ALLOWED:
            continue
        text = _without_test_module((root / rel).read_text(encoding="utf-8"))
        for m in _HAND_ROLLED_SWAP.finditer(text):
            lineno = text.count("\n", 0, m.start()) + 1
            hits.append(f"  {rel}:{lineno}: {m.group(0)}")
    if not hits:
        print("OK: every write-then-rename goes through datalib_runtime::atomic.")
        return 0
    print(
        "ERROR: a temp file renamed into place by hand:\n\n"
        + "\n".join(hits)
        + "\n\n  Use `datalib_runtime::atomic::write` (or `write_with` to stream,\n"
        "  `write_owner_only` for credentials). See lint_repo.py check 14.",
        file=sys.stderr,
    )
    return 1


# --- Check 15: every crate the Cargo manifest lists is used -------------
#
# The first-party crates have no Cargo.toml; datalib/backend/Cargo.toml is
# one list of the third-party crates, which crate_universe turns into
# `@datalib_crates//:<name>`. Bazel already fails on a target that names a
# crate the list lacks. This is the other direction: a crate nothing names
# any more still costs a resolve, a lockfile entry and a license review.
_CARGO_MANIFEST = "datalib/backend/Cargo.toml"
# `<crate>__<binary>` is the label crate_universe gives a crate's binary.
_CRATE_LABEL = re.compile(
    r"@datalib_crates//:([A-Za-z0-9_.-]+?)(?:__[A-Za-z0-9_.-]+)?\""
)


def _check_cargo_manifest_crates_used(root: Path) -> int:
    manifest = tomllib.loads((root / _CARGO_MANIFEST).read_text(encoding="utf-8"))
    listed = set(manifest.get("dependencies", {})) | set(
        manifest.get("dev-dependencies", {})
    )
    named: set[str] = set()
    for rel in _git_ls_files(root, "*BUILD.bazel") + _git_ls_files(root, "*.bzl"):
        named.update(_CRATE_LABEL.findall((root / rel).read_text(encoding="utf-8")))
    unused = sorted(listed - named)
    if not unused:
        print(f"OK: every crate {_CARGO_MANIFEST} lists is named by a BUILD.bazel.")
        return 0
    print(
        f"ERROR: {_CARGO_MANIFEST} lists crates no BUILD.bazel names:\n\n"
        + "\n".join(f"  {name}" for name in unused)
        + "\n\n  Delete them from the manifest and run tools/repin_cargo.sh.\n"
        "  See lint_repo.py check 15.",
        file=sys.stderr,
    )
    return 1


# --- Check 16: a `?` run is sized by a chunk, never by a whole set ------
#
# SQLite refuses a statement that binds more than 32,766 values. A `?,?,…`
# list sized from a load set passes every test, whose sets are small, and
# fails on the person with the big mailbox: email render's
# `thread_id IN (…)` did, on a full render of a JMAP account (#1156). So a
# list of values to match is bound as one JSON array,
# `IN (SELECT value FROM json_each(?))`, which has no limit; and a `?` run
# is sized from `chunk.len()` of a `.chunks(N)` loop, which is what a
# multi-row `VALUES` needs. `datalib/backend/etl/README.md` §"Binding a set
# of values" has the rule.
# Where a `?` run is built; the size expression follows.
_PLACEHOLDER_RUN = re.compile(
    r"""repeat_n\(\s*"\?"\s*,|vec!\[\s*"\?"\s*;|"\?,?\s*"\.repeat\("""
    r"""|push_placeholder_list\(\s*&mut\s+\w+\s*,|push_placeholders\(\s*&mut\s+\w+\s*,"""
    r"""|(?<!fn )\bplaceholders\("""
)

# `?` runs sized by something that is not data, with the reason.
_RUN_ALLOWED: dict[tuple[str, str], str] = {
    ("datalib/backend/applets/src/unified_index/problems.rs", "columns.len()"): (
        "one row's VALUES, one `?` per column of a table the code declares"
    ),
    ("datalib/backend/etl/render/src/search_terms.rs", "per_row"): (
        "the helper's own `(?, ?)` tuple; its callers are checked"
    ),
}


def _size_expression(text: str, at: int) -> str:
    """The argument starting at `at`, up to its `,`, `)` or `]` at depth 0."""
    depth = 0
    for i in range(at, len(text)):
        c = text[i]
        if c in "([{":
            depth += 1
        elif c in ")]}":
            if depth == 0:
                return text[at:i].strip()
            depth -= 1
        elif c in ",;" and depth == 0:
            return text[at:i].strip()
    return text[at:].strip()


def _check_bound_lists_are_chunked(root: Path) -> int:
    hits: list[str] = []
    for rel in _git_ls_files(root, "datalib/*.rs"):
        if "/tests/" in rel:
            continue
        text = _without_test_module((root / rel).read_text(encoding="utf-8"))
        for m in _PLACEHOLDER_RUN.finditer(text):
            size = " ".join(_size_expression(text, m.end()).split())
            if size == "chunk.len()" or (rel, size) in _RUN_ALLOWED:
                continue
            lineno = text.count("\n", 0, m.start()) + 1
            hits.append(f"  {rel}:{lineno}: a `?` run sized by `{size}`")
    if not hits:
        print("OK: every `?` run is sized by a chunk.")
        return 0
    print(
        "ERROR: a `?` run sized by something other than a chunk:\n\n"
        + "\n".join(hits)
        + "\n\n  Bind the set as one JSON array, `IN (SELECT value FROM json_each(?))`,\n"
        "  or size the run from `chunk.len()` in a `.chunks(N)` loop. See\n"
        "  lint_repo.py check 16.",
        file=sys.stderr,
    )
    return 1


# --- Check 17: a `try_get(...).ok()` reads `Option<T>` -------------------
#
# sqlx's `Row::try_get` skips its type check for a NULL, and its SQLite
# decoders read a NULL as `""` or `0` (doltlite_facts'
# `a_null_read_as_a_bare_type_is_its_default_not_an_error`). So
# `let x: Option<String> = r.try_get("c").ok()` is `Some("")` for a NULL:
# the type is inferred as a bare `String`, and the read succeeds (#13).
# Reading `Option<T>` and flattening is the one spelling of "maybe absent"
# that is right; a column that cannot be NULL is read with `?` instead,
# so a missing column fails rather than reading as nothing.
_TRY_GET_OK = re.compile(
    r"\btry_get(?:::<[^()]*>)?\((?:[^()]|\([^()]*\))*\)\s*\.ok\(\)(?!\s*\.flatten\(\))"
)

# Files that spell the trap on purpose, with the reason.
_TRY_GET_OK_ALLOWED: dict[str, str] = {
    "datalib/backend/doltlite_facts/doltlite_facts.rs": (
        "the test that shows a NULL read as a bare type is its default"
    ),
}


def _check_try_get_ok_is_flattened(root: Path) -> int:
    hits: list[str] = []
    for rel in _git_ls_files(root, "datalib/*.rs"):
        if rel in _TRY_GET_OK_ALLOWED:
            continue
        text = (root / rel).read_text(encoding="utf-8")
        for m in _TRY_GET_OK.finditer(text):
            line_start = text.rfind("\n", 0, m.start()) + 1
            if text[line_start : m.start()].lstrip().startswith("//"):
                continue
            lineno = text.count("\n", 0, m.start()) + 1
            hits.append(f"  {rel}:{lineno}: {' '.join(m.group(0).split())}")
    if not hits:
        print("OK: every `try_get(...).ok()` reads an `Option` and flattens it.")
        return 0
    print(
        'ERROR: a `try_get(...).ok()` that reads a NULL as `Some("")` or `Some(0)`:\n\n'
        + "\n".join(hits)
        + "\n\n  Read the column with `?` (or `.context(..)?`), as `Option<T>` if it\n"
        "  can be NULL; where a missing column really is no answer, spell it\n"
        "  `try_get::<Option<T>, _>(..).ok().flatten()`. See lint_repo.py check 17.",
        file=sys.stderr,
    )
    return 1


# --- Check 18: playback is scoped, not set in the environment -----------
#
# `DATALIB_HTTP_PLAYBACK` and its siblings select the tape for a whole
# process: a step launched by a test, the fixture pipeline, an e2e
# backend. Set in a test with `set_var`, it is every test's in that
# binary, and they run in parallel. A test scopes its future to its tape
# instead (`datalib_etl_web::playback::scope`). The step's own
# `--playback-root` is the one in-process setter.
_PLAYBACK_SET_VAR = re.compile(
    r"\bset_var\(\s*(?:[\w:]*PLAYBACK\w*_ENV\b|\"DATALIB_HTTP_PLAYBACK)"
)
_PLAYBACK_SET_VAR_ALLOWED = {"datalib/backend/datalib_step/src/main.rs"}


def _check_no_playback_set_var(root: Path) -> int:
    hits: list[str] = []
    for rel in _git_ls_files(root, "datalib/*.rs"):
        if rel in _PLAYBACK_SET_VAR_ALLOWED:
            continue
        text = (root / rel).read_text(encoding="utf-8")
        for m in _PLAYBACK_SET_VAR.finditer(text):
            lineno = text.count("\n", 0, m.start()) + 1
            hits.append(f"  {rel}:{lineno}: {m.group(0)}")
    if not hits:
        print("OK: no test sets the playback environment variables.")
        return 0
    print(
        "ERROR: playback pointed at a tape through the environment:\n\n"
        + "\n".join(hits)
        + "\n\n  Run the future under `datalib_etl_web::playback::scope(<tape>, …)`;\n"
        "  `Playback::delay`, `hold` and `hold_sealed` set the rest.\n"
        "  See lint_repo.py check 18.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
