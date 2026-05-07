from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from pymysql.connections import Connection

from ingest.providers.openai.parse import ParsedChatGPTApi, parse_api_dir
from ingest.providers.openai.schema import ensure_schema


@dataclass
class OpenAIIngestStats:
    accounts: int = 0
    projects: int = 0  # always 0 — kept for shape parity with anthropic stats
    conversations: int = 0
    messages: int = 0
    content_blocks: int = 0  # populated from content_parts; kept named for parity
    attachments: int = 0     # always 0 today (chatgpt API surfaces none)


# UPSERT helper: per-column gating. ChatGPT today is api-only, but we keep
# the same shape as the anthropic ingest so a future export transport can
# land here without touching the SQL.
def _api_wins(col: str) -> str:
    return f"{col} = IF(VALUES(source) = 'api' OR source != 'api', VALUES({col}), {col})"


def _source_merge() -> str:
    return "source = IF(source = 'api', 'api', VALUES(source))"


def ingest_api_dir(
    conn: Connection,
    api_dir: Path,
    ingest_started_at: str,
    source: str = "api",
) -> tuple[ParsedChatGPTApi, OpenAIIngestStats]:
    if source not in ("export", "api"):
        raise ValueError(f"source must be 'export' or 'api', got {source!r}")
    ensure_schema(conn)
    parsed = parse_api_dir(api_dir)
    stats = OpenAIIngestStats()

    with conn.cursor() as cur:
        for a in parsed.accounts:
            cur.execute(
                f"""
                INSERT INTO openai_accounts
                    (account_id, email, name, raw_json, source,
                     first_seen_at, last_seen_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    {_api_wins("email")},
                    {_api_wins("name")},
                    {_api_wins("raw_json")},
                    {_source_merge()},
                    last_seen_at = VALUES(last_seen_at)
                """,
                (
                    a.account_id, a.email, a.name,
                    json.dumps(a.raw_json, ensure_ascii=False),
                    source, ingest_started_at, ingest_started_at,
                ),
            )
            stats.accounts += 1

        for c in parsed.conversations:
            cur.execute(
                f"""
                INSERT INTO openai_conversations
                    (account_id, conversation_id, title, create_time, update_time,
                     current_node, default_model_slug, gizmo_id, gizmo_type,
                     is_archived, is_starred, raw_json, source, last_seen_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    {_api_wins("account_id")},
                    {_api_wins("title")},
                    {_api_wins("create_time")},
                    {_api_wins("update_time")},
                    {_api_wins("current_node")},
                    {_api_wins("default_model_slug")},
                    {_api_wins("gizmo_id")},
                    {_api_wins("gizmo_type")},
                    {_api_wins("is_archived")},
                    {_api_wins("is_starred")},
                    {_api_wins("raw_json")},
                    {_source_merge()},
                    last_seen_at = VALUES(last_seen_at)
                """,
                (
                    c.account_id, c.conversation_id, c.title,
                    c.create_time, c.update_time, c.current_node,
                    c.default_model_slug, c.gizmo_id, c.gizmo_type,
                    c.is_archived, c.is_starred,
                    json.dumps(c.raw_json, ensure_ascii=False),
                    source, ingest_started_at,
                ),
            )
            stats.conversations += 1

        # api-locked-message detection (parity with anthropic ingest). Today
        # only relevant if a future 'export' transport ever co-exists.
        api_locked_msg_ids: set[str] = set()
        if source == "export" and parsed.messages:
            ids = [m.message_id for m in parsed.messages]
            placeholders = ",".join(["%s"] * len(ids))
            cur.execute(
                f"SELECT message_id FROM openai_messages "
                f"WHERE message_id IN ({placeholders}) AND source = 'api'",
                ids,
            )
            api_locked_msg_ids = {r[0] for r in cur.fetchall()}

        for m in parsed.messages:
            cur.execute(
                f"""
                INSERT INTO openai_messages
                    (conversation_id, message_id, parent_id, role, recipient, channel,
                     content_type, text, status, end_turn, weight, model_slug,
                     create_time, update_time, raw_json, source, last_seen_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    {_api_wins("conversation_id")},
                    {_api_wins("parent_id")},
                    {_api_wins("role")},
                    {_api_wins("recipient")},
                    {_api_wins("channel")},
                    {_api_wins("content_type")},
                    {_api_wins("text")},
                    {_api_wins("status")},
                    {_api_wins("end_turn")},
                    {_api_wins("weight")},
                    {_api_wins("model_slug")},
                    {_api_wins("create_time")},
                    {_api_wins("update_time")},
                    {_api_wins("raw_json")},
                    {_source_merge()},
                    last_seen_at = VALUES(last_seen_at)
                """,
                (
                    m.conversation_id, m.message_id, m.parent_id, m.role,
                    m.recipient, m.channel, m.content_type, m.text, m.status,
                    m.end_turn, m.weight, m.model_slug,
                    m.create_time, m.update_time,
                    json.dumps(m.raw_json, ensure_ascii=False),
                    source, ingest_started_at,
                ),
            )
            stats.messages += 1

        # content_parts: API-authoritative delete-and-reinsert per message.
        parts_by_msg: dict[str, list] = {}
        for p in parsed.content_parts:
            parts_by_msg.setdefault(p.message_id, []).append(p)

        for msg_id, parts in parts_by_msg.items():
            if source == "export" and msg_id in api_locked_msg_ids:
                continue
            if source == "api":
                cur.execute(
                    "DELETE FROM openai_content_parts WHERE message_id = %s",
                    (msg_id,),
                )
            for p in parts:
                cur.execute(
                    f"""
                    INSERT INTO openai_content_parts
                        (message_id, part_index, kind, language, text, raw_json, source)
                    VALUES (%s, %s, %s, %s, %s, %s, %s)
                    ON DUPLICATE KEY UPDATE
                        {_api_wins("kind")},
                        {_api_wins("language")},
                        {_api_wins("text")},
                        {_api_wins("raw_json")},
                        {_source_merge()}
                    """,
                    (
                        p.message_id, p.part_index, p.kind, p.language, p.text,
                        json.dumps(p.raw_json, ensure_ascii=False), source,
                    ),
                )
                stats.content_blocks += 1

    return parsed, stats
