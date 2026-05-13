"""Populate the `documents` table from the same in-memory `_Row` stream
that grid_rows is built from.

The schema (see `schemas/documents.schema.json`) lives next to grid_rows;
this module owns the producer side. The strategy is:

  1. Group the `_Row` stream by `document_uuid`.
  2. Pick a canonical "document row" per group — the row whose kind names
     the document itself (`Chat`, `Slack Thread`, `GitHub PR`, `Notion
     Page`, etc.) rather than a child message/comment row. Title +
     timestamps come from that row when present; otherwise we fall back
     to the first row in the group.
  3. Hash the canonical row tuples to get `row_set_hash`. If a future
     ingest produces a different hash for the same document, the
     renderer knows the document has changed and re-emits its `.md`.

`renderer_version` is a constant defined here. Bump it when the canonical
tuple shape changes or the renderer output layout changes — that
invalidates every cached `documents.row_set_hash` at once and forces a
global re-render on the next ingest.
"""

from __future__ import annotations

import hashlib
from collections.abc import Iterable
from dataclasses import dataclass

from pymysql.connections import Connection

from ingest.generated_documents import COLUMNS, DDL, MAX_LENGTHS
from ingest.grid_rows import _Row

# Bump this string whenever the renderer output layout or the canonical
# tuple shape (`_canonical_tuple` below) changes. Every documents.row will
# look stale on the next ingest and the renderer will re-emit its `.md`.
RENDERER_VERSION = "v1"

_DOCUMENTS_COLUMNS = COLUMNS["documents"]


def ensure_schema(conn: Connection) -> None:
    """Create the `documents` table if it doesn't exist. Idempotent."""
    with conn.cursor() as cur:
        for stmt in DDL:
            cur.execute(stmt)


# Per-provider mapping from the grid_rows `kind` of the "document row"
# (the row whose uuid equals document_uuid) to the documents.kind enum
# (`chat`, `thread`, `page`, `pr`, `mr`). Anything not in this map is a
# child row — useful for timestamp aggregation but not for naming the
# document.
_GRID_KIND_TO_DOC_KIND: dict[str, str] = {
    "Chat": "chat",
    "Slack Thread": "thread",
    "GitHub PR": "pr",
    "GitLab MR": "mr",
    "Notion Page": "page",
    "Notion Database": "page",
    "Notion Comment Thread": "thread",
}


@dataclass(slots=True)
class _DocRow:
    document_uuid: str
    source_name: str
    provider: str
    kind: str
    title: str | None
    created_at: str | None
    updated_at: str | None
    md_path: str | None
    row_set_hash: str
    renderer_version: str
    rendered_at: str | None


def _canonical_tuple(r: _Row) -> tuple:
    """The shape we hash to detect document-content drift. Includes every
    field a reader would notice (text, author, ordering, attachment links)
    but excludes per-ingest noise (qmd_path → renderer concern, not
    content)."""
    return (
        r.uuid,
        r.kind,
        r.when_ts,
        r.author,
        r.message_index,
        r.text,
        r.source_url,
        r.slack_link,
        r.git_sha,
        r.external_id,
        r.notion_page_uuid,
        r.notion_block_uuid,
    )


def compute_document_hashes(rows: Iterable[_Row]) -> dict[str, str]:
    """Group `rows` by `document_uuid` and return the SHA-256 row_set_hash
    for each group. Used by ingest.py to decide which documents still
    match the previously rendered output (skip) and which need to be
    re-rendered."""
    by_doc: dict[str, list[_Row]] = {}
    for r in rows:
        if r.document_uuid is None:
            continue
        by_doc.setdefault(r.document_uuid, []).append(r)
    return {uuid: _hash_rows(group) for uuid, group in by_doc.items()}


def fetch_existing_document_state(
    conn: Connection,
) -> dict[str, tuple[str, str, str | None]]:
    """Read `(row_set_hash, renderer_version, rendered_at)` for every row
    currently in the `documents` table. Returns an empty dict if the table
    doesn't exist yet (first ingest)."""
    ensure_schema(conn)
    with conn.cursor() as cur:
        cur.execute(
            "SELECT document_uuid, row_set_hash, renderer_version, rendered_at FROM documents"
        )
        return {uuid: (h, v, r) for (uuid, h, v, r) in cur.fetchall()}


def documents_to_skip(
    new_hashes: dict[str, str],
    existing: dict[str, tuple[str, str, str | None]],
) -> set[str]:
    """Document UUIDs whose stored `(row_set_hash, renderer_version)`
    matches `new_hashes[uuid]` paired with the current RENDERER_VERSION —
    these don't need to be re-rendered."""
    skip: set[str] = set()
    for uuid, new_hash in new_hashes.items():
        prev = existing.get(uuid)
        if prev is None:
            continue
        old_hash, old_version, _ = prev
        if old_hash == new_hash and old_version == RENDERER_VERSION:
            skip.add(uuid)
    return skip


def _hash_rows(rows: list[_Row]) -> str:
    """SHA-256 (hex) over the canonical tuples of `rows`, ordered by
    `(when_ts, uuid)` so the hash is independent of producer iteration
    order."""
    sorted_rows = sorted(rows, key=lambda r: (r.when_ts or "", r.uuid))
    h = hashlib.sha256()
    for r in sorted_rows:
        h.update(repr(_canonical_tuple(r)).encode("utf-8"))
        h.update(b"\x00")
    return h.hexdigest()


def _document_rows_from_grid_rows(
    rows: Iterable[_Row], provider_to_source_name: dict[str, str]
) -> list[_DocRow]:
    """Group `rows` by `document_uuid` and emit one `_DocRow` per group.
    Rows whose `document_uuid` is None are skipped — Phase A/B sources
    that haven't yet been wired."""
    by_doc: dict[str, list[_Row]] = {}
    for r in rows:
        if r.document_uuid is None:
            continue
        by_doc.setdefault(r.document_uuid, []).append(r)

    docs: list[_DocRow] = []
    for doc_uuid, group in by_doc.items():
        # Canonical row = the row whose `uuid` equals the document_uuid.
        # That's the chat / thread / pr / mr / page row constructed at
        # the top of each provider's _Xxx_rows() generator.
        canonical = next((r for r in group if r.uuid == doc_uuid), group[0])
        kind = _GRID_KIND_TO_DOC_KIND.get(canonical.kind, "chat")
        timestamps = [r.when_ts for r in group if r.when_ts]
        created_at = min(timestamps) if timestamps else None
        updated_at = max(timestamps) if timestamps else None
        docs.append(
            _DocRow(
                document_uuid=doc_uuid,
                source_name=provider_to_source_name.get(
                    canonical.provider, canonical.provider
                ),
                provider=canonical.provider,
                kind=kind,
                title=canonical.conversation_name,
                created_at=created_at,
                updated_at=updated_at,
                md_path=canonical.qmd_path,
                row_set_hash=_hash_rows(group),
                renderer_version=RENDERER_VERSION,
                rendered_at=None,
            )
        )
    return docs


def populate_documents(
    conn: Connection,
    rows: Iterable[_Row],
    provider_to_source_name: dict[str, str],
    rendered_at: str | None = None,
    skipped: set[str] | None = None,
) -> int:
    """Re-emit one `documents` row per distinct `document_uuid` in `rows`
    using a per-document delete+insert pattern, mirroring
    `populate_grid_rows`. Also performs per-provider orphan cleanup:
    `documents` rows for providers present in this run whose
    `document_uuid` is absent from the fresh stream are deleted. Returns
    the number of documents inserted.

    `rendered_at` is the ingest timestamp; it's stamped on every doc that
    was just re-rendered. Docs in `skipped` keep their prior `rendered_at`
    (the file on disk is still up to date — see `documents_to_skip`)."""
    ensure_schema(conn)
    docs = _document_rows_from_grid_rows(list(rows), provider_to_source_name)
    if not docs:
        return 0

    skipped = skipped or set()
    prior_rendered_at: dict[str, str | None] = {}
    if skipped:
        with conn.cursor() as cur:
            cur.execute(
                "SELECT document_uuid, rendered_at FROM documents "
                "WHERE document_uuid IN (" + ",".join(["%s"] * len(skipped)) + ")",
                tuple(skipped),
            )
            prior_rendered_at = {uuid: ts for (uuid, ts) in cur.fetchall()}

    placeholders = ",".join(["%s"] * len(_DOCUMENTS_COLUMNS))
    columns_sql = ", ".join(_DOCUMENTS_COLUMNS)
    insert_sql = f"INSERT INTO documents ({columns_sql}) VALUES ({placeholders})"
    providers_in_run = {d.provider for d in docs}
    kept = [d.document_uuid for d in docs]
    with conn.cursor() as cur:
        prov_placeholders = ",".join(["%s"] * len(providers_in_run))
        keep_placeholders = ",".join(["%s"] * len(kept))
        cur.execute(
            f"DELETE FROM documents WHERE provider IN ({prov_placeholders}) "
            f"AND document_uuid NOT IN ({keep_placeholders})",
            tuple(providers_in_run) + tuple(kept),
        )
        for d in docs:
            if d.document_uuid in skipped:
                ts = prior_rendered_at.get(d.document_uuid)
            else:
                ts = rendered_at
            cur.execute(
                "DELETE FROM documents WHERE document_uuid = %s", (d.document_uuid,)
            )
            raw = (
                d.document_uuid,
                d.source_name,
                d.provider,
                d.kind,
                d.title,
                d.created_at,
                d.updated_at,
                d.md_path,
                d.row_set_hash,
                d.renderer_version,
                ts,
            )
            cur.execute(
                insert_sql,
                tuple(
                    _truncate_for_column_doc(col, v)
                    for col, v in zip(_DOCUMENTS_COLUMNS, raw)
                ),
            )
    return len(docs)


def _truncate_for_column_doc(col: str, value):
    """Same shape as grid_rows._truncate_for_column but reads from the
    documents schema's MAX_LENGTHS map."""
    if value is None or not isinstance(value, str):
        return value
    limit = MAX_LENGTHS.get("documents", {}).get(col)
    if limit is None or len(value) <= limit:
        return value
    return value[:limit]
