from __future__ import annotations

import json
import re
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path

from pymysql.connections import Connection


def _bump_iso(ts: str) -> str:
    """Return ISO timestamp `ts` advanced by one microsecond.

    Used to give synthetic ordering to messages that arrive with no
    timestamp of their own (e.g. ChatGPT tool/system messages): each
    inherits the previous message's timestamp plus a tiny epsilon so
    they sort stably in downstream views without colliding."""
    s = ts.replace("Z", "+00:00") if ts.endswith("Z") else ts
    try:
        dt = datetime.fromisoformat(s)
    except ValueError:
        return ts
    bumped = (dt + timedelta(microseconds=1)).isoformat()
    if ts.endswith("Z") and bumped.endswith("+00:00"):
        bumped = bumped[:-6] + "Z"
    return bumped

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


# Tool-block rendering pattern (collapsible <details>, JSON-pretty input,
# string-or-JSON result body) cribbed from marcheiligers/ccexport's
# format_tool_use in lib/claude_conversation_exporter.rb.
# https://github.com/marcheiligers/ccexport
def _render_anthropic_block(
    msg_uuid: str,
    block_index: int,
    btype: str | None,
    btext: str | None,
    braw: object,
) -> list[str]:
    raw = braw if isinstance(braw, dict) else (json.loads(braw) if braw else {})
    # Block-scoped anchor; tool blocks also get a content-id anchor below
    # so tool_result can link back to its tool_use across messages.
    anchors = [f'<a id="b-{msg_uuid}-{block_index}"></a>']
    if btype == "tool_use" and raw.get("id"):
        anchors.append(f'<a id="tu-{raw["id"]}"></a>')
    elif btype == "tool_result" and raw.get("tool_use_id"):
        anchors.append(f'<a id="tr-{raw["tool_use_id"]}"></a>')
    head = "".join(anchors)

    if btype == "text" and btext:
        return [head + btext.rstrip(), ""]

    if btype == "thinking":
        thought = (raw.get("thinking") if isinstance(raw, dict) else None) or btext
        if not thought:
            return [f"{head}<!-- thinking (no text) -->", ""]
        quoted = "> " + str(thought).rstrip().replace("\n", "\n> ")
        return [
            f"{head}<details><summary>Thinking</summary>",
            "",
            quoted,
            "",
            "</details>",
            "",
        ]

    if btype == "tool_use":
        name = raw.get("name") or "tool"
        msg = raw.get("message")
        tool_input = raw.get("input")
        summary = f"Tool use: {name}" + (f" — {msg}" if msg else "")
        out = [f"{head}<details><summary>{summary}</summary>", ""]
        if tool_input:
            out.append("```json")
            out.append(json.dumps(tool_input, indent=2, ensure_ascii=False))
            out.append("```")
        out.extend(["</details>", ""])
        return out

    if btype == "tool_result":
        name = raw.get("name") or "tool"
        is_err = raw.get("is_error")
        content = raw.get("content")
        summary = f"Tool result: {name}" + (" (error)" if is_err else "")
        out = [f"{head}<details><summary>{summary}</summary>", ""]
        if isinstance(content, str):
            out += ["```", content.rstrip(), "```"]
        elif isinstance(content, list):
            for item in content:
                if isinstance(item, dict) and item.get("type") == "text" and item.get("text"):
                    out += [str(item["text"]).rstrip(), ""]
                elif isinstance(item, dict):
                    out += ["```json", json.dumps(item, indent=2, ensure_ascii=False), "```", ""]
                else:
                    out += ["```", str(item).rstrip(), "```", ""]
        elif content is not None:
            out += ["```json", json.dumps(content, indent=2, ensure_ascii=False), "```"]
        out.extend(["</details>", ""])
        return out

    if btext:
        return [head, f"```{btype or ''}".rstrip(), btext.rstrip(), "```", ""]

    return [f"{head}<!-- {btype or 'block'} (no text) -->", ""]


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

    last_ts = created_at
    with conn.cursor() as cur:
        for msg_uuid, sender, msg_created in messages:
            if not msg_created and last_ts:
                msg_created = _bump_iso(last_ts)
            if msg_created:
                last_ts = msg_created
            heading = (sender or "unknown").capitalize()
            parts.append(f'<a id="m-{msg_uuid}"></a>')
            parts.append(f"## {heading}")
            if msg_created:
                parts.append("")
                parts.append(f"*{msg_created}*")
            parts.append("")
            cur.execute(
                """
                SELECT block_index, type, text, raw_json
                FROM anthropic_content_blocks
                WHERE message_uuid = %s
                ORDER BY block_index
                """,
                (msg_uuid,),
            )
            blocks = list(cur.fetchall())
            if blocks:
                for bidx, btype, btext, braw in blocks:
                    parts.extend(
                        _render_anthropic_block(msg_uuid, bidx, btype, btext, braw)
                    )
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


def _table_exists(conn: Connection, name: str) -> bool:
    with conn.cursor() as cur:
        cur.execute("SELECT DATABASE()")
        (db,) = cur.fetchone()  # type: ignore[misc]
        cur.execute(
            "SELECT COUNT(*) FROM information_schema.tables "
            "WHERE table_schema = %s AND table_name = %s",
            (db, name),
        )
        (n,) = cur.fetchone()  # type: ignore[misc]
        return bool(n)


def render_openai_conversation(conn: Connection, conversation_id: str,
                               root: Path) -> Path:
    """Render one ChatGPT conversation into QMD.

    The DAG is collapsed to the chat's "current" leaf-to-root path
    (`current_node` walked back through `parent_id`), so the rendered file
    matches what the user actually sees in chatgpt.com — siblings/edits
    that were not in the kept branch are not emitted, but they remain in
    Dolt for later analysis."""
    with conn.cursor() as cur:
        cur.execute(
            """
            SELECT account_id, conversation_id, title, create_time, update_time,
                   current_node, default_model_slug
            FROM openai_conversations
            WHERE conversation_id = %s
            """,
            (conversation_id,),
        )
        row = cur.fetchone()
        if not row:
            raise KeyError(f"conversation not found: {conversation_id}")
        account_id, _, title, create_time, update_time, current_node, model_slug = row

        cur.execute(
            """
            SELECT message_id, parent_id, role, content_type, text,
                   create_time, model_slug
            FROM openai_messages
            WHERE conversation_id = %s
            """,
            (conversation_id,),
        )
        msgs = {r[0]: r for r in cur.fetchall()}

    # Walk current_node → root via parent_id to get the displayed path.
    path: list[tuple] = []
    seen: set[str] = set()
    cursor = current_node
    while cursor and cursor in msgs and cursor not in seen:
        seen.add(cursor)
        path.append(msgs[cursor])
        cursor = msgs[cursor][1]  # parent_id
    path.reverse()
    # If current_node is None or its message isn't recorded (e.g. it points
    # at a node with no `message`, like the synthetic root), fall back to
    # rendering messages in create_time order so we still surface content.
    if not path:
        path = sorted(msgs.values(), key=lambda r: r[5] or "")

    out_dir = root / "openai" / (account_id or "unknown") / "llm_chats"
    out_dir.mkdir(parents=True, exist_ok=True)
    slug = _slugify(title)
    target = out_dir / f"{conversation_id}__{slug}.qmd"
    for existing in out_dir.glob(f"{conversation_id}__*.qmd"):
        if existing != target:
            existing.unlink()

    parts: list[str] = []
    parts.append("---")
    parts.append("provider: openai")
    parts.append(f"id: {_yaml_scalar(conversation_id)}")
    parts.append(f"title: {_yaml_scalar(title)}")
    parts.append(f"account_id: {_yaml_scalar(account_id)}")
    parts.append(f"create_time: {_yaml_scalar(create_time)}")
    parts.append(f"update_time: {_yaml_scalar(update_time)}")
    if model_slug:
        parts.append(f"default_model_slug: {_yaml_scalar(model_slug)}")
    parts.append("---")
    parts.append("")
    parts.append(f"# {title or '(untitled)'}")
    parts.append("")

    last_ts = create_time
    with conn.cursor() as cur:
        for msg_id, _parent, role, content_type, text, msg_created, msg_model in path:
            # Skip system / model_editable_context fluff in the rendered
            # markdown; it's still in Dolt if we ever need it.
            if role == "system" or content_type == "model_editable_context":
                continue
            if not msg_created and last_ts:
                msg_created = _bump_iso(last_ts)
            if msg_created:
                last_ts = msg_created
            heading = (role or "unknown").capitalize()
            parts.append(f'<a id="m-{msg_id}"></a>')
            parts.append(f"## {heading}")
            meta_bits = []
            if msg_created:
                meta_bits.append(msg_created)
            if msg_model:
                meta_bits.append(msg_model)
            if meta_bits:
                parts.append("")
                parts.append("*" + " · ".join(meta_bits) + "*")
            parts.append("")
            cur.execute(
                """
                SELECT part_index, kind, language, text
                FROM openai_content_parts
                WHERE message_id = %s
                ORDER BY part_index
                """,
                (msg_id,),
            )
            for pidx, kind, language, ptext in cur.fetchall():
                if not ptext and kind not in ("execution_output", "code"):
                    continue
                anchor = f'<a id="b-{msg_id}-{pidx}"></a>'
                if kind == "text":
                    parts.append(anchor + (ptext or "").rstrip())
                    parts.append("")
                elif kind == "code":
                    parts.append(anchor)
                    parts.append(f"```{language or ''}".rstrip())
                    parts.append((ptext or "").rstrip())
                    parts.append("```")
                    parts.append("")
                elif kind == "execution_output":
                    parts.append(anchor)
                    parts.append("```")
                    parts.append((ptext or "").rstrip())
                    parts.append("```")
                    parts.append("")
                elif kind in ("thoughts", "reasoning_recap"):
                    parts.append(f"{anchor}<!-- {kind} -->")
                    parts.append("> " + (ptext or "").replace("\n", "\n> "))
                    parts.append("")
                else:
                    parts.append(f"{anchor}<!-- {kind} -->")
                    parts.append((ptext or "").rstrip())
                    parts.append("")

    body = "\n".join(parts).rstrip() + "\n"
    target.write_text(body)
    return target


def _write_accounts_index(conn: Connection, root: Path) -> None:
    """Emit `<root>/accounts.json` mapping account UUIDs → human labels.

    Read very late by the UI to render display names instead of opaque
    UUIDs in Author/Account columns. Keeping the lookup file alongside
    the QMDs lets the backend stay file-only without having to talk
    to Dolt."""
    accounts: dict[str, dict] = {}
    if _table_exists(conn, "anthropic_accounts"):
        with conn.cursor() as cur:
            cur.execute("SELECT account_uuid, full_name, email FROM anthropic_accounts")
            for uuid, full_name, email in cur.fetchall():
                accounts[uuid] = {
                    "provider": "anthropic",
                    "label": full_name or email or uuid,
                    "email": email,
                }
    if _table_exists(conn, "openai_accounts"):
        with conn.cursor() as cur:
            cur.execute("SELECT account_id, name, email FROM openai_accounts")
            for uid, name, email in cur.fetchall():
                accounts[uid] = {
                    "provider": "openai",
                    "label": name or email or uid,
                    "email": email,
                }
    root.mkdir(parents=True, exist_ok=True)
    (root / "accounts.json").write_text(
        json.dumps(accounts, indent=2, sort_keys=True) + "\n"
    )


def render_all(conn: Connection, root: Path) -> RenderSummary:
    summary = RenderSummary()

    if _table_exists(conn, "anthropic_conversations"):
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

        for acct in accounts:
            chats_dir = root / "anthropic" / acct / "llm_chats"
            if not chats_dir.is_dir():
                continue
            for f in chats_dir.glob("*.qmd"):
                uuid_prefix = f.name.split("__", 1)[0]
                if uuid_prefix not in live_uuids:
                    f.unlink()
                    summary.orphans_removed += 1

    if _table_exists(conn, "openai_conversations"):
        with conn.cursor() as cur:
            cur.execute(
                "SELECT conversation_id, account_id FROM openai_conversations"
            )
            rows = list(cur.fetchall())
        live_ids: set[str] = set()
        accts: set[str] = set()
        for cid, acct in rows:
            live_ids.add(cid)
            accts.add(acct or "unknown")
            render_openai_conversation(conn, cid, root)
            summary.rendered += 1
        for acct in accts:
            chats_dir = root / "openai" / acct / "llm_chats"
            if not chats_dir.is_dir():
                continue
            for f in chats_dir.glob("*.qmd"):
                id_prefix = f.name.split("__", 1)[0]
                if id_prefix not in live_ids:
                    f.unlink()
                    summary.orphans_removed += 1

    _write_accounts_index(conn, root)

    return summary
