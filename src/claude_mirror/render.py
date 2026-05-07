from __future__ import annotations

import json
import re
from dataclasses import dataclass
from pathlib import Path

from pymysql.connections import Connection

SLUG_MAX_LEN = 60
_SLUG_RE = re.compile(r"[^a-z0-9]+")


def _slugify(name: str | None) -> str:
    if not name:
        return "untitled"
    s = _SLUG_RE.sub("-", name.lower()).strip("-")
    if not s:
        return "untitled"
    return s[:SLUG_MAX_LEN].rstrip("-") or "untitled"


def _yaml_scalar(v: object) -> str:
    if v is None:
        return "null"
    s = str(v)
    if any(c in s for c in ":#\n\"'") or s != s.strip():
        return json.dumps(s, ensure_ascii=False)
    return s


@dataclass
class RenderSummary:
    rendered: int = 0
    orphans_removed: int = 0


def render_conversation(conn: Connection, conversation_uuid: str, root: Path) -> Path:
    with conn.cursor() as cur:
        cur.execute(
            """
            SELECT account_uuid, conversation_uuid, project_uuid, name, summary,
                   created_at, updated_at
            FROM anthropic_conversations
            WHERE conversation_uuid = %s
            """,
            (conversation_uuid,),
        )
        row = cur.fetchone()
        if not row:
            raise KeyError(f"conversation not found: {conversation_uuid}")
        account_uuid, _, project_uuid, name, summary, created_at, updated_at = row

        cur.execute(
            """
            SELECT message_uuid, sender, created_at
            FROM anthropic_messages
            WHERE conversation_uuid = %s
            ORDER BY created_at, message_uuid
            """,
            (conversation_uuid,),
        )
        messages = list(cur.fetchall())

    out_dir = root / "anthropic" / account_uuid / "llm_chats"
    out_dir.mkdir(parents=True, exist_ok=True)

    slug = _slugify(name)
    target = out_dir / f"{conversation_uuid}__{slug}.qmd"

    # If an older file exists with the same UUID prefix but different slug, remove it.
    for existing in out_dir.glob(f"{conversation_uuid}__*.qmd"):
        if existing != target:
            existing.unlink()

    parts: list[str] = []
    parts.append("---")
    parts.append(f"provider: anthropic")
    parts.append(f"uuid: {_yaml_scalar(conversation_uuid)}")
    parts.append(f"name: {_yaml_scalar(name)}")
    parts.append(f"account_uuid: {_yaml_scalar(account_uuid)}")
    parts.append(f"project_uuid: {_yaml_scalar(project_uuid)}")
    parts.append(f"created_at: {_yaml_scalar(created_at)}")
    parts.append(f"updated_at: {_yaml_scalar(updated_at)}")
    if summary:
        parts.append(f"summary: {_yaml_scalar(summary)}")
    parts.append("---")
    parts.append("")
    parts.append(f"# {name or '(untitled)'}")
    parts.append("")

    with conn.cursor() as cur:
        for msg_uuid, sender, msg_created in messages:
            heading = (sender or "unknown").capitalize()
            parts.append(f"## {heading}")
            if msg_created:
                parts.append("")
                parts.append(f"*{msg_created}*")
            parts.append("")
            cur.execute(
                """
                SELECT block_index, type, text
                FROM anthropic_content_blocks
                WHERE message_uuid = %s
                ORDER BY block_index
                """,
                (msg_uuid,),
            )
            blocks = list(cur.fetchall())
            if blocks:
                for _, btype, btext in blocks:
                    if btype == "text" and btext:
                        parts.append(btext.rstrip())
                        parts.append("")
                    elif btext:
                        parts.append(f"```{btype or ''}".rstrip())
                        parts.append(btext.rstrip())
                        parts.append("```")
                        parts.append("")
                    else:
                        parts.append(f"<!-- {btype or 'block'} (no text) -->")
                        parts.append("")
            cur.execute(
                """
                SELECT attachment_index, kind, raw_json
                FROM anthropic_attachments
                WHERE message_uuid = %s
                ORDER BY attachment_index
                """,
                (msg_uuid,),
            )
            atts = list(cur.fetchall())
            if atts:
                parts.append("**Attachments:**")
                parts.append("")
                for _, kind, raw in atts:
                    raw_obj = raw if isinstance(raw, dict) else json.loads(raw)
                    label = (
                        raw_obj.get("file_name")
                        or raw_obj.get("name")
                        or raw_obj.get("file_kind")
                        or "(unnamed)"
                    )
                    parts.append(f"- [{kind}] {label}")
                parts.append("")

    body = "\n".join(parts).rstrip() + "\n"
    target.write_text(body)
    return target


def render_all(conn: Connection, root: Path) -> RenderSummary:
    summary = RenderSummary()
    with conn.cursor() as cur:
        cur.execute(
            "SELECT conversation_uuid, account_uuid FROM anthropic_conversations"
        )
        rows = list(cur.fetchall())

    live_uuids: set[str] = set()
    accounts: set[str] = set()
    for conv_uuid, acct in rows:
        live_uuids.add(conv_uuid)
        accounts.add(acct)
        render_conversation(conn, conv_uuid, root)
        summary.rendered += 1

    # GC orphans per account dir.
    for acct in accounts:
        chats_dir = root / "anthropic" / acct / "llm_chats"
        if not chats_dir.is_dir():
            continue
        for f in chats_dir.glob("*.qmd"):
            uuid_prefix = f.name.split("__", 1)[0]
            if uuid_prefix not in live_uuids:
                f.unlink()
                summary.orphans_removed += 1
    return summary
