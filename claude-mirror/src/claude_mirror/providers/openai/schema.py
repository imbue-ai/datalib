from __future__ import annotations

from pymysql.connections import Connection

# Mirror of providers/anthropic/schema.py for ChatGPT web data.
#
# Shape note: ChatGPT's `/backend-api/conversation/{id}` returns a `mapping`
# dict (DAG of nodes) keyed by node id, where each node has {id, parent,
# children, message?}. We flatten that into `openai_messages` (one row per
# node that carries a message) with `parent_id` from the node's parent
# pointer. The conversation's "current" path is preserved via
# `openai_conversations.current_node` rather than encoded in the row order.
#
# Every row carries a `source` column ('export' | 'api'). For now ChatGPT
# only has a web-API transport (there's no equivalent of Anthropic's bulk
# zip), so all rows here are sourced 'api'. We keep the column for symmetry
# and so a future export ingest can land here without a schema change.

TABLES_WITH_SOURCE = (
    "openai_accounts",
    "openai_conversations",
    "openai_messages",
    "openai_content_parts",
)

DDL: list[str] = [
    """
    CREATE TABLE IF NOT EXISTS openai_accounts (
        account_id      VARCHAR(64)  NOT NULL,
        email           VARCHAR(320),
        name            VARCHAR(255),
        raw_json        JSON         NOT NULL,
        source          VARCHAR(16)  NOT NULL DEFAULT 'api',
        first_seen_at   VARCHAR(40)  NOT NULL,
        last_seen_at    VARCHAR(40)  NOT NULL,
        PRIMARY KEY (account_id)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS openai_conversations (
        account_id          VARCHAR(64),
        conversation_id     VARCHAR(64)  NOT NULL,
        title               VARCHAR(1024),
        create_time         VARCHAR(40),
        update_time         VARCHAR(40),
        current_node        VARCHAR(64),
        default_model_slug  VARCHAR(128),
        gizmo_id            VARCHAR(128),
        gizmo_type          VARCHAR(64),
        is_archived         BOOLEAN,
        is_starred          BOOLEAN,
        raw_json            JSON         NOT NULL,
        source              VARCHAR(16)  NOT NULL DEFAULT 'api',
        last_seen_at        VARCHAR(40)  NOT NULL,
        PRIMARY KEY (conversation_id)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS openai_messages (
        conversation_id  VARCHAR(64)  NOT NULL,
        message_id       VARCHAR(64)  NOT NULL,
        parent_id        VARCHAR(64),
        role             VARCHAR(32),
        recipient        VARCHAR(64),
        channel          VARCHAR(64),
        content_type     VARCHAR(64),
        text             LONGTEXT,
        status           VARCHAR(64),
        end_turn         BOOLEAN,
        weight           DOUBLE,
        model_slug       VARCHAR(128),
        create_time      VARCHAR(40),
        update_time      VARCHAR(40),
        raw_json         JSON         NOT NULL,
        source           VARCHAR(16)  NOT NULL DEFAULT 'api',
        last_seen_at     VARCHAR(40)  NOT NULL,
        PRIMARY KEY (message_id)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS openai_content_parts (
        message_id    VARCHAR(64)  NOT NULL,
        part_index    INT          NOT NULL,
        kind          VARCHAR(32),
        language      VARCHAR(64),
        text          LONGTEXT,
        raw_json      JSON         NOT NULL,
        source        VARCHAR(16)  NOT NULL DEFAULT 'api',
        PRIMARY KEY (message_id, part_index)
    )
    """,
]


def ensure_schema(conn: Connection) -> None:
    with conn.cursor() as cur:
        for stmt in DDL:
            cur.execute(stmt)
        # Backfill `source` on repos that pre-date the column. (None today,
        # but kept for parity with the anthropic schema.)
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
                    f"ALTER TABLE {table} ADD COLUMN source VARCHAR(16) NOT NULL DEFAULT 'api'"
                )
