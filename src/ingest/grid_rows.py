"""Populate the `grid_rows` union table from the per-provider tables.

`grid_rows` is the denormalized projection that backs the AG Grid in
frankweiler — one row per displayable entity (chat conversation, message,
content block, slack message, ...), keyed by a provider-namespaced UUID.
Per-provider tables (`anthropic_*`, `openai_*`, `slack_*`) remain the
authoritative store for raw payloads + render input; this module reads
those and emits the union.

Schema (column names, types, per-provider mappings) lives in
`schemas/grid_rows.schema.json` — codegen produces matching Python /
Rust / TypeScript artifacts. See `docs/grid_rows.md` for the
architecture overview.

Re-population strategy: full delete + reinsert per provider on every
ingest. Cheap at our scale (~5k rows), avoids row-level UPSERT
complexity, and guarantees consistency with any mapping changes.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from datetime import datetime, timedelta
from typing import Iterable

from pymysql.connections import Connection

from ingest.generated_grid_rows import COLUMNS, DDL


_GRID_ROWS_COLUMNS = COLUMNS["grid_rows"]


def ensure_schema(conn: Connection) -> None:
    """(Re)create the grid_rows table. Drops first so schema changes
    (new columns, retyped columns) take effect even when the underlying
    Dolt repo persists between ingest runs — grid_rows is fully derived
    from the per-provider tables, so dropping is always safe."""
    with conn.cursor() as cur:
        cur.execute("DROP TABLE IF EXISTS grid_rows")
        for stmt in DDL:
            cur.execute(stmt)


# ----- helpers --------------------------------------------------------------


@dataclass(slots=True)
class _Row:
    uuid: str
    provider: str
    kind: str
    source_label: str
    when_ts: str
    author: str | None
    account: str | None
    project: str | None
    channel: str | None
    conversation_name: str | None
    conversation_uuid: str
    message_index: int | None
    entire_chat: str
    text: str
    slack_link: str | None


def _bump_micros(ts: str, n: int) -> str:
    """Add `n` microseconds to an ISO-8601 timestamp string, preserving
    the explicit offset suffix. Falls back to returning the input
    unchanged if the format isn't recognized — synthetic ordering is
    best-effort, matching the Rust `bump_micros` in db.rs."""
    if not ts:
        return ts
    # Accept both '+00:00'/'-07:00' and trailing 'Z' forms.
    s = ts.replace("Z", "+00:00") if ts.endswith("Z") else ts
    try:
        dt = datetime.fromisoformat(s)
    except ValueError:
        return ts
    bumped = dt + timedelta(microseconds=n)
    # Match the export format ("...+00:00") rather than the default "...Z".
    return bumped.isoformat(timespec="microseconds")


def _anthropic_kind_for_sender(sender: str) -> str:
    s = (sender or "").lower()
    if s in ("human", "user"):
        return "User Input"
    if s == "assistant":
        return "LLM Response"
    return "Tool Call"


def _anthropic_kind_for_block(block_type: str) -> str:
    return "LLM Thinking" if block_type == "thinking" else "Tool Call"


def _openai_kind_for_role_and_type(role: str, content_type: str) -> str:
    r = (role or "").lower()
    if r == "user":
        return "User Input"
    if r == "assistant":
        if content_type in ("thoughts", "reasoning_recap"):
            return "LLM Thinking"
        return "LLM Response"
    return "Tool Call"


_MODEL_RE = re.compile(r'"model"\s*:\s*"([^"]+)"')


def _extract_model_from_raw(raw_json: str | None) -> str:
    """Pull `raw_json.model` out of an Anthropic conversation. Cheap regex
    rather than json.loads since raw_json can be large and we only want
    one shallow field."""
    if not raw_json:
        return ""
    m = _MODEL_RE.search(raw_json)
    return m.group(1) if m else ""


# ----- row builders ---------------------------------------------------------


def _anthropic_chat_rows(conn: Connection) -> Iterable[_Row]:
    with conn.cursor() as cur:
        cur.execute(
            """
            SELECT conversation_uuid, account_uuid, project_uuid, name, summary,
                   COALESCE(created_at, updated_at) AS when_ts
            FROM anthropic_conversations
            """
        )
        for cuuid, account, project, name, summary, when in cur.fetchall():
            text = summary or name or ""
            yield _Row(
                uuid=cuuid,
                provider="anthropic",
                kind="Chat",
                source_label="Claude",
                when_ts=when or "",
                author=None,
                account=account,
                project=project,
                channel=None,
                conversation_name=name,
                conversation_uuid=cuuid,
                message_index=None,
                entire_chat=f"/chat/{cuuid}",
                text=text,
                slack_link=None,
            )


def _anthropic_message_rows(conn: Connection) -> Iterable[_Row]:
    with conn.cursor() as cur:
        cur.execute(
            """
            WITH m AS (
                SELECT message_uuid, conversation_uuid, sender, text, created_at,
                       ROW_NUMBER() OVER (PARTITION BY conversation_uuid
                                          ORDER BY created_at, message_uuid) - 1 AS msg_idx
                FROM anthropic_messages
            )
            SELECT m.message_uuid, m.conversation_uuid, m.sender, m.text, m.created_at,
                   c.account_uuid, c.project_uuid, c.raw_json, m.msg_idx, c.name
            FROM m JOIN anthropic_conversations c
                 ON m.conversation_uuid = c.conversation_uuid
            """
        )
        for (
            mid,
            cuuid,
            sender,
            text,
            when,
            account,
            project,
            raw_json,
            msg_idx,
            cname,
        ) in cur.fetchall():
            kind = _anthropic_kind_for_sender(sender or "")
            model = _extract_model_from_raw(raw_json)
            if kind == "User Input":
                author = account
            elif kind == "LLM Response":
                author = model or sender
            else:
                author = sender
            yield _Row(
                uuid=mid,
                provider="anthropic",
                kind=kind,
                source_label="Claude",
                when_ts=when or "",
                author=author,
                account=account,
                project=project,
                channel=None,
                conversation_name=cname,
                conversation_uuid=cuuid,
                message_index=int(msg_idx),
                entire_chat=f"/chat/{cuuid}",
                text=text or "",
                slack_link=None,
            )


def _anthropic_block_rows(conn: Connection) -> Iterable[_Row]:
    with conn.cursor() as cur:
        cur.execute(
            """
            WITH m AS (
                SELECT message_uuid, conversation_uuid, created_at,
                       ROW_NUMBER() OVER (PARTITION BY conversation_uuid
                                          ORDER BY created_at, message_uuid) - 1 AS msg_idx
                FROM anthropic_messages
            )
            SELECT b.message_uuid, m.conversation_uuid, b.type,
                   COALESCE(NULLIF(b.text, ''),
                            JSON_UNQUOTE(JSON_EXTRACT(b.raw_json, '$.thinking'))) AS btext,
                   b.start_timestamp, c.account_uuid, c.project_uuid, c.raw_json,
                   m.msg_idx, m.created_at, b.block_index, c.name
            FROM anthropic_content_blocks b
                 JOIN m ON b.message_uuid = m.message_uuid
                 JOIN anthropic_conversations c ON m.conversation_uuid = c.conversation_uuid
            WHERE b.type IN ('tool_use', 'tool_result', 'thinking')
            """
        )
        for (
            mid,
            cuuid,
            btype,
            text,
            when,
            account,
            project,
            raw_json,
            msg_idx,
            msg_created,
            block_index,
            cname,
        ) in cur.fetchall():
            kind = _anthropic_kind_for_block(btype or "")
            model = _extract_model_from_raw(raw_json)
            author = model or btype or ""
            row_text = text or btype or ""
            row_when = when or _bump_micros(msg_created or "", int(block_index) + 1)
            yield _Row(
                uuid=f"{mid}:{block_index}",
                provider="anthropic",
                kind=kind,
                source_label="Claude",
                when_ts=row_when or "",
                author=author,
                account=account,
                project=project,
                channel=None,
                conversation_name=cname,
                conversation_uuid=cuuid,
                message_index=int(msg_idx),
                entire_chat=f"/chat/{cuuid}",
                text=row_text,
                slack_link=None,
            )


def _openai_chat_rows(conn: Connection) -> Iterable[_Row]:
    with conn.cursor() as cur:
        cur.execute(
            """
            SELECT conversation_id, account_id, title,
                   COALESCE(create_time, update_time) AS when_ts
            FROM openai_conversations
            """
        )
        for cid, account, title, when in cur.fetchall():
            yield _Row(
                uuid=cid,
                provider="openai",
                kind="Chat",
                source_label="ChatGPT",
                when_ts=when or "",
                author=None,
                account=account,
                project=None,
                channel=None,
                conversation_name=title,
                conversation_uuid=cid,
                message_index=None,
                entire_chat=f"/chat/{cid}",
                text=title or "",
                slack_link=None,
            )


def _openai_message_rows(conn: Connection) -> Iterable[_Row]:
    with conn.cursor() as cur:
        cur.execute(
            """
            WITH m AS (
                SELECT message_id, conversation_id, role, content_type, text,
                       create_time, model_slug,
                       ROW_NUMBER() OVER (PARTITION BY conversation_id
                                          ORDER BY create_time, message_id) - 1 AS msg_idx
                FROM openai_messages
            )
            SELECT m.message_id, m.conversation_id, m.role, m.text, m.create_time,
                   m.model_slug, m.content_type, m.msg_idx,
                   c.account_id, COALESCE(c.create_time, c.update_time) AS conv_time,
                   c.title
            FROM m JOIN openai_conversations c
                 ON m.conversation_id = c.conversation_id
            """
        )
        for (
            mid,
            cid,
            role,
            text,
            when,
            model,
            content_type,
            msg_idx,
            account,
            conv_time,
            ctitle,
        ) in cur.fetchall():
            kind = _openai_kind_for_role_and_type(role or "", content_type or "")
            if kind == "User Input":
                author = account
            elif kind in ("LLM Response", "LLM Thinking"):
                author = model or role
            else:
                author = role
            row_when = when or _bump_micros(conv_time or "", int(msg_idx) + 1)
            yield _Row(
                uuid=mid,
                provider="openai",
                kind=kind,
                source_label="ChatGPT",
                when_ts=row_when or "",
                author=author,
                account=account,
                project=None,
                channel=None,
                conversation_name=ctitle,
                conversation_uuid=cid,
                message_index=int(msg_idx),
                entire_chat=f"/chat/{cid}",
                text=text or "",
                slack_link=None,
            )


def _slack_link(team_id: str, channel_id: str, ts: str) -> str:
    ts_no_dot = ts.replace(".", "")
    return f"https://slack.com/archives/{channel_id}/p{ts_no_dot}?team={team_id}"


def _table_exists(conn: Connection, name: str) -> bool:
    with conn.cursor() as cur:
        cur.execute("SELECT DATABASE()")
        (db,) = cur.fetchone()  # type: ignore[misc]
        cur.execute(
            "SELECT COUNT(*) FROM information_schema.tables "
            "WHERE table_schema = %s AND table_name = %s",
            (db, name),
        )
        (n,) = cur.fetchone()  # type: ignore[misc]
        return bool(n)


def _slack_thread_rows(conn: Connection) -> Iterable[_Row]:
    """One Slack Thread row per distinct thread_uuid. The row's `text` is
    the root message's body (which is what the user searches against);
    when_ts is the root's ts_iso so the thread sorts by its origin time."""
    if not _table_exists(conn, "slack_messages"):
        return
    with conn.cursor() as cur:
        cur.execute(
            """
            SELECT m.thread_uuid, m.team_id, m.channel_id, m.ts, m.user_id,
                   m.text, m.ts_iso, c.name AS channel_name,
                   u.real_name, u.name
            FROM slack_messages m
            JOIN slack_channels c ON c.channel_id = m.channel_id
            LEFT JOIN slack_users u ON u.user_id = m.user_id
            WHERE m.is_thread_root = TRUE
            """
        )
        for (
            thread_uuid,
            team_id,
            channel_id,
            ts,
            user_id,
            text,
            ts_iso,
            channel_name,
            user_real,
            user_name,
        ) in cur.fetchall():
            author = user_real or user_name or user_id
            yield _Row(
                uuid=thread_uuid,
                provider="slack",
                kind="Slack Thread",
                source_label="Slack",
                when_ts=ts_iso or "",
                author=author,
                account=team_id,
                project=None,
                channel=channel_name,
                conversation_name=f"#{channel_name}",
                conversation_uuid=thread_uuid,
                message_index=None,
                entire_chat=f"/slack/{thread_uuid}",
                text=text or "",
                slack_link=_slack_link(team_id, channel_id, ts),
            )


def _slack_message_rows(conn: Connection) -> Iterable[_Row]:
    """One Slack Message row per slack_messages row (root + replies). Each
    carries the parent thread's uuid in `conversation_uuid` so clicking a
    message scrolls into the thread QMD at the right anchor."""
    if not _table_exists(conn, "slack_messages"):
        return
    with conn.cursor() as cur:
        cur.execute(
            """
            WITH m AS (
                SELECT uuid, team_id, channel_id, ts, thread_uuid, user_id, text,
                       ts_iso,
                       ROW_NUMBER() OVER (PARTITION BY thread_uuid
                                          ORDER BY ts_iso, ts) - 1 AS msg_idx
                FROM slack_messages
            )
            SELECT m.uuid, m.team_id, m.channel_id, m.ts, m.thread_uuid,
                   m.user_id, m.text, m.ts_iso, m.msg_idx,
                   c.name AS channel_name,
                   u.real_name, u.name
            FROM m
            JOIN slack_channels c ON c.channel_id = m.channel_id
            LEFT JOIN slack_users u ON u.user_id = m.user_id
            """
        )
        for (
            mid,
            team_id,
            channel_id,
            ts,
            thread_uuid,
            user_id,
            text,
            ts_iso,
            msg_idx,
            channel_name,
            user_real,
            user_name,
        ) in cur.fetchall():
            author = user_real or user_name or user_id
            yield _Row(
                uuid=mid,
                provider="slack",
                kind="Slack Message",
                source_label="Slack",
                when_ts=ts_iso or "",
                author=author,
                account=team_id,
                project=None,
                channel=channel_name,
                conversation_name=f"#{channel_name}",
                conversation_uuid=thread_uuid,
                message_index=int(msg_idx),
                entire_chat=f"/slack/{thread_uuid}",
                text=text or "",
                slack_link=_slack_link(team_id, channel_id, ts),
            )


# ----- entry point ----------------------------------------------------------


def populate_grid_rows(conn: Connection) -> int:
    """Truncate `grid_rows` and re-emit every row from the per-provider
    tables. Returns the number of rows inserted."""
    ensure_schema(conn)
    builders = (
        _anthropic_chat_rows,
        _anthropic_message_rows,
        _anthropic_block_rows,
        _openai_chat_rows,
        _openai_message_rows,
        _slack_thread_rows,
        _slack_message_rows,
    )
    rows: list[_Row] = []
    for builder in builders:
        rows.extend(builder(conn))

    placeholders = ",".join(["%s"] * len(_GRID_ROWS_COLUMNS))
    columns_sql = ", ".join(_GRID_ROWS_COLUMNS)
    with conn.cursor() as cur:
        cur.execute("DELETE FROM grid_rows")
        if rows:
            cur.executemany(
                f"INSERT INTO grid_rows ({columns_sql}) VALUES ({placeholders})",
                [
                    (
                        r.uuid,
                        r.provider,
                        r.kind,
                        r.source_label,
                        r.when_ts,
                        r.author,
                        r.account,
                        r.project,
                        r.channel,
                        r.conversation_name,
                        r.conversation_uuid,
                        r.message_index,
                        r.entire_chat,
                        r.text,
                        r.slack_link,
                    )
                    for r in rows
                ],
            )
    return len(rows)
