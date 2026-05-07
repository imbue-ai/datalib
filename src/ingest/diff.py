"""Human-readable diff between two Dolt commits.

Dolt does the actual row-level diff via the `dolt_commit_diff_<table>`
system table. We just walk every known table, classify rows as
added / modified / removed, identify which columns changed for the
modified ones, and render a textual report.

Cosmetic columns (`raw_json`, `first_seen_at`, `last_seen_at`) are
ignored when deciding whether a row was "really" modified -- a row
whose only changes are in those columns counts as cosmetic-only and
is reported separately. See AGENTS.md for the rationale.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from pymysql.connections import Connection

from ingest.dump import _TABLES, _columns, _primary_key, _table_exists

# Columns whose changes are noise -- bookkeeping fields that the ingest
# updates on every run regardless of whether the underlying record
# actually changed.
COSMETIC_COLUMNS: frozenset[str] = frozenset({"raw_json", "first_seen_at", "last_seen_at"})


@dataclass
class RowDiff:
    pk: tuple[Any, ...]
    diff_type: str  # 'added' | 'modified' | 'removed'
    # Column -> (from_value, to_value). For 'added' rows, from_value is
    # always None; for 'removed', to_value is always None.
    changed: dict[str, tuple[Any, Any]] = field(default_factory=dict)


@dataclass
class TableDiff:
    table: str
    added: list[RowDiff] = field(default_factory=list)
    modified: list[RowDiff] = field(default_factory=list)
    removed: list[RowDiff] = field(default_factory=list)
    cosmetic_only: list[RowDiff] = field(default_factory=list)

    @property
    def is_empty(self) -> bool:
        return not (self.added or self.modified or self.removed or self.cosmetic_only)


@dataclass
class DiffReport:
    from_ref: str
    to_ref: str
    tables: list[TableDiff] = field(default_factory=list)


def diff_commits(conn: Connection, from_ref: str, to_ref: str) -> DiffReport:
    """Build a `DiffReport` between two Dolt commit refs.

    `from_ref` and `to_ref` are passed through to Dolt's
    `dolt_commit_diff_<table>` system table -- anything Dolt accepts as
    a rev-spec works (commit hash, `HEAD`, `HEAD~1`, branch name).
    """
    report = DiffReport(from_ref=from_ref, to_ref=to_ref)
    for table in _TABLES:
        if not _table_exists(conn, table):
            continue
        report.tables.append(_diff_table(conn, table, from_ref, to_ref))
    return report


def _diff_table(conn: Connection, table: str, from_ref: str, to_ref: str) -> TableDiff:
    out = TableDiff(table=table)
    cols = _columns(conn, table)
    pk_cols = _primary_key(conn, table) or cols

    # `dolt_commit_diff_<table>` exposes `to_<col>` / `from_<col>` for
    # every column plus the `diff_type` discriminator. We project
    # exactly the shape we need.
    select_parts = ["diff_type"]
    for c in cols:
        select_parts.append(f"`to_{c}` AS `to__{c}`")
        select_parts.append(f"`from_{c}` AS `from__{c}`")
    sql = (
        f"SELECT {', '.join(select_parts)} "
        f"FROM `dolt_commit_diff_{table}` "
        f"WHERE from_commit = %s AND to_commit = %s"
    )
    with conn.cursor() as cur:
        cur.execute(sql, (from_ref, to_ref))
        rows = cur.fetchall()

    for row in rows:
        # row is a tuple in column-order: diff_type, to__c1, from__c1, to__c2, from__c2, ...
        diff_type = row[0]
        to_vals: dict[str, Any] = {}
        from_vals: dict[str, Any] = {}
        for i, c in enumerate(cols):
            to_vals[c] = row[1 + 2 * i]
            from_vals[c] = row[1 + 2 * i + 1]

        if diff_type == "added":
            pk = tuple(to_vals[c] for c in pk_cols)
            out.added.append(RowDiff(pk=pk, diff_type="added"))
        elif diff_type == "removed":
            pk = tuple(from_vals[c] for c in pk_cols)
            out.removed.append(RowDiff(pk=pk, diff_type="removed"))
        elif diff_type == "modified":
            pk = tuple(to_vals[c] for c in pk_cols)
            changed: dict[str, tuple[Any, Any]] = {}
            for c in cols:
                if from_vals[c] != to_vals[c]:
                    changed[c] = (from_vals[c], to_vals[c])
            rd = RowDiff(pk=pk, diff_type="modified", changed=changed)
            non_cosmetic = {c for c in changed if c not in COSMETIC_COLUMNS}
            if non_cosmetic:
                out.modified.append(rd)
            else:
                out.cosmetic_only.append(rd)
        # Unknown diff_type values fall through silently -- shouldn't happen.
    return out


def format_report(report: DiffReport, max_samples: int = 3, max_value_len: int = 80) -> str:
    """Render a `DiffReport` as plain text suitable for terminals."""
    lines: list[str] = []
    lines.append(f"Diff: {report.from_ref} → {report.to_ref}")

    total_added = sum(len(t.added) for t in report.tables)
    total_modified = sum(len(t.modified) for t in report.tables)
    total_removed = sum(len(t.removed) for t in report.tables)
    total_cosmetic = sum(len(t.cosmetic_only) for t in report.tables)
    lines.append(
        f"  +{total_added} added  ~{total_modified} modified  "
        f"-{total_removed} removed  ({total_cosmetic} cosmetic-only)"
    )
    lines.append("")

    for tdiff in report.tables:
        if tdiff.is_empty:
            continue
        lines.append(f"== {tdiff.table} ==")
        lines.append(
            f"  +{len(tdiff.added)} added  "
            f"~{len(tdiff.modified)} modified  "
            f"-{len(tdiff.removed)} removed  "
            f"({len(tdiff.cosmetic_only)} cosmetic-only)"
        )

        if tdiff.modified:
            col_changes: dict[str, int] = {}
            for rd in tdiff.modified:
                for c in rd.changed:
                    if c in COSMETIC_COLUMNS:
                        continue
                    col_changes[c] = col_changes.get(c, 0) + 1
            n = len(tdiff.modified)
            lines.append("  Modified columns:")
            for c, count in sorted(col_changes.items(), key=lambda kv: (-kv[1], kv[0])):
                lines.append(f"    {c}: {count}/{n}")

            shown = min(max_samples, len(tdiff.modified))
            if shown:
                more = len(tdiff.modified) - shown
                tail = f" (and {more} more)" if more > 0 else ""
                lines.append(f"  Sample modifications (showing {shown} of {len(tdiff.modified)}{tail}):")
                for rd in tdiff.modified[:shown]:
                    pk_str = ", ".join(repr(p) for p in rd.pk)
                    lines.append(f"    [{pk_str}]")
                    for col, (old, new) in rd.changed.items():
                        if col in COSMETIC_COLUMNS:
                            continue
                        lines.append(
                            f"      {col}: {_truncate(old, max_value_len)} "
                            f"→ {_truncate(new, max_value_len)}"
                        )

        if tdiff.added and max_samples > 0:
            shown = min(max_samples, len(tdiff.added))
            more = len(tdiff.added) - shown
            tail = f" (and {more} more)" if more > 0 else ""
            lines.append(f"  Sample inserts (showing {shown} of {len(tdiff.added)}{tail}):")
            for rd in tdiff.added[:shown]:
                pk_str = ", ".join(repr(p) for p in rd.pk)
                lines.append(f"    [{pk_str}]")

        if tdiff.removed and max_samples > 0:
            shown = min(max_samples, len(tdiff.removed))
            more = len(tdiff.removed) - shown
            tail = f" (and {more} more)" if more > 0 else ""
            lines.append(f"  Sample removals (showing {shown} of {len(tdiff.removed)}{tail}):")
            for rd in tdiff.removed[:shown]:
                pk_str = ", ".join(repr(p) for p in rd.pk)
                lines.append(f"    [{pk_str}]")

        lines.append("")

    if all(t.is_empty for t in report.tables):
        lines.append("(no changes)")

    return "\n".join(lines).rstrip() + "\n"


def _truncate(v: Any, n: int) -> str:
    s = repr(v)
    if len(s) > n:
        return s[: n - 1] + "…"
    return s
