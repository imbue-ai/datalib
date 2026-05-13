"""Unit test for per-provider orphan cleanup in populate_grid_rows and
populate_documents (C.6).

Uses a thin pymysql-shape shim over sqlite3 so we don't need to spin up
a Dolt server for an SQL-level regression test. The shim only has to
translate `%s` -> `?` and support `cursor()` / `execute` / `executemany`
because that's all the populate_* code relies on.
"""

from __future__ import annotations

import sqlite3
from contextlib import contextmanager

from ingest.documents import populate_documents
from ingest.grid_rows import _Row, populate_grid_rows


class _Cursor:
    def __init__(self, raw: sqlite3.Cursor) -> None:
        self._raw = raw

    def execute(self, sql: str, params: tuple = ()) -> None:
        self._raw.execute(sql.replace("%s", "?"), params)

    def executemany(self, sql: str, seq) -> None:
        self._raw.executemany(sql.replace("%s", "?"), seq)

    def fetchall(self):
        return self._raw.fetchall()

    def fetchone(self):
        return self._raw.fetchone()


class _Conn:
    """Mimics pymysql.Connection's `cursor()` context-manager surface
    against a plain sqlite3 connection."""

    def __init__(self, db: sqlite3.Connection) -> None:
        self.db = db

    @contextmanager
    def cursor(self):
        cur = self.db.cursor()
        try:
            yield _Cursor(cur)
        finally:
            cur.close()

    def autocommit(self, _flag: bool) -> None:  # noqa: D401
        pass

    def commit(self) -> None:
        self.db.commit()


def _mkrow(doc_uuid: str, provider: str, uuid: str | None = None) -> _Row:
    return _Row(
        uuid=uuid or doc_uuid,
        provider=provider,
        kind="Chat",
        source_label=provider.title(),
        when_ts="2026-01-01T00:00:00+00:00",
        author=None,
        account=None,
        project=None,
        channel=None,
        conversation_name="t",
        conversation_uuid=doc_uuid,
        message_index=None,
        entire_chat=f"/chat/{doc_uuid}",
        text="",
        slack_link=None,
        qmd_path=f"rendered_md/{provider}/{doc_uuid}.md",
        document_uuid=doc_uuid,
    )


def test_orphan_cleanup_removes_in_run_provider_docs_but_spares_others() -> None:
    conn = _Conn(sqlite3.connect(":memory:"))

    # Seed: two anthropic docs, two openai docs.
    rows = [
        _mkrow("a-1", "anthropic"),
        _mkrow("a-2", "anthropic"),
        _mkrow("o-1", "openai"),
        _mkrow("o-2", "openai"),
    ]
    populate_grid_rows(conn, None, None, None, rows=rows)  # type: ignore[arg-type]
    populate_documents(conn, rows, {"anthropic": "claude", "openai": "chatgpt"})  # type: ignore[arg-type]

    # Second run drops anthropic a-2 but does not touch openai at all.
    rows2 = [_mkrow("a-1", "anthropic")]
    populate_grid_rows(conn, None, None, None, rows=rows2)  # type: ignore[arg-type]
    populate_documents(conn, rows2, {"anthropic": "claude"})  # type: ignore[arg-type]

    with conn.cursor() as cur:
        cur.execute("SELECT document_uuid, provider FROM grid_rows ORDER BY uuid")
        gr = cur.fetchall()
        cur.execute(
            "SELECT document_uuid, provider FROM documents ORDER BY document_uuid"
        )
        docs = cur.fetchall()

    # a-2 was orphan-cleaned (anthropic was in the run). o-1/o-2 untouched.
    assert sorted(gr) == sorted(
        [("a-1", "anthropic"), ("o-1", "openai"), ("o-2", "openai")]
    )
    assert sorted(docs) == sorted(
        [("a-1", "anthropic"), ("o-1", "openai"), ("o-2", "openai")]
    )
