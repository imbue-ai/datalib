#!/usr/bin/env python3
"""Draft a release's notes: what changed in the raw stores, and the PRs.

An app that reads a raw store directly (`docs/user/raw_stores.md`) needs
to know when a table's shape changes, or when a migration rewrites what
a value means. Both are recorded in the tree: each tag's
`raw_shapes/current.json` is the shape that build creates, and each rung
on a migration ladder is a `Migration { version, name, .. }` in its
provider's source. This reads them at the two tags and writes markdown.
The release skill runs it and puts the result on the GitHub release.

    tools/release_notes.py <previous tag> <new tag or ref>

It fails, rather than writing a short list, when a ladder rose by more
rungs than it can find.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from dataclasses import dataclass, field

_SHAPES = "datalib/backend/datalib_step/raw_shapes/current.json"
_PROVIDERS = "datalib/backend/etl/providers"
_SHARED_LADDER = "datalib/backend/etl/src/doltlite_raw.rs"
_GUIDE = "docs/user/raw_stores.md"

_RUNG = re.compile(
    r'Migration\s*\{\s*version:\s*(\d+),\s*name:\s*"((?:[^"\\]|\\.)*)"', re.DOTALL
)
_TABLE = re.compile(
    r'CREATE TABLE\s+(?:IF NOT EXISTS\s+)?"?(\w+)"?\s*\((.*)\)\s*$', re.DOTALL
)
_CONSTRAINT = ("PRIMARY", "UNIQUE", "FOREIGN", "CHECK", "CONSTRAINT")


def rungs_in(source: str) -> dict[int, str]:
    """Every rung a Rust file declares outside its tests, by version."""
    source = source.split("#[cfg(test)]", 1)[0]
    return {int(v): name.replace('\\"', '"') for v, name in _RUNG.findall(source)}


def _split_top_level(body: str) -> list[str]:
    parts, depth, start = [], 0, 0
    for i, ch in enumerate(body):
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        elif ch == "," and depth == 0:
            parts.append(body[start:i])
            start = i + 1
    parts.append(body[start:])
    return [p.strip() for p in parts if p.strip()]


def tables_in(sql: list[str]) -> dict[str, dict[str, str]]:
    """Each table's columns, name to definition; indexes are left out."""
    tables = {}
    for statement in sql:
        m = _TABLE.match(statement.strip())
        if not m:
            continue
        columns = {}
        for part in _split_top_level(m.group(2)):
            if part.split()[0].upper() in _CONSTRAINT:
                continue
            name, _, definition = part.partition(" ")
            columns[name.strip('"')] = definition.strip()
        tables[m.group(1)] = columns
    return tables


@dataclass
class StoreChange:
    source_type: str
    rungs: list[str] = field(default_factory=list)
    breaking: list[str] = field(default_factory=list)
    added: list[str] = field(default_factory=list)

    def empty(self) -> bool:
        return not (self.rungs or self.breaking or self.added)


def table_changes(old: dict[str, dict[str, str]], new: dict[str, dict[str, str]]):
    """What a reader sees move between two shapes: (breaking, added)."""
    breaking, added = [], []
    for table in sorted(old.keys() - new.keys()):
        breaking.append(f"table `{table}` is gone")
    for table in sorted(new.keys() - old.keys()):
        added.append(f"new table `{table}`")
    for table in sorted(old.keys() & new.keys()):
        before, after = old[table], new[table]
        for col in sorted(before.keys() - after.keys()):
            breaking.append(f"`{table}.{col}` is gone")
        for col in sorted(before.keys() & after.keys()):
            if before[col] != after[col]:
                breaking.append(
                    f"`{table}.{col}` was `{before[col]}`, is `{after[col]}`"
                )
        for col in sorted(after.keys() - before.keys()):
            added.append(f"new column `{table}.{col}` `{after[col]}`")
    return breaking, added


def climbed(
    ladder: dict[int, str], old_version: int, new_version: int, what: str
) -> list[str]:
    wanted = range(old_version + 1, new_version + 1)
    missing = [v for v in wanted if v not in ladder]
    if missing:
        raise SystemExit(
            f"{what}'s ladder rose from {old_version} to {new_version}, but no "
            f"`Migration {{ version, name }}` was found for {missing}; "
            "tools/release_notes.py no longer reads the ladder's source"
        )
    return [f"{ladder[v]} (rung {v})" for v in wanted]


def store_changes(old_shapes: dict, new_shapes: dict, ladder_of) -> list[StoreChange]:
    """`ladder_of(source_type)` returns that provider's rungs at the new tag."""
    changes = []
    for source_type in sorted(old_shapes.keys() | new_shapes.keys()):
        change = StoreChange(source_type)
        old, new = old_shapes.get(source_type), new_shapes.get(source_type)
        if old is None:
            change.added.append("new source type")
        elif new is None:
            change.breaking.append("the source type has no raw store any more")
        else:
            if new["schema_version"] > old["schema_version"]:
                change.rungs = climbed(
                    ladder_of(source_type),
                    old["schema_version"],
                    new["schema_version"],
                    source_type,
                )
            change.breaking, change.added = table_changes(
                tables_in(old["sql"]), tables_in(new["sql"])
            )
        if not change.empty():
            changes.append(change)
    return changes


def shared_rungs(
    old_shapes: dict, new_shapes: dict, shared_ladder: dict[int, str]
) -> list[str]:
    def top(shapes: dict) -> int:
        return max((s["shared_schema_version"] for s in shapes.values()), default=0)

    return climbed(shared_ladder, top(old_shapes), top(new_shapes), "the shared")


def pull_requests(subjects: list[str]) -> list[str]:
    return [s for s in subjects if not s.startswith("chore(release)")]


def render(
    previous: str, shared: list[str], changes: list[StoreChange], prs: list[str]
) -> str:
    out = []
    if shared or changes:
        intro = (
            f"For apps that read a source's raw store directly ([{_GUIDE}]"
            f"(https://github.com/imbue-ai/datalib/blob/main/{_GUIDE})), since "
            f"{previous}. A store an older build wrote is migrated the first "
            "time this one opens it. A migration can change what a stored "
            "value means without changing the table's shape, so check each "
            "one against what your reader expects."
        )
        out += ["## Changes to the raw stores", "", intro, ""]
        if shared:
            out.append("**Every raw store**")
            out += [f"- Migrated on first open: {r}" for r in shared]
            out.append("")
        for change in changes:
            out.append(f"**`{change.source_type}`**")
            out += [f"- Migrated on first open: {r}" for r in change.rungs]
            out += [f"- Breaking: {b}" for b in change.breaking]
            out += [f"- Added: {a}" for a in change.added]
            out.append("")
    else:
        out += [f"No raw store changed shape or was migrated since {previous}.", ""]
    out += [f"## Pull requests since {previous}", ""]
    out += [f"- {pr}" for pr in prs] or ["(none)"]
    return "\n".join(out) + "\n"


def _git(*args: str) -> str:
    return subprocess.run(
        ["git", *args], check=True, capture_output=True, text=True
    ).stdout


def _ladder_at(ref: str, source_type: str) -> dict[int, str]:
    files = _git(
        "ls-tree", "-r", "--name-only", ref, f"{_PROVIDERS}/{source_type}/src/"
    )
    ladder: dict[int, str] = {}
    for path in files.split():
        if path.endswith(".rs"):
            ladder.update(rungs_in(_git("show", f"{ref}:{path}")))
    return ladder


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    previous, new = argv[1], argv[2]
    try:
        old_shapes = json.loads(_git("show", f"{previous}:{_SHAPES}"))
        new_shapes = json.loads(_git("show", f"{new}:{_SHAPES}"))
    except subprocess.CalledProcessError as e:
        raise SystemExit(f"{e.stderr.strip()}: v0.41.0 is the first tag with {_SHAPES}")
    shared = shared_rungs(
        old_shapes, new_shapes, rungs_in(_git("show", f"{new}:{_SHARED_LADDER}"))
    )
    changes = store_changes(old_shapes, new_shapes, lambda t: _ladder_at(new, t))
    prs = pull_requests(
        _git("log", "--first-parent", "--format=%s", f"{previous}..{new}").splitlines()
    )
    sys.stdout.write(render(previous, shared, changes, prs))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
