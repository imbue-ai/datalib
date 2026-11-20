"""Every statement the TNG fixture sync runs that walks a whole table or
index is allowed by `sql_scans_allowed.toml`, with a reason. A fixture's tables are a few rows each, so a scan costs
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
TABLE = re.compile(r"\b(?:FROM|JOIN)\s+\"?([A-Za-z_][\w.]*)\"?", re.IGNORECASE)


def reads_only_the_catalog(statement: str) -> bool:
    tables = TABLE.findall(statement)
    return bool(tables) and all(CATALOG.match(t.split(".")[-1]) for t in tables)


def stanza(statement: str, files: list[str]) -> str:
    quoted = json.dumps(statement, ensure_ascii=False)
    return f'[[scan]]\nstatement = {quoted}\nfiles = {json.dumps(files)}\nwhy = ""\n'


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
        why_of[statement] = why
    seen: set[str] = set()
    for row in recorded:
        statement = str(row["statement"])
        files = row["files"]
        assert isinstance(files, list)
        seen.add(statement)
        if statement in why_of or reads_only_the_catalog(statement):
            continue
        errors.append(
            "a statement walks a whole table or index and the allowlist does not name it.\n"
            "Give it an index, or add this to tests/fixtures/sql_scans_allowed.toml "
            "with a reason:\n\n" + stanza(statement, [str(f) for f in files])
        )
    # A scan of an empty table counts no steps, so whether a statement shows
    # can depend on the order the sync's steps ran: an entry missing from one
    # run is a note, not a failure.
    for statement in sorted(set(why_of) - seen):
        print(
            f"note: the allowlist names a statement this run did not record:\n{statement}\n"
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
    errors = check(recorded, tomllib.loads(allowed_path.read_text()))
    for error in errors:
        print(error, end="\n\n")
    print(f"{len(recorded)} scanning statements, {len(errors)} problems")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(Path(sys.argv[1]), Path(sys.argv[2])))
