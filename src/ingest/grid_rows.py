"""Build the `grid_rows` union projection from parsed provider data.

`grid_rows` is the only structured table that survives ingest — it backs
the AG Grid in frankweiler. Per-provider tables no longer exist; this
module reads the in-memory `Parsed*` dataclasses directly.

Schema (column names, types, per-provider mappings) lives in
`schemas/grid_rows.schema.json` — codegen produces matching Python /
Rust / TypeScript artifacts. See `docs/grid_rows.md` for the
architecture overview.

Re-population strategy: full delete + reinsert on every ingest. Cheap at
our scale (~5k rows), avoids row-level UPSERT complexity, and guarantees
consistency with any mapping changes.
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timedelta
from typing import Iterable

from pymysql.connections import Connection

from ingest.generated_grid_rows import COLUMNS, DDL
from ingest.providers.anthropic.parse import ParsedExport
from ingest.providers.openai.parse import ParsedChatGPTApi
from ingest.providers.slack.parse import ParsedSlackApi
from ingest.render import _slugify

_GRID_ROWS_COLUMNS = COLUMNS["grid_rows"]


def ensure_schema(conn: Connection) -> None:
    """(Re)create the grid_rows table. Drops first so schema changes
    (new columns, retyped columns) take effect even when the underlying
    Dolt repo persists between ingest runs — grid_rows is fully derived
    from the parsed provider data, so dropping is always safe."""
    with conn.cursor() as cur:
        cur.execute("DROP TABLE IF EXISTS grid_rows")
        for stmt in DDL:
            cur.execute(stmt)


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
    qmd_path: str | None


def _bump_micros(ts: str, n: int) -> str:
    """Add `n` microseconds to an ISO-8601 timestamp string, preserving
    the explicit offset suffix. Falls back to returning the input
    unchanged if the format isn't recognized — synthetic ordering is
    best-effort, matching the Rust `bump_micros` in db.rs."""
    if not ts:
        return ts
    s = ts.replace("Z", "+00:00") if ts.endswith("Z") else ts
    try:
        dt = datetime.fromisoformat(s)
    except ValueError:
        return ts
    bumped = dt + timedelta(microseconds=n)
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


# ----- qmd path computation -------------------------------------------------
#
# The grid row points the preview pane at a specific QMD on disk by carrying
# the file's path (relative to the data root) in `qmd_path`. We compute the
# path by mirroring exactly what the renderer in `ingest/render.py` writes,
# so every grid row knows the file without globbing or frontmatter scanning.


def _anthropic_qmd_path(
    account_uuid: str | None, conversation_uuid: str, name: str | None
) -> str:
    return (
        f"anthropic/{account_uuid}/llm_chats/{conversation_uuid}__{_slugify(name)}.qmd"
    )


def _openai_qmd_path(
    account_id: str | None, conversation_id: str, title: str | None
) -> str:
    return f"openai/{account_id or 'unknown'}/llm_chats/{conversation_id}__{_slugify(title)}.qmd"


def _slack_thread_title(root_text: str | None) -> str:
    snippet = (root_text or "").strip().splitlines()
    title = snippet[0] if snippet else "(empty thread)"
    return title[:80]


def _slack_qmd_path(
    team_id: str, channel_name: str, thread_uuid: str, root_text: str | None
) -> str:
    return f"slack/{team_id}/{channel_name}/threads/{thread_uuid}__{_slugify(_slack_thread_title(root_text))}.qmd"


def _slack_link(team_id: str, channel_id: str, ts: str) -> str:
    ts_no_dot = ts.replace(".", "")
    return f"https://slack.com/archives/{channel_id}/p{ts_no_dot}?team={team_id}"


# ----- Anthropic ------------------------------------------------------------


def _anthropic_rows(parsed: ParsedExport) -> Iterable[_Row]:
    convs = {c.conversation_uuid: c for c in parsed.conversations}
    msgs_by_conv: dict[str, list] = {}
    for m in parsed.messages:
        msgs_by_conv.setdefault(m.conversation_uuid, []).append(m)
    blocks_by_msg: dict[str, list] = {}
    for b in parsed.content_blocks:
        blocks_by_msg.setdefault(b.message_uuid, []).append(b)

    # Chat rows.
    for c in parsed.conversations:
        when = c.created_at or c.updated_at or ""
        text = c.summary or c.name or ""
        yield _Row(
            uuid=c.conversation_uuid,
            provider="anthropic",
            kind="Chat",
            source_label="Claude",
            when_ts=when,
            author=None,
            account=c.account_uuid,
            project=c.project_uuid,
            channel=None,
            conversation_name=c.name,
            conversation_uuid=c.conversation_uuid,
            message_index=None,
            entire_chat=f"/chat/{c.conversation_uuid}",
            text=text,
            slack_link=None,
            qmd_path=_anthropic_qmd_path(c.account_uuid, c.conversation_uuid, c.name),
        )

    # Message + block rows. Index messages within their conversation by
    # (created_at, message_uuid) to mirror the renderer's order.
    for cuuid, conv in convs.items():
        msgs = sorted(
            msgs_by_conv.get(cuuid, []),
            key=lambda m: (m.created_at or "", m.message_uuid),
        )
        model = (conv.raw_json or {}).get("model") or ""
        for msg_idx, m in enumerate(msgs):
            kind = _anthropic_kind_for_sender(m.sender or "")
            if kind == "User Input":
                author = conv.account_uuid
            elif kind == "LLM Response":
                author = model or m.sender
            else:
                author = m.sender
            yield _Row(
                uuid=m.message_uuid,
                provider="anthropic",
                kind=kind,
                source_label="Claude",
                when_ts=m.created_at or "",
                author=author,
                account=conv.account_uuid,
                project=conv.project_uuid,
                channel=None,
                conversation_name=conv.name,
                conversation_uuid=cuuid,
                message_index=msg_idx,
                entire_chat=f"/chat/{cuuid}",
                text=m.text or "",
                slack_link=None,
                qmd_path=_anthropic_qmd_path(conv.account_uuid, cuuid, conv.name),
            )

            for b in sorted(
                blocks_by_msg.get(m.message_uuid, []), key=lambda x: x.block_index
            ):
                if b.type not in ("tool_use", "tool_result", "thinking"):
                    continue
                btext = b.text or ""
                if not btext and b.type == "thinking":
                    btext = (b.raw_json or {}).get("thinking") or ""
                row_when = b.start_timestamp or _bump_micros(
                    m.created_at or "", b.block_index + 1
                )
                yield _Row(
                    uuid=f"{m.message_uuid}:{b.block_index}",
                    provider="anthropic",
                    kind=_anthropic_kind_for_block(b.type or ""),
                    source_label="Claude",
                    when_ts=row_when or "",
                    author=model or b.type or "",
                    account=conv.account_uuid,
                    project=conv.project_uuid,
                    channel=None,
                    conversation_name=conv.name,
                    conversation_uuid=cuuid,
                    message_index=msg_idx,
                    entire_chat=f"/chat/{cuuid}",
                    text=btext or (b.type or ""),
                    slack_link=None,
                    qmd_path=_anthropic_qmd_path(conv.account_uuid, cuuid, conv.name),
                )


# ----- OpenAI ---------------------------------------------------------------


def _openai_rows(parsed: ParsedChatGPTApi) -> Iterable[_Row]:
    msgs_by_conv: dict[str, list] = {}
    for m in parsed.messages:
        msgs_by_conv.setdefault(m.conversation_id, []).append(m)

    for c in parsed.conversations:
        when = c.create_time or c.update_time or ""
        yield _Row(
            uuid=c.conversation_id,
            provider="openai",
            kind="Chat",
            source_label="ChatGPT",
            when_ts=when,
            author=None,
            account=c.account_id,
            project=None,
            channel=None,
            conversation_name=c.title,
            conversation_uuid=c.conversation_id,
            message_index=None,
            entire_chat=f"/chat/{c.conversation_id}",
            text=c.title or "",
            slack_link=None,
            qmd_path=_openai_qmd_path(c.account_id, c.conversation_id, c.title),
        )

        msgs = sorted(
            msgs_by_conv.get(c.conversation_id, []),
            key=lambda m: (m.create_time or "", m.message_id),
        )
        conv_time = c.create_time or c.update_time or ""
        for msg_idx, m in enumerate(msgs):
            kind = _openai_kind_for_role_and_type(m.role or "", m.content_type or "")
            if kind == "User Input":
                author = c.account_id
            elif kind in ("LLM Response", "LLM Thinking"):
                author = m.model_slug or m.role
            else:
                author = m.role
            row_when = m.create_time or _bump_micros(conv_time, msg_idx + 1)
            yield _Row(
                uuid=m.message_id,
                provider="openai",
                kind=kind,
                source_label="ChatGPT",
                when_ts=row_when or "",
                author=author,
                account=c.account_id,
                project=None,
                channel=None,
                conversation_name=c.title,
                conversation_uuid=c.conversation_id,
                message_index=msg_idx,
                entire_chat=f"/chat/{c.conversation_id}",
                text=m.text or "",
                slack_link=None,
                qmd_path=_openai_qmd_path(c.account_id, c.conversation_id, c.title),
            )


# ----- Slack ----------------------------------------------------------------


def _slack_rows(parsed: ParsedSlackApi) -> Iterable[_Row]:
    if not parsed.messages:
        return
    channels = {c.channel_id: c for c in parsed.channels}
    user_labels: dict[str, str] = {
        u.user_id: (u.real_name or u.name or u.user_id) for u in parsed.users
    }

    msgs_by_thread: dict[str, list] = {}
    for m in parsed.messages:
        msgs_by_thread.setdefault(m.thread_uuid, []).append(m)

    for thread_uuid, msgs in msgs_by_thread.items():
        msgs = sorted(msgs, key=lambda m: (m.ts_iso, m.ts))
        root_msg = next((m for m in msgs if m.is_thread_root), msgs[0])
        ch = channels.get(root_msg.channel_id)
        cname = (ch.name if ch and ch.name else None) or root_msg.channel_id
        author = user_labels.get(root_msg.user_id or "", root_msg.user_id)

        # Thread row.
        yield _Row(
            uuid=thread_uuid,
            provider="slack",
            kind="Slack Thread",
            source_label="Slack",
            when_ts=root_msg.ts_iso or "",
            author=author,
            account=root_msg.team_id,
            project=None,
            channel=cname,
            conversation_name=f"#{cname}",
            conversation_uuid=thread_uuid,
            message_index=None,
            entire_chat=f"/slack/{thread_uuid}",
            text=root_msg.text or "",
            slack_link=_slack_link(root_msg.team_id, root_msg.channel_id, root_msg.ts),
            qmd_path=_slack_qmd_path(
                root_msg.team_id, cname, thread_uuid, root_msg.text
            ),
        )

        # Message rows.
        for msg_idx, m in enumerate(msgs):
            mauthor = user_labels.get(m.user_id or "", m.user_id)
            yield _Row(
                uuid=m.uuid,
                provider="slack",
                kind="Slack Message",
                source_label="Slack",
                when_ts=m.ts_iso or "",
                author=mauthor,
                account=m.team_id,
                project=None,
                channel=cname,
                conversation_name=f"#{cname}",
                conversation_uuid=thread_uuid,
                message_index=msg_idx,
                entire_chat=f"/slack/{thread_uuid}",
                text=m.text or "",
                slack_link=_slack_link(m.team_id, m.channel_id, m.ts),
                qmd_path=_slack_qmd_path(
                    root_msg.team_id, cname, thread_uuid, root_msg.text
                ),
            )


# ----- entry point ----------------------------------------------------------


def populate_grid_rows(
    conn: Connection,
    anthropic: ParsedExport | None,
    openai: ParsedChatGPTApi | None,
    slack: ParsedSlackApi | None,
) -> int:
    """Truncate `grid_rows` and re-emit every row from the parsed provider
    data. Returns the number of rows inserted."""
    ensure_schema(conn)
    rows: list[_Row] = []
    if anthropic is not None:
        rows.extend(_anthropic_rows(anthropic))
    if openai is not None:
        rows.extend(_openai_rows(openai))
    if slack is not None:
        rows.extend(_slack_rows(slack))

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
                        r.qmd_path,
                    )
                    for r in rows
                ],
            )
    return len(rows)
