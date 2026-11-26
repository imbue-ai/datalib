"""Every statement the TNG fixture sync runs that walks a whole table or
index is allowed by `sql_scans_allowed.toml`, with a reason, and every entry there
is still needed. And every statement it runs carries its name
(`datalib/backend/sql`), but the few SQLite and doltlite run on their own,
which the same file lists by shape. A fixture's tables are a few rows each, so a scan costs
nothing here: this checks the plan, which a real library pays for.

Usage: sql_scans_test.py <ingested/sql_scans.jsonl> <sql_scans_allowed.toml>
"""

import json
import re
import sys
import tomllib
from pathlib import Path

# The catalog and SQLite's table functions over it hold a few rows per table
# in any store, so a statement that reads nothing else is allowed by rule.
CATALOG = re.compile(
    r"^(sqlite_master|sqlite_schema|sqlite_temp_master|pragma_\w+)$", re.IGNORECASE
)
TABLE = re.compile(
    r"""\b(?:FROM|JOIN)\s+((?:["']?\w+["']?\.)?["']?[A-Za-z_]\w*["']?)""", re.IGNORECASE
)


def reads_only_the_catalog(statement: str) -> bool:
    tables = [re.sub(r"[\"']", "", t) for t in TABLE.findall(statement)]
    return bool(tables) and all(CATALOG.match(t.split(".")[-1]) for t in tables)


def stanza(statement: str, files: list[str]) -> str:
    quoted = json.dumps(statement, ensure_ascii=False)
    return f'[[scan]]\nstatement = {quoted}\nfiles = {json.dumps(files)}\nwhy = ""\n'


# How a statement Datalib wrote opens (`datalib_sql::tag`).
NAMED = re.compile(r"^/\* sql:\S+ \*/ ")


def unnamed_problems(
    recorded: list[dict[str, object]], allowed: dict[str, object]
) -> list[str]:
    """A statement that ran with no name, unless the allowlist says SQLite
    or doltlite runs it on its own; and an entry nothing matches any more."""
    entries = allowed.get("unnamed", [])
    assert isinstance(entries, list)
    used = [False] * len(entries)
    errors = []
    for e in entries:
        assert isinstance(e, dict)
        if not str(e.get("why", "")).strip():
            errors.append(f"an unnamed entry gives no reason: {e['like']}")
    for row in recorded:
        statement = str(row["statement"])
        if NAMED.match(statement):
            continue
        hits = [
            i for i, e in enumerate(entries) if re.fullmatch(str(e["like"]), statement)
        ]
        for i in hits:
            used[i] = True
        if not hits:
            errors.append(
                "a statement ran without a name. Name it where it is written, "
                '`sql!("<package>.<what_it_does>", …)` (datalib/backend/sql); if SQLite '
                "or doltlite runs it on its own, add its shape to "
                "tests/fixtures/sql_scans_allowed.toml as an [[unnamed]] entry with a "
                f"reason:\n{statement}"
            )
    for e, u in zip(entries, used, strict=True):
        if not u:
            errors.append(
                "an [[unnamed]] entry matches nothing the sync runs any more. Take it "
                f"out of tests/fixtures/sql_scans_allowed.toml:\n{e['like']}"
            )
    return errors


def check(recorded: list[dict[str, object]], allowed: dict[str, object]) -> list[str]:
    entries = allowed.get("scan", [])
    assert isinstance(entries, list)
    why_of: dict[str, str] = {}
    errors: list[str] = []
    for entry in entries:
        assert isinstance(entry, dict)
        statement, why = str(entry["statement"]), str(entry.get("why", "")).strip()
        if not why:
            errors.append(f"an entry gives no reason: {statement}")
        if statement in why_of:
            errors.append(f"two entries name one statement: {statement}")
        why_of[statement] = why
    ran: dict[str, dict[str, object]] = {}
    for row in recorded:
        statement = str(row["statement"])
        files = row["files"]
        assert isinstance(files, list)
        ran[statement] = row
        if not row["scans"] or statement in why_of or reads_only_the_catalog(statement):
            continue
        errors.append(
            "a statement walks a whole table or index and the allowlist does not name it.\n"
            "Give it an index, or add this to tests/fixtures/sql_scans_allowed.toml "
            "with a reason:\n\n" + stanza(statement, [str(f) for f in files])
        )
    # An entry nothing needs any more goes, or it waves through the scan
    # its statement might grow back. The one exception is a statement judged
    # by the rows it walked (its plan could not be read afterwards), which
    # walks none when its table was empty.
    for statement in sorted(why_of):
        row = ran.get(statement)
        if row is None:
            errors.append(
                "the allowlist names a statement the sync no longer runs. "
                f"Take its entry out of tests/fixtures/sql_scans_allowed.toml:\n{statement}"
            )
            continue
        judged_by = row["judged_by"]
        assert isinstance(judged_by, list)
        if row["scans"]:
            continue
        if "ran" in judged_by:
            print(
                f"note: this allowed statement walked no rows this run:\n{statement}\n"
            )
        else:
            errors.append(
                "the allowlist names a statement whose plan no longer walks a whole "
                "table or index. Take its entry out of "
                f"tests/fixtures/sql_scans_allowed.toml:\n{statement}"
            )
    return errors


def main(recorded_path: Path, allowed_path: Path) -> int:
    recorded = [
        json.loads(line)
        for line in recorded_path.read_text().splitlines()
        if line.strip()
    ]
    if not recorded:
        print("the census recorded nothing: is DATALIB_SQL_CENSUS reaching the sync?")
        return 1
    allowed = tomllib.loads(allowed_path.read_text())
    errors = check(recorded, allowed) + unnamed_problems(recorded, allowed)
    for error in errors:
        print(error, end="\n\n")
    scanning = sum(1 for row in recorded if row["scans"])
    print(f"{len(recorded)} statements, {scanning} scanning, {len(errors)} problems")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(Path(sys.argv[1]), Path(sys.argv[2])))
