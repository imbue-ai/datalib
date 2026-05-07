from __future__ import annotations

from pymysql.connections import Connection

# Dolt's JSON support is solid in 1.x; we use JSON for raw_json columns.
# Timestamps from the export are ISO-8601 strings — we store them as VARCHAR
# verbatim to avoid lossy parsing; downstream readers can cast as needed.
#
# Every row carries a `source` column, one of {'export', 'api'}. The ingest
# UPSERT logic treats 'api' as authoritative: an export re-ingest is *not*
# allowed to overwrite or clear data on a row last touched by 'api'. See
# CLAUDE_WEB_SCHEMA.md for the full rationale and field-level differences
# between the two transports.

TABLES_WITH_SOURCE = (
    "anthropic_accounts",
    "anthropic_projects",
    "anthropic_conversations",
    "anthropic_messages",
    "anthropic_content_blocks",
    "anthropic_attachments",
)

DDL: list[str] = [
    """
    CREATE TABLE IF NOT EXISTS anthropic_accounts (
        account_uuid    VARCHAR(64)  NOT NULL,
        email           VARCHAR(320),
        full_name       VARCHAR(255),
        raw_json        JSON         NOT NULL,
        source          VARCHAR(16)  NOT NULL DEFAULT 'export',
        first_seen_at   VARCHAR(40)  NOT NULL,
        last_seen_at    VARCHAR(40)  NOT NULL,
        PRIMARY KEY (account_uuid)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS anthropic_projects (
        account_uuid    VARCHAR(64)  NOT NULL,
        project_uuid    VARCHAR(64)  NOT NULL,
        name            VARCHAR(512),
        description     TEXT,
        is_starter      BOOLEAN,
        created_at      VARCHAR(40),
        updated_at      VARCHAR(40),
        raw_json        JSON         NOT NULL,
        source          VARCHAR(16)  NOT NULL DEFAULT 'export',
        last_seen_at    VARCHAR(40)  NOT NULL,
        PRIMARY KEY (project_uuid)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS anthropic_conversations (
        account_uuid     VARCHAR(64)  NOT NULL,
        conversation_uuid VARCHAR(64) NOT NULL,
        project_uuid     VARCHAR(64),
        name             VARCHAR(1024),
        summary          TEXT,
        created_at       VARCHAR(40),
        updated_at       VARCHAR(40),
        raw_json         JSON         NOT NULL,
        source           VARCHAR(16)  NOT NULL DEFAULT 'export',
        last_seen_at     VARCHAR(40)  NOT NULL,
        PRIMARY KEY (conversation_uuid)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS anthropic_messages (
        conversation_uuid    VARCHAR(64)  NOT NULL,
        message_uuid         VARCHAR(64)  NOT NULL,
        parent_message_uuid  VARCHAR(64),
        sender               VARCHAR(32),
        text                 LONGTEXT,
        created_at           VARCHAR(40),
        updated_at           VARCHAR(40),
        raw_json             JSON         NOT NULL,
        source               VARCHAR(16)  NOT NULL DEFAULT 'export',
        last_seen_at         VARCHAR(40)  NOT NULL,
        PRIMARY KEY (message_uuid)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS anthropic_content_blocks (
        message_uuid    VARCHAR(64)  NOT NULL,
        block_index     INT          NOT NULL,
        type            VARCHAR(64),
        text            LONGTEXT,
        start_timestamp VARCHAR(40),
        stop_timestamp  VARCHAR(40),
        raw_json        JSON         NOT NULL,
        source          VARCHAR(16)  NOT NULL DEFAULT 'export',
        PRIMARY KEY (message_uuid, block_index)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS anthropic_attachments (
        message_uuid     VARCHAR(64)  NOT NULL,
        attachment_index INT         NOT NULL,
        kind             VARCHAR(32) NOT NULL,
        raw_json         JSON         NOT NULL,
        source           VARCHAR(16) NOT NULL DEFAULT 'export',
        PRIMARY KEY (message_uuid, attachment_index, kind)
    )
    """,
]


def ensure_schema(conn: Connection) -> None:
    with conn.cursor() as cur:
        for stmt in DDL:
            cur.execute(stmt)
        # Backfill `source` on repos that pre-date the column. Older databases
        # were created without it; treat their existing rows as 'export'.
        cur.execute("SELECT DATABASE()")
        (db,) = cur.fetchone()  # type: ignore[misc]
        for table in TABLES_WITH_SOURCE:
            cur.execute(
                "SELECT COUNT(*) FROM information_schema.columns "
                "WHERE table_schema = %s AND table_name = %s AND column_name = 'source'",
                (db, table),
            )
            (present,) = cur.fetchone()  # type: ignore[misc]
            if not present:
                cur.execute(
                    f"ALTER TABLE {table} ADD COLUMN source VARCHAR(16) NOT NULL DEFAULT 'export'"
                )
