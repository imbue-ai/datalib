from __future__ import annotations

import json
import logging
import re
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path

from pymysql.connections import Connection
from tqdm import tqdm

from ingest.providers.slack.mrkdwn import to_commonmark as _slack_to_commonmark

log = logging.getLogger(__name__)


def _msg_div_open(msg_uuid: str, msg_index: int, provider: str) -> str:
    """Per-message wrapper used by the chat preview/detail views.

    The UI renders the QMD body verbatim (no parsing); each message becomes
    a `<div id="m-{uuid}" data-msg-index="{i}" class="msg msg--{provider}">`
    so the UI can scroll to and highlight a specific message by uuid OR by
    index without round-tripping through a structured chat schema. Blank
    line on either side keeps the inner content rendered as CommonMark
    (CommonMark HTML-block-type-6 ends at the blank line)."""
    return (
        f'<div id="m-{msg_uuid}" data-msg-index="{msg_index}" '
        f'class="msg msg--{provider}">'
    )


_MSG_DIV_CLOSE = "</div>"


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
    if isinstance(braw, dict):
        raw = braw
    elif isinstance(braw, (str, bytes, bytearray)) and braw:
        raw = json.loads(braw)
    else:
        raw = {}
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
                if (
                    isinstance(item, dict)
                    and item.get("type") == "text"
                    and item.get("text")
                ):
                    out += [str(item["text"]).rstrip(), ""]
                elif isinstance(item, dict):
                    out += [
                        "```json",
                        json.dumps(item, indent=2, ensure_ascii=False),
                        "```",
                        "",
                    ]
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
    parts.append("provider: anthropic")
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
        for msg_index, (msg_uuid, sender, msg_created) in enumerate(messages):
            if not msg_created and last_ts:
                msg_created = _bump_iso(last_ts)
            if msg_created:
                last_ts = msg_created
            heading = (sender or "unknown").capitalize()
            parts.append(_msg_div_open(msg_uuid, msg_index, "anthropic"))
            parts.append("")
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
            parts.append(_MSG_DIV_CLOSE)
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


def render_openai_conversation(
    conn: Connection, conversation_id: str, root: Path
) -> Path:
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
    msg_index = 0
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
            parts.append(_msg_div_open(msg_id, msg_index, "openai"))
            parts.append("")
            parts.append(f"## {heading}")
            msg_index += 1
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
            parts.append(_MSG_DIV_CLOSE)
            parts.append("")

    body = "\n".join(parts).rstrip() + "\n"
    target.write_text(body)
    return target


def _slack_message_link(team_id: str, channel_id: str, ts: str) -> str:
    """Slack web deep-link of the form
    `https://slack.com/archives/{channel}/p{ts_no_dot}`. The `team` param
    is appended so cross-workspace clicks land in the right org."""
    ts_no_dot = ts.replace(".", "")
    return f"https://slack.com/archives/{channel_id}/p{ts_no_dot}?team={team_id}"


def render_slack_thread(conn: Connection, thread_uuid: str, root: Path) -> Path:
    """Render one Slack thread (one root + zero-or-more replies) into a
    single QMD. Standalone messages without replies are still threads of
    length 1, so this function handles them too."""
    with conn.cursor() as cur:
        cur.execute(
            """
            SELECT m.uuid, m.team_id, m.channel_id, m.ts, m.thread_ts, m.user_id,
                   m.text, m.ts_iso, m.is_thread_root, c.name AS channel_name
            FROM slack_messages m
            JOIN slack_channels c ON c.channel_id = m.channel_id
            WHERE m.thread_uuid = %s
            ORDER BY m.ts_iso, m.ts
            """,
            (thread_uuid,),
        )
        rows = list(cur.fetchall())
        if not rows:
            raise KeyError(f"slack thread not found: {thread_uuid}")

        # Resolve user_id -> display label once for the whole thread.
        user_ids = {r[5] for r in rows if r[5]}
        user_labels: dict[str, str] = {}
        if user_ids:
            placeholders = ",".join(["%s"] * len(user_ids))
            cur.execute(
                f"SELECT user_id, real_name, name FROM slack_users "
                f"WHERE user_id IN ({placeholders})",
                tuple(user_ids),
            )
            for uid, real_name, name in cur.fetchall():
                user_labels[uid] = real_name or name or uid

        cur.execute(
            """
            SELECT message_uuid, name, user_id
            FROM slack_reactions
            WHERE message_uuid IN (
                SELECT uuid FROM slack_messages WHERE thread_uuid = %s
            )
            ORDER BY message_uuid, name, user_id
            """,
            (thread_uuid,),
        )
        reactions_by_msg: dict[str, list[tuple[str, str]]] = {}
        for mid, name, uid in cur.fetchall():
            reactions_by_msg.setdefault(mid, []).append((name, uid))

    root_row = next((r for r in rows if r[8]), rows[0])
    _, team_id, channel_id, root_ts, _, _, root_text, root_ts_iso, _, channel_name = (
        root_row
    )

    # Use the root message's text (truncated) as the thread's display name.
    # Mirrors how the Slack UI labels threads when no explicit title exists.
    snippet = (root_text or "").strip().splitlines()
    title = snippet[0] if snippet else "(empty thread)"
    title = title[:80]

    out_dir = root / "slack" / team_id / channel_name / "threads"
    out_dir.mkdir(parents=True, exist_ok=True)
    slug = _slugify(title)
    target = out_dir / f"{thread_uuid}__{slug}.qmd"
    for existing in out_dir.glob(f"{thread_uuid}__*.qmd"):
        if existing != target:
            existing.unlink()

    parts: list[str] = []
    parts.append("---")
    parts.append("provider: slack")
    parts.append(f"thread_uuid: {_yaml_scalar(thread_uuid)}")
    parts.append(f"team_id: {_yaml_scalar(team_id)}")
    parts.append(f"channel_id: {_yaml_scalar(channel_id)}")
    parts.append(f"channel_name: {_yaml_scalar(channel_name)}")
    parts.append(f"root_ts: {_yaml_scalar(root_ts)}")
    parts.append(f"root_ts_iso: {_yaml_scalar(root_ts_iso)}")
    parts.append(
        f"slack_link: {_yaml_scalar(_slack_message_link(team_id, channel_id, root_ts))}"
    )
    parts.append("---")
    parts.append("")
    parts.append(f"# #{channel_name}: {title}")
    parts.append("")

    for msg_index, (
        msg_uuid,
        _,
        _,
        ts,
        _,
        user_id,
        text,
        ts_iso,
        _is_root,
        _,
    ) in enumerate(rows):
        author = user_labels.get(user_id, user_id or "unknown")
        link = _slack_message_link(team_id, channel_id, ts)
        parts.append(_msg_div_open(msg_uuid, msg_index, "slack"))
        parts.append("")
        parts.append(f"## {author}")
        parts.append("")
        # Plain HTML for the meta line so the link renders even though the
        # surrounding span is italic — markdown's `*…[label](url)…*` parsed
        # the link inconsistently inside emphasis.
        parts.append(
            f'<div class="msg-meta"><em>{ts_iso}</em> · '
            f'<a href="{link}">view in Slack ↗</a></div>'
        )
        parts.append("")
        parts.append(_slack_to_commonmark((text or "").rstrip(), user_labels))
        parts.append("")
        rxs = reactions_by_msg.get(msg_uuid)
        if rxs:
            # Group reactions by emoji name to render `:thumbsup: ×2` instead
            # of one line per reactor. The (name, user) tuples are already
            # sorted by name from the SELECT above.
            counts: dict[str, int] = {}
            for name, _uid in rxs:
                counts[name] = counts.get(name, 0) + 1
            emoji_strs = [
                f":{n}: ×{c}" if c > 1 else f":{n}:" for n, c in counts.items()
            ]
            parts.append("> Reactions: " + " ".join(emoji_strs))
            parts.append("")
        parts.append(_MSG_DIV_CLOSE)
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

        log.info("rendering anthropic: %d conversations", len(rows))
        live_uuids: set[str] = set()
        accounts: set[str] = set()
        for conv_uuid, acct in tqdm(
            rows, desc="render anthropic", unit="conv", leave=False
        ):
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
            cur.execute("SELECT conversation_id, account_id FROM openai_conversations")
            rows = list(cur.fetchall())
        log.info("rendering openai: %d conversations", len(rows))
        live_ids: set[str] = set()
        accts: set[str] = set()
        for cid, acct in tqdm(rows, desc="render openai", unit="conv", leave=False):
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

    if _table_exists(conn, "slack_messages"):
        with conn.cursor() as cur:
            cur.execute(
                "SELECT DISTINCT thread_uuid, team_id, channel_id FROM slack_messages"
            )
            slack_rows = list(cur.fetchall())
        live_threads: set[str] = set()
        slack_dirs: set[tuple[str, str]] = set()
        with conn.cursor() as cur:
            cur.execute("SELECT channel_id, name FROM slack_channels")
            channel_names = {cid: name for cid, name in cur.fetchall()}
        log.info("rendering slack: %d threads", len(slack_rows))
        for thread_uuid, team_id, channel_id in tqdm(
            slack_rows, desc="render slack", unit="thr", leave=False
        ):
            live_threads.add(thread_uuid)
            cname = channel_names.get(channel_id) or channel_id
            slack_dirs.add((team_id, cname))
            render_slack_thread(conn, thread_uuid, root)
            summary.rendered += 1
        for team_id, cname in slack_dirs:
            threads_dir = root / "slack" / team_id / cname / "threads"
            if not threads_dir.is_dir():
                continue
            for f in threads_dir.glob("*.qmd"):
                tid_prefix = f.name.split("__", 1)[0]
                if tid_prefix not in live_threads:
                    f.unlink()
                    summary.orphans_removed += 1

    _write_accounts_index(conn, root)

    return summary
