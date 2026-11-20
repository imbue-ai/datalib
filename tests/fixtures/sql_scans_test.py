"""Every statement the TNG fixture sync runs that walks a whole table or
index is allowed by `sql_scans_allowed.toml`, with a reason, and every entry
there names a statement that still scans. A fixture's tables are a few rows
each, so a scan costs nothing here: this checks the plan, which a real
library pays for.

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


def stanza(statement: str, files: list[str], judged_by: list[str]) -> str:
    quoted = json.dumps(statement, ensure_ascii=False)
    ran_only = 'judged_by = "ran"\n' if judged_by == ["ran"] else ""
    return (
        f"[[scan]]\nstatement = {quoted}\nfiles = {json.dumps(files)}\n"
        f'{ran_only}why = ""\n'
    )


def check(recorded: list[dict[str, object]], allowed: dict[str, object]) -> list[str]:
    entries = allowed.get("scan", [])
    assert isinstance(entries, list)
    why_of: dict[str, str] = {}
    judged_by_ran: set[str] = set()
    errors: list[str] = []
    for entry in entries:
        assert isinstance(entry, dict)
        statement, why = str(entry["statement"]), str(entry.get("why", "")).strip()
        if not why:
            errors.append(f"an entry gives no reason: {statement}")
        if statement in why_of:
            errors.append(f"two entries name one statement: {statement}")
        why_of[statement] = why
        if entry.get("judged_by") == "ran":
            judged_by_ran.add(statement)
    seen: set[str] = set()
    for row in recorded:
        statement = str(row["statement"])
        files = row["files"]
        judged_by = row.get("judged_by", [])
        assert isinstance(files, list) and isinstance(judged_by, list)
        seen.add(statement)
        if statement in why_of or reads_only_the_catalog(statement):
            continue
        errors.append(
            "a statement walks a whole table or index and the allowlist does not name it.\n"
            "Give it an index, or add this to tests/fixtures/sql_scans_allowed.toml "
            "with a reason:\n\n"
            + stanza(statement, [str(f) for f in files], [str(j) for j in judged_by])
        )
    # A statement judged by its plan scans in every run or in none. One whose
    # plan cannot be read afterwards counts only when it walked rows, which
    # can depend on the order the steps ran in; its entry says so.
    for statement in sorted(set(why_of) - seen):
        if statement in judged_by_ran:
            print(f"note: this run did not see this statement scan:\n{statement}\n")
            continue
        errors.append(
            "the allowlist names a statement this run did not record scanning. "
            "Its plan no longer scans, its text changed, or nothing runs it: "
            "delete its entry from tests/fixtures/sql_scans_allowed.toml.\n\n"
            + statement
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
