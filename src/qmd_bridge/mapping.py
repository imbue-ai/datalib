"""Map qmd search results to grid_rows, and back.

The render pipeline emits one `.qmd` file per conversation (LLM chats,
Slack threads) or per PR/MR (with sub-files per discussion thread under
`threads/`). Inside each file, every individual message is wrapped in
`<div id="m-{uuid}" ...>` where `{uuid}` is the same value used as
`grid_rows.uuid` for message-level rows.

That gives us two ways to resolve a qmd hit to grid rows, applied in
this order (the "strict" mapping the prototype uses):

  1. **By embedded uuid.** Pull every `m-{uuid}` out of the snippet. Any
     uuid that exists as a `grid_rows.uuid` resolves to that specific
     row. This is the precise case — a PR thread chunk maps to exactly
     the comment rows that appear in the snippet.

  2. **By file path.** If no `m-{uuid}` in the snippet matches a known
     row, fall back to every row whose `grid_rows.qmd_path` equals the
     hit's file path. This is the conversation/container case — an LLM
     chat or Slack thread is a single grid row, and the inner `m-`
     divs (when present) name *messages*, not rows.

There's one wrinkle in path matching: qmd lowercases paths and collapses
runs of `_`/`-` to a single hyphen when forming its internal docid URI.
So `pr-42__recalibrate-...` on disk shows up as `pr-42-recalibrate-...`
in `qmd://mirror/...`. `_norm_path` reproduces that normalization on the
grid side so comparisons are apples-to-apples.

Reverse direction (`hits_for_row`) is the same logic run in reverse:
filter all hits to those whose normalized path matches the row's, then
intersect with the row's uuid if the row is message-level.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Iterable, Literal

QueryMode = Literal["query", "vsearch"]

# An `m-{uuid}` token inside a rendered <div id="..."> wrapper.
_M_UUID_RE = re.compile(
    r"m-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})"
)

# qmd:"text"  /  qmd_vsearch:"text"  /  bare text (defaults to query mode).
_PREDICATE_RE = re.compile(r'^\s*(qmd|qmd_vsearch)\s*:\s*"(.*)"\s*$', re.DOTALL)


@dataclass(frozen=True)
class QmdHit:
    """One result from a qmd search.

    `path` is the path qmd reports inside its `qmd://<collection>/...`
    URI — already collection-stripped and normalized (lowercased,
    `[_-]+` collapsed to `-`). Compare against `_norm_path(row.qmd_path)`.
    """

    path: str
    score: float
    snippet: str
    docid: str = ""
    title: str = ""


@dataclass(frozen=True)
class GridRowRef:
    """The bits of a grid row needed for hit↔row mapping."""

    uuid: str
    kind: str
    qmd_path: str
    provider: str


def _norm_path(p: str) -> str:
    """Match qmd's path normalization: lowercase + collapse `[_-]+` -> `-`."""
    return re.sub(r"[_-]+", "-", p.lower())


def parse_query(raw: str) -> tuple[QueryMode, str]:
    """Parse a search-bar string into `(mode, inner_query)`.

    - `qmd:"foo"` → `("query", "foo")`
    - `qmd_vsearch:"foo"` → `("vsearch", "foo")`
    - anything else → `("query", raw.strip())` (bare text defaults to
      the hybrid query mode).
    """
    m = _PREDICATE_RE.match(raw)
    if not m:
        return ("query", raw.strip())
    return ("vsearch" if m.group(1) == "qmd_vsearch" else "query", m.group(2))


class GridIndex:
    """Indexes a set of `GridRowRef`s for fast hit→rows / row→hits lookup."""

    def __init__(self, rows: Iterable[GridRowRef]) -> None:
        rows = list(rows)
        self.by_uuid: dict[str, GridRowRef] = {r.uuid: r for r in rows}
        self.by_norm_path: dict[str, list[GridRowRef]] = {}
        for r in rows:
            self.by_norm_path.setdefault(_norm_path(r.qmd_path), []).append(r)

    @classmethod
    def from_sqlite(cls, conn) -> GridIndex:
        """Build from an in-memory sqlite3.Connection (see `sqlite_load`)."""
        rows = [
            GridRowRef(
                uuid=r["uuid"],
                kind=r["kind"],
                qmd_path=r["qmd_path"],
                provider=r["provider"],
            )
            for r in conn.execute(
                "SELECT uuid, kind, qmd_path, provider FROM grid_rows"
            ).fetchall()
        ]
        return cls(rows)

    def rows_for_hit(self, hit: QmdHit) -> list[GridRowRef]:
        """Resolve a single hit to grid rows using strict semantics.

        Returns rows in the order they appear in the snippet (uuid-match
        case) or in arbitrary stable order (path-fallback case). Dedupes.
        """
        seen: set[str] = set()
        out: list[GridRowRef] = []
        for u in _M_UUID_RE.findall(hit.snippet):
            row = self.by_uuid.get(u)
            if row and row.uuid not in seen:
                seen.add(row.uuid)
                out.append(row)
        if out:
            return out
        return list(self.by_norm_path.get(_norm_path(hit.path), []))

    def rows_for_hits(self, hits: Iterable[QmdHit]) -> list[GridRowRef]:
        """Aggregate over hits, preserve rank order, dedupe by uuid."""
        seen: set[str] = set()
        out: list[GridRowRef] = []
        for h in hits:
            for r in self.rows_for_hit(h):
                if r.uuid not in seen:
                    seen.add(r.uuid)
                    out.append(r)
        return out

    def hits_for_row(self, row: GridRowRef, hits: Iterable[QmdHit]) -> list[QmdHit]:
        """Reverse: which of `hits` mention `row`?

        A hit mentions the row when:
          - the hit's path matches `row.qmd_path` (after normalization), AND
          - either the snippet has no parseable `m-{uuid}` ids (so the
            file-level fallback applies), or the row's uuid appears
            among the snippet's `m-` ids.
        """
        target = _norm_path(row.qmd_path)
        out: list[QmdHit] = []
        for h in hits:
            if _norm_path(h.path) != target:
                continue
            uuids = set(_M_UUID_RE.findall(h.snippet))
            if not uuids or row.uuid in uuids:
                out.append(h)
        return out
