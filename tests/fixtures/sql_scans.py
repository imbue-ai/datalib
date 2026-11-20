"""The statements the fixture sync ran whose plan walks a whole table or
index, one per line, from what `datalib_sql_work` recorded under
`DATALIB_SQL_CENSUS`. `sql_scans_test.py` checks them against
`sql_scans_allowed.toml`.

Each statement is judged by its plan, read from the store it ran on once the
sync is done: a full scan is a `Next` or `Prev` SQLite marks as one (its
`p5`), the same mark its run-time count of full-scan steps goes by. The plan
does not change with what the tables held, where the count does: a scan of
an empty table walks nothing. A statement whose plan cannot be read
afterwards (a temporary table, a store opened in memory, a table since
dropped) is judged by whether it scanned as it ran, and says so.

Usage: sql_scans.py <doltlite shell> <census dir> <data root> <out>
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path


def normalized(statement: str) -> str:
    """One spelling per statement: runs of `?` and of `(?, ?)` tuples, whose
    length follows the batch, come to one."""
    s = " ".join(statement.split())
    s = re.sub(r"\?(\s*,\s*\?)+", "?…", s)
    s = re.sub(r"\((\?…|\?)\)(\s*,\s*\((\?…|\?)\))+", "(?…)…", s)
    return s


def role(file: str, root: Path) -> str:
    """Where a store sits, without its group: `*/render_markdown/…`."""
    if not file:
        return "(memory)"
    path = Path(file)
    for base in (root, root.resolve()):
        try:
            parts = path.relative_to(base).parts
        except ValueError:
            continue
        if len(parts) > 1:
            return "/".join(["*", *parts[1:]])
        return parts[0] if parts else path.name
    return path.name


def plan_query(i: int, statement: str) -> str:
    quoted = statement.replace("'", "''")
    return (
        f"SELECT {i}, count(*) FROM bytecode('{quoted}') "
        "WHERE opcode IN ('Next', 'Prev') AND p5 = 1;\n"
    )


def read_plans(output: str) -> dict[int, bool]:
    """`i|n` per statement the shell could plan; a statement it could not
    prepare printed an error instead, and is absent."""
    plans = {}
    for line in output.splitlines():
        i, sep, n = line.partition("|")
        if sep and i.isdigit() and n.isdigit():
            plans[int(i)] = int(n) > 0
    return plans


def store_path(file: str) -> Path | None:
    """The file a connection named, without a doltlite `@<revision>`."""
    if not file or file == ":memory:":
        return None
    path = Path(re.sub(r"@[^/]*$", "", file))
    return path if path.is_file() else None


def plans_in(doltlite: Path, store: Path, statements: list[str]) -> dict[int, bool]:
    script = "".join(plan_query(i, s) for i, s in enumerate(statements))
    done = subprocess.run(
        [str(doltlite), "-readonly", "-list", "-separator", "|", str(store)],
        input=script,
        capture_output=True,
        text=True,
        check=False,
    )
    return read_plans(done.stdout)


def judge(
    ran: dict[tuple[str, str], bool], plans: dict[tuple[str, str], bool]
) -> dict[tuple[str, str], tuple[bool, str]]:
    """Whether each (statement, file) scans, and how that was decided."""
    return {
        key: (plans[key], "plan") if key in plans else (scanned, "ran")
        for key, scanned in ran.items()
    }


def main(doltlite: Path, census: Path, root: Path, out: Path) -> int:
    ran: dict[tuple[str, str], bool] = {}
    for f in sorted(census.glob("*.jsonl")):
        for line in f.read_text().splitlines():
            if line.strip():
                row = json.loads(line)
                key = (row["statement"], row["file"])
                ran[key] = ran.get(key, False) or bool(row.get("scanned"))
    plans: dict[tuple[str, str], bool] = {}
    by_file: dict[str, list[str]] = {}
    for statement, file in ran:
        by_file.setdefault(file, []).append(statement)
    for file, statements in sorted(by_file.items()):
        store = store_path(file)
        if store is None:
            continue
        for i, scans in plans_in(doltlite, store, statements).items():
            plans[(statements[i], file)] = scans
    scanning: dict[str, tuple[set[str], set[str]]] = {}
    for (statement, file), (scans, how) in judge(ran, plans).items():
        if scans:
            files, hows = scanning.setdefault(normalized(statement), (set(), set()))
            files.add(role(file, root))
            hows.add(how)
    lines = [
        json.dumps({"statement": s, "files": sorted(fs), "judged_by": sorted(hs)})
        for s, (fs, hs) in sorted(scanning.items())
    ]
    out.write_text("".join(line + "\n" for line in lines))
    print(
        f"{len(ran)} statements recorded, {len(plans)} judged by their plan, "
        f"{len(lines)} scan",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    a = sys.argv
    sys.exit(main(Path(a[1]), Path(a[2]), Path(a[3]), Path(a[4])))
