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


def ingest_export_dir(
    conn: Connection,
    export_dir: Path,
    ingest_started_at: str,
) -> tuple[ParsedExport, AnthropicIngestStats]:
    """Parse the Anthropic export and UPSERT every row, bumping last_seen_at."""
    ensure_schema(conn)
    parsed = parse_export(export_dir)
    stats = AnthropicIngestStats()

    with conn.cursor() as cur:
        # accounts: keep first_seen_at, bump last_seen_at; insert new ones with both = now.
        for a in parsed.accounts:
            cur.execute(
                """
                INSERT INTO anthropic_accounts
                    (account_uuid, email, full_name, raw_json, first_seen_at, last_seen_at)
                VALUES (%s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    email = VALUES(email),
                    full_name = VALUES(full_name),
                    raw_json = VALUES(raw_json)
                """,
                (
                    a.account_uuid,
                    a.email,
                    a.full_name,
                    json.dumps(a.raw_json, ensure_ascii=False),
                    ingest_started_at,
                    ingest_started_at,
                ),
            )
            stats.accounts += 1

        for p in parsed.projects:
            cur.execute(
                """
                INSERT INTO anthropic_projects
                    (account_uuid, project_uuid, name, description, is_starter,
                     created_at, updated_at, raw_json, last_seen_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    account_uuid = VALUES(account_uuid),
                    name = VALUES(name),
                    description = VALUES(description),
                    is_starter = VALUES(is_starter),
                    created_at = VALUES(created_at),
                    updated_at = VALUES(updated_at),
                    raw_json = VALUES(raw_json)
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
                    ingest_started_at,
                ),
            )
            stats.projects += 1

        for c in parsed.conversations:
            cur.execute(
                """
                INSERT INTO anthropic_conversations
                    (account_uuid, conversation_uuid, project_uuid, name, summary,
                     created_at, updated_at, raw_json, last_seen_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    account_uuid = VALUES(account_uuid),
                    project_uuid = VALUES(project_uuid),
                    name = VALUES(name),
                    summary = VALUES(summary),
                    created_at = VALUES(created_at),
                    updated_at = VALUES(updated_at),
                    raw_json = VALUES(raw_json)
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
                    ingest_started_at,
                ),
            )
            stats.conversations += 1

        for m in parsed.messages:
            cur.execute(
                """
                INSERT INTO anthropic_messages
                    (conversation_uuid, message_uuid, parent_message_uuid, sender, text,
                     created_at, updated_at, raw_json, last_seen_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    conversation_uuid = VALUES(conversation_uuid),
                    parent_message_uuid = VALUES(parent_message_uuid),
                    sender = VALUES(sender),
                    text = VALUES(text),
                    created_at = VALUES(created_at),
                    updated_at = VALUES(updated_at),
                    raw_json = VALUES(raw_json)
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
                    ingest_started_at,
                ),
            )
            stats.messages += 1

        # content_blocks: index can shift across re-exports if Anthropic changes
        # ordering, but in practice we trust the export order. Delete-and-reinsert
        # per message would be safer for shape changes; for v0 we UPSERT.
        for b in parsed.content_blocks:
            cur.execute(
                """
                INSERT INTO anthropic_content_blocks
                    (message_uuid, block_index, type, text, start_timestamp, stop_timestamp, raw_json)
                VALUES (%s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    type = VALUES(type),
                    text = VALUES(text),
                    start_timestamp = VALUES(start_timestamp),
                    stop_timestamp = VALUES(stop_timestamp),
                    raw_json = VALUES(raw_json)
                """,
                (
                    b.message_uuid,
                    b.block_index,
                    b.type,
                    b.text,
                    b.start_timestamp,
                    b.stop_timestamp,
                    json.dumps(b.raw_json, ensure_ascii=False),
                ),
            )
            stats.content_blocks += 1

        for at in parsed.attachments:
            cur.execute(
                """
                INSERT INTO anthropic_attachments
                    (message_uuid, attachment_index, kind, raw_json)
                VALUES (%s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    raw_json = VALUES(raw_json)
                """,
                (
                    at.message_uuid,
                    at.attachment_index,
                    at.kind,
                    json.dumps(at.raw_json, ensure_ascii=False),
                ),
            )
            stats.attachments += 1

    return parsed, stats
