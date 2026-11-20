"""The statements that walked a whole table or index during the fixture
sync, one per line, from what `datalib_sql_work` recorded under
`DATALIB_SQL_CENSUS`. `sql_scans_test.py` checks them against
`sql_scans_allowed.toml`.

Usage: sql_scans.py <census dir> <data root> <out>
"""

import json
import re
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


def main(census: Path, root: Path, out: Path) -> int:
    seen: dict[str, set[str]] = {}
    for f in sorted(census.glob("*.jsonl")):
        for line in f.read_text().splitlines():
            if not line.strip():
                continue
            row = json.loads(line)
            seen.setdefault(normalized(row["statement"]), set()).add(
                role(row["file"], root)
            )
    lines = [
        json.dumps({"statement": s, "files": sorted(fs)})
        for s, fs in sorted(seen.items())
    ]
    out.write_text("".join(l + "\n" for l in lines))
    return 0


if __name__ == "__main__":
    sys.exit(main(Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])))
