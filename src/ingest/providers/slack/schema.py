from __future__ import annotations

from pymysql.connections import Connection

# Tables backing the Slack ingest. The Slack web API gives us:
#   - workspace identity (one row per team_id)
#   - users (one row per user_id, scoped to a team)
#   - channels (one row per channel_id, scoped to a team)
#   - messages (one row per top-level or threaded reply, scoped to a channel)
#   - reactions (one row per (message, emoji, user))
#
# `slack_messages.thread_ts` is Slack's native thread join key — top-level
# messages either omit it or set it equal to their own `ts`; replies carry
# the parent's `ts`. We keep both `ts` (this message) and `thread_ts`
# (parent) to make per-thread rendering and grid row generation cheap.
#
# UUIDs are deterministic v5 hashes of `slack:{team}:{channel}:{ts}` so a
# re-ingest is idempotent and we have a stable id to anchor in QMD output.

DDL: list[str] = [
    """
    CREATE TABLE IF NOT EXISTS slack_workspaces (
        team_id       VARCHAR(64)  NOT NULL,
        team_name     VARCHAR(255),
        team_url      VARCHAR(512),
        self_user_id  VARCHAR(64),
        raw_json      JSON         NOT NULL,
        first_seen_at VARCHAR(40)  NOT NULL,
        last_seen_at  VARCHAR(40)  NOT NULL,
        PRIMARY KEY (team_id)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS slack_users (
        team_id       VARCHAR(64)  NOT NULL,
        user_id       VARCHAR(64)  NOT NULL,
        name          VARCHAR(255),
        real_name     VARCHAR(255),
        display_name  VARCHAR(255),
        title         VARCHAR(255),
        deleted       BOOLEAN,
        raw_json      JSON         NOT NULL,
        last_seen_at  VARCHAR(40)  NOT NULL,
        PRIMARY KEY (user_id)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS slack_channels (
        team_id       VARCHAR(64)  NOT NULL,
        channel_id    VARCHAR(64)  NOT NULL,
        name          VARCHAR(255),
        is_private    BOOLEAN,
        is_archived   BOOLEAN,
        topic         VARCHAR(1024),
        purpose       VARCHAR(1024),
        raw_json      JSON         NOT NULL,
        last_seen_at  VARCHAR(40)  NOT NULL,
        PRIMARY KEY (channel_id)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS slack_messages (
        uuid          VARCHAR(64)  NOT NULL,
        team_id       VARCHAR(64)  NOT NULL,
        channel_id    VARCHAR(64)  NOT NULL,
        ts            VARCHAR(32)  NOT NULL,
        thread_ts     VARCHAR(32),
        thread_uuid   VARCHAR(64)  NOT NULL,
        user_id       VARCHAR(64),
        text          LONGTEXT,
        ts_iso        VARCHAR(40)  NOT NULL,
        is_thread_root BOOLEAN     NOT NULL,
        raw_json      JSON         NOT NULL,
        last_seen_at  VARCHAR(40)  NOT NULL,
        PRIMARY KEY (uuid)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS slack_reactions (
        uuid          VARCHAR(64)  NOT NULL,
        message_uuid  VARCHAR(64)  NOT NULL,
        name          VARCHAR(128) NOT NULL,
        user_id       VARCHAR(64)  NOT NULL,
        last_seen_at  VARCHAR(40)  NOT NULL,
        PRIMARY KEY (uuid)
    )
    """,
]


def ensure_schema(conn: Connection) -> None:
    with conn.cursor() as cur:
        for stmt in DDL:
            cur.execute(stmt)
