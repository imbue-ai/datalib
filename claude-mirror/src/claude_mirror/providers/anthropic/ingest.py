from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from pymysql.connections import Connection

from claude_mirror.providers.anthropic.parse import ParsedExport, parse_export
from claude_mirror.providers.anthropic.schema import ensure_schema


@dataclass
class AnthropicIngestStats:
    accounts: int = 0
    projects: int = 0
    conversations: int = 0
    messages: int = 0
    content_blocks: int = 0
    attachments: int = 0


# UPSERT helper: for each data column, only overwrite when incoming source is
# 'api' or the existing row is not api-locked. The `source` column itself
# upgrades export→api but never downgrades api→export.
def _api_wins(col: str) -> str:
    return f"{col} = IF(VALUES(source) = 'api' OR source != 'api', VALUES({col}), {col})"


def _source_merge() -> str:
    return "source = IF(source = 'api', 'api', VALUES(source))"


def ingest_export_dir(
    conn: Connection,
    export_dir: Path,
    ingest_started_at: str,
    source: str = "export",
) -> tuple[ParsedExport, AnthropicIngestStats]:
    """Parse the Anthropic export and UPSERT every row.

    `source` is one of {'export', 'api'}. API rows are authoritative: a
    later 'export' ingest does not clobber a row last touched by 'api'. An
    'api' ingest also delete-and-reinserts content_blocks and attachments
    per message_uuid so that fewer/changed blocks from the API replace any
    stale ones the export left behind.
    """
    if source not in ("export", "api"):
        raise ValueError(f"source must be 'export' or 'api', got {source!r}")
    ensure_schema(conn)
    parsed = parse_export(export_dir)
    stats = AnthropicIngestStats()

    with conn.cursor() as cur:
        for a in parsed.accounts:
            cur.execute(
                f"""
                INSERT INTO anthropic_accounts
                    (account_uuid, email, full_name, raw_json, source, first_seen_at, last_seen_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    {_api_wins("email")},
                    {_api_wins("full_name")},
                    {_api_wins("raw_json")},
                    {_source_merge()},
                    last_seen_at = VALUES(last_seen_at)
                """,
                (
                    a.account_uuid,
                    a.email,
                    a.full_name,
                    json.dumps(a.raw_json, ensure_ascii=False),
                    source,
                    ingest_started_at,
                    ingest_started_at,
                ),
            )
            stats.accounts += 1

        for p in parsed.projects:
            cur.execute(
                f"""
                INSERT INTO anthropic_projects
                    (account_uuid, project_uuid, name, description, is_starter,
                     created_at, updated_at, raw_json, source, last_seen_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    {_api_wins("account_uuid")},
                    {_api_wins("name")},
                    {_api_wins("description")},
                    {_api_wins("is_starter")},
                    {_api_wins("created_at")},
                    {_api_wins("updated_at")},
                    {_api_wins("raw_json")},
                    {_source_merge()},
                    last_seen_at = VALUES(last_seen_at)
                """,
                (
                    p.account_uuid,
                    p.project_uuid,
                    p.name,
                    p.description,
                    p.is_starter,
                    p.created_at,
                    p.updated_at,
                    json.dumps(p.raw_json, ensure_ascii=False),
                    source,
                    ingest_started_at,
                ),
            )
            stats.projects += 1

        for c in parsed.conversations:
            cur.execute(
                f"""
                INSERT INTO anthropic_conversations
                    (account_uuid, conversation_uuid, project_uuid, name, summary,
                     created_at, updated_at, raw_json, source, last_seen_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    {_api_wins("account_uuid")},
                    {_api_wins("project_uuid")},
                    {_api_wins("name")},
                    {_api_wins("summary")},
                    {_api_wins("created_at")},
                    {_api_wins("updated_at")},
                    {_api_wins("raw_json")},
                    {_source_merge()},
                    last_seen_at = VALUES(last_seen_at)
                """,
                (
                    c.account_uuid,
                    c.conversation_uuid,
                    c.project_uuid,
                    c.name,
                    c.summary,
                    c.created_at,
                    c.updated_at,
                    json.dumps(c.raw_json, ensure_ascii=False),
                    source,
                    ingest_started_at,
                ),
            )
            stats.conversations += 1

        # Track which messages this ingest *would* touch, and which are
        # api-locked (so an export ingest must skip their blocks/attachments).
        api_locked_msg_uuids: set[str] = set()
        if source == "export" and parsed.messages:
            uuids = [m.message_uuid for m in parsed.messages]
            placeholders = ",".join(["%s"] * len(uuids))
            cur.execute(
                f"SELECT message_uuid FROM anthropic_messages "
                f"WHERE message_uuid IN ({placeholders}) AND source = 'api'",
                uuids,
            )
            api_locked_msg_uuids = {r[0] for r in cur.fetchall()}

        for m in parsed.messages:
            cur.execute(
                f"""
                INSERT INTO anthropic_messages
                    (conversation_uuid, message_uuid, parent_message_uuid, sender, text,
                     created_at, updated_at, raw_json, source, last_seen_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    {_api_wins("conversation_uuid")},
                    {_api_wins("parent_message_uuid")},
                    {_api_wins("sender")},
                    {_api_wins("text")},
                    {_api_wins("created_at")},
                    {_api_wins("updated_at")},
                    {_api_wins("raw_json")},
                    {_source_merge()},
                    last_seen_at = VALUES(last_seen_at)
                """,
                (
                    m.conversation_uuid,
                    m.message_uuid,
                    m.parent_message_uuid,
                    m.sender,
                    m.text,
                    m.created_at,
                    m.updated_at,
                    json.dumps(m.raw_json, ensure_ascii=False),
                    source,
                    ingest_started_at,
                ),
            )
            stats.messages += 1

        # content_blocks: API ingest is authoritative — delete-and-reinsert
        # per message so trimmed/reordered blocks don't leave orphans.
        # Export ingest skips messages that are api-locked.
        blocks_by_msg: dict[str, list] = {}
        for b in parsed.content_blocks:
            blocks_by_msg.setdefault(b.message_uuid, []).append(b)

        for msg_uuid, blocks in blocks_by_msg.items():
            if source == "export" and msg_uuid in api_locked_msg_uuids:
                continue
            if source == "api":
                cur.execute(
                    "DELETE FROM anthropic_content_blocks WHERE message_uuid = %s",
                    (msg_uuid,),
                )
            for b in blocks:
                cur.execute(
                    f"""
                    INSERT INTO anthropic_content_blocks
                        (message_uuid, block_index, type, text,
                         start_timestamp, stop_timestamp, raw_json, source)
                    VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
                    ON DUPLICATE KEY UPDATE
                        {_api_wins("type")},
                        {_api_wins("text")},
                        {_api_wins("start_timestamp")},
                        {_api_wins("stop_timestamp")},
                        {_api_wins("raw_json")},
                        {_source_merge()}
                    """,
                    (
                        b.message_uuid,
                        b.block_index,
                        b.type,
                        b.text,
                        b.start_timestamp,
                        b.stop_timestamp,
                        json.dumps(b.raw_json, ensure_ascii=False),
                        source,
                    ),
                )
                stats.content_blocks += 1

        attachments_by_msg: dict[str, list] = {}
        for at in parsed.attachments:
            attachments_by_msg.setdefault(at.message_uuid, []).append(at)

        for msg_uuid, atts in attachments_by_msg.items():
            if source == "export" and msg_uuid in api_locked_msg_uuids:
                continue
            if source == "api":
                cur.execute(
                    "DELETE FROM anthropic_attachments WHERE message_uuid = %s",
                    (msg_uuid,),
                )
            for at in atts:
                cur.execute(
                    f"""
                    INSERT INTO anthropic_attachments
                        (message_uuid, attachment_index, kind, raw_json, source)
                    VALUES (%s, %s, %s, %s, %s)
                    ON DUPLICATE KEY UPDATE
                        {_api_wins("raw_json")},
                        {_source_merge()}
                    """,
                    (
                        at.message_uuid,
                        at.attachment_index,
                        at.kind,
                        json.dumps(at.raw_json, ensure_ascii=False),
                        source,
                    ),
                )
                stats.attachments += 1

    return parsed, stats
