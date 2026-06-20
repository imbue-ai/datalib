#!/usr/bin/env python3
"""Allowlist-check `no-sandbox` tags in BUILD.bazel files.

Why this exists
---------------
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
The script greps every committed `BUILD.bazel` for `"no-sandbox"`,
counts the targets by package, and compares against
`ALLOWED_NO_SANDBOX` below. A new `no-sandbox` outside the allowlist
fails the lint. A removal of an existing allowed entry also fails
(forcing the allowlist to be updated when usage genuinely changes).

When adding a new entry, document WHY in the dict value — that note
gets surfaced in the failure message if the entry is ever removed.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# Mapping of `<package>:<target-name>` → one-line rationale.
#
# Every entry here is a Bazel rule that legitimately needs to run
# unsandboxed. New additions require updating this dict AND landing
# the BUILD change in the same commit.
ALLOWED_NO_SANDBOX: dict[str, str] = {
    # Live API tests under `frankweiler/backend/etl/providers/*` —
    # tagged `manual`, never auto-run via `bazel test //...`. They
    # shell out to `latchkey`, which reads tokens from the host's
    # keychain / Secret Service — fundamentally non-hermetic.
    "frankweiler/backend/etl/providers/anthropic:anthropic_live": (
        "manual live test, latchkey needs host keychain"
    ),
    "frankweiler/backend/etl/providers/chatgpt:chatgpt_live": (
        "manual live test, latchkey needs host keychain"
    ),
    "frankweiler/backend/etl/providers/github:github_live": (
        "manual live test, latchkey needs host keychain"
    ),
    "frankweiler/backend/etl/providers/email:jmap_live": (
        "manual live test, latchkey needs host keychain"
    ),
    "frankweiler/backend/etl/providers/gitlab:gitlab_live": (
        "manual live test, latchkey needs host keychain"
    ),
    "frankweiler/backend/etl/providers/notion:notion_live": (
        "manual live test, latchkey needs host keychain"
    ),
    "frankweiler/backend/sync:manual_e2e_live_sync_golden": (
        "manual end-to-end test, latchkey needs host keychain"
    ),
    # Wrappers that intentionally run against the source tree, not the
    # sandbox, so they can reuse .venv / node_modules / target / the
    # ms-playwright browser cache.
    "//:precommit_test": (
        "reads source tree (.venv, node_modules, target) deliberately"
    ),
    "frankweiler/ui:e2e_test": (
        "shells out to host pnpm + reuses ~/Library/Caches/ms-playwright"
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


def _scan(root: Path) -> set[str]:
    """Return the set of `<package>:<name>` tagged `no-sandbox`."""
    found: set[str] = set()
    for build_file in root.rglob("BUILD.bazel"):
        if "bazel-" in build_file.parts:
            continue
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


def main() -> int:
    root = Path(__file__).resolve().parent.parent
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
            "ALLOWED_NO_SANDBOX in scripts/lint_no_sandbox.py with a one-"
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
            "ALLOWED_NO_SANDBOX in scripts/lint_no_sandbox.py to match.",
            file=sys.stderr,
        )

    return 1


if __name__ == "__main__":
    sys.exit(main())
