from __future__ import annotations

from pymysql.connections import Connection

# Dolt's JSON support is solid in 1.x; we use JSON for raw_json columns.
# Timestamps from the export are ISO-8601 strings — we store them as VARCHAR
# verbatim to avoid lossy parsing; downstream readers can cast as needed.

DDL: list[str] = [
    """
    CREATE TABLE IF NOT EXISTS anthropic_accounts (
        account_uuid    VARCHAR(64)  NOT NULL,
        email           VARCHAR(320),
        full_name       VARCHAR(255),
        raw_json        JSON         NOT NULL,
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
        PRIMARY KEY (message_uuid, block_index)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS anthropic_attachments (
        message_uuid     VARCHAR(64)  NOT NULL,
        attachment_index INT         NOT NULL,
        kind             VARCHAR(32) NOT NULL,
        raw_json         JSON         NOT NULL,
        PRIMARY KEY (message_uuid, attachment_index, kind)
    )
    """,
]


def ensure_schema(conn: Connection) -> None:
    with conn.cursor() as cur:
        for stmt in DDL:
            cur.execute(stmt)
