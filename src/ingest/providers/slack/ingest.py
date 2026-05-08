from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from pymysql.connections import Connection

from ingest.providers.slack.parse import ParsedSlackApi, parse_api_dir
from ingest.providers.slack.schema import ensure_schema


@dataclass
class SlackIngestStats:
    workspaces: int = 0
    users: int = 0
    channels: int = 0
    messages: int = 0
    reactions: int = 0


def ingest_api_dir(
    conn: Connection,
    api_dir: Path,
    ingest_started_at: str,
) -> tuple[ParsedSlackApi, SlackIngestStats]:
    """Ingest a Slack-API events directory (the layout produced by
    `src/download/slack_web.py`) into the slack_* tables.

    Re-ingest is idempotent: every row's PK is deterministic (team_id /
    user_id / channel_id are Slack-native; message uuid is uuidv5 over
    `slack:{team}:{channel}:{ts}`), so a second run upserts in place.
    Reactions are wiped per-message before reinsert to keep the set
    consistent with whatever the latest Slack snapshot shows."""
    ensure_schema(conn)
    parsed = parse_api_dir(api_dir)
    stats = SlackIngestStats()

    with conn.cursor() as cur:
        for w in parsed.workspaces:
            cur.execute(
                """
                INSERT INTO slack_workspaces
                    (team_id, team_name, team_url, self_user_id, raw_json,
                     first_seen_at, last_seen_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    team_name = VALUES(team_name),
                    team_url = VALUES(team_url),
                    self_user_id = VALUES(self_user_id),
                    raw_json = VALUES(raw_json),
                    last_seen_at = VALUES(last_seen_at)
                """,
                (
                    w.team_id,
                    w.team_name,
                    w.team_url,
                    w.self_user_id,
                    json.dumps(w.raw_json, ensure_ascii=False),
                    ingest_started_at,
                    ingest_started_at,
                ),
            )
            stats.workspaces += 1

        for u in parsed.users:
            cur.execute(
                """
                INSERT INTO slack_users
                    (team_id, user_id, name, real_name, display_name, title,
                     deleted, raw_json, last_seen_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    team_id = VALUES(team_id),
                    name = VALUES(name),
                    real_name = VALUES(real_name),
                    display_name = VALUES(display_name),
                    title = VALUES(title),
                    deleted = VALUES(deleted),
                    raw_json = VALUES(raw_json),
                    last_seen_at = VALUES(last_seen_at)
                """,
                (
                    u.team_id,
                    u.user_id,
                    u.name,
                    u.real_name,
                    u.display_name,
                    u.title,
                    u.deleted,
                    json.dumps(u.raw_json, ensure_ascii=False),
                    ingest_started_at,
                ),
            )
            stats.users += 1

        for c in parsed.channels:
            cur.execute(
                """
                INSERT INTO slack_channels
                    (team_id, channel_id, name, is_private, is_archived,
                     topic, purpose, raw_json, last_seen_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    team_id = VALUES(team_id),
                    name = VALUES(name),
                    is_private = VALUES(is_private),
                    is_archived = VALUES(is_archived),
                    topic = VALUES(topic),
                    purpose = VALUES(purpose),
                    raw_json = VALUES(raw_json),
                    last_seen_at = VALUES(last_seen_at)
                """,
                (
                    c.team_id,
                    c.channel_id,
                    c.name,
                    c.is_private,
                    c.is_archived,
                    c.topic,
                    c.purpose,
                    json.dumps(c.raw_json, ensure_ascii=False),
                    ingest_started_at,
                ),
            )
            stats.channels += 1

        for m in parsed.messages:
            cur.execute(
                """
                INSERT INTO slack_messages
                    (uuid, team_id, channel_id, ts, thread_ts, thread_uuid,
                     user_id, text, ts_iso, is_thread_root, raw_json,
                     last_seen_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    team_id = VALUES(team_id),
                    channel_id = VALUES(channel_id),
                    ts = VALUES(ts),
                    thread_ts = VALUES(thread_ts),
                    thread_uuid = VALUES(thread_uuid),
                    user_id = VALUES(user_id),
                    text = VALUES(text),
                    ts_iso = VALUES(ts_iso),
                    is_thread_root = VALUES(is_thread_root),
                    raw_json = VALUES(raw_json),
                    last_seen_at = VALUES(last_seen_at)
                """,
                (
                    m.uuid,
                    m.team_id,
                    m.channel_id,
                    m.ts,
                    m.thread_ts,
                    m.thread_uuid,
                    m.user_id,
                    m.text,
                    m.ts_iso,
                    m.is_thread_root,
                    json.dumps(m.raw_json, ensure_ascii=False),
                    ingest_started_at,
                ),
            )
            stats.messages += 1

        # Reactions: delete-then-insert per message so the stored set
        # exactly matches the latest Slack snapshot (someone removed an
        # emoji upstream → we drop it locally too).
        msg_ids_with_reactions = {r.message_uuid for r in parsed.reactions}
        for mid in msg_ids_with_reactions:
            cur.execute("DELETE FROM slack_reactions WHERE message_uuid = %s", (mid,))
        for r in parsed.reactions:
            cur.execute(
                """
                INSERT INTO slack_reactions
                    (uuid, message_uuid, name, user_id, last_seen_at)
                VALUES (%s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    last_seen_at = VALUES(last_seen_at)
                """,
                (r.uuid, r.message_uuid, r.name, r.user_id, ingest_started_at),
            )
            stats.reactions += 1

    return parsed, stats
