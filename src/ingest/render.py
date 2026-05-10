"""Render parsed provider data to QMD markdown files on disk.

Inputs are the in-memory `Parsed*` dataclasses produced by each provider's
`parse` module. We do not query SQL here — provider-specific tables no
longer exist; the parsed structs ARE the source of truth for rendering.
The only Dolt/sqlite table that survives is `grid_rows`, populated
elsewhere from the same parsed data.
"""

from __future__ import annotations

import json
import logging
import re
from dataclasses import dataclass
from datetime import datetime, timedelta
from pathlib import Path
from typing import Any

from tqdm import tqdm

from ingest.providers.anthropic.parse import ParsedExport
from ingest.providers.openai.parse import ParsedChatGPTApi
from ingest.providers.slack.mrkdwn import to_commonmark as _slack_to_commonmark
from ingest.providers.slack.parse import ParsedSlackApi

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
            out.append(
                json.dumps(tool_input, indent=2, ensure_ascii=False, sort_keys=True)
            )
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
                        json.dumps(item, indent=2, ensure_ascii=False, sort_keys=True),
                        "```",
                        "",
                    ]
                else:
                    out += ["```", str(item).rstrip(), "```", ""]
        elif content is not None:
            out += [
                "```json",
                json.dumps(content, indent=2, ensure_ascii=False, sort_keys=True),
                "```",
            ]
        out.extend(["</details>", ""])
        return out

    if btext:
        return [head, f"```{btype or ''}".rstrip(), btext.rstrip(), "```", ""]

    return [f"{head}<!-- {btype or 'block'} (no text) -->", ""]


@dataclass
class RenderSummary:
    rendered: int = 0
    orphans_removed: int = 0


# ---------------- Anthropic ----------------


def _render_one_anthropic(
    parsed: ParsedExport,
    conv_uuid: str,
    blocks_by_msg: dict[str, list],
    atts_by_msg: dict[str, list],
    msgs_by_conv: dict[str, list],
    root: Path,
) -> Path:
    conv = next(c for c in parsed.conversations if c.conversation_uuid == conv_uuid)
    out_dir = root / "anthropic" / conv.account_uuid / "llm_chats"
    out_dir.mkdir(parents=True, exist_ok=True)
    slug = _slugify(conv.name)
    target = out_dir / f"{conv_uuid}__{slug}.qmd"
    for existing in out_dir.glob(f"{conv_uuid}__*.qmd"):
        if existing != target:
            existing.unlink()

    parts: list[str] = []
    parts.append("---")
    parts.append("provider: anthropic")
    parts.append(f"uuid: {_yaml_scalar(conv_uuid)}")
    parts.append(f"name: {_yaml_scalar(conv.name)}")
    parts.append(f"account_uuid: {_yaml_scalar(conv.account_uuid)}")
    parts.append(f"project_uuid: {_yaml_scalar(conv.project_uuid)}")
    parts.append(f"created_at: {_yaml_scalar(conv.created_at)}")
    parts.append(f"updated_at: {_yaml_scalar(conv.updated_at)}")
    if conv.summary:
        parts.append(f"summary: {_yaml_scalar(conv.summary)}")
    parts.append("---")
    parts.append("")
    parts.append(f"# {conv.name or '(untitled)'}")
    parts.append("")

    msgs = sorted(
        msgs_by_conv.get(conv_uuid, []),
        key=lambda m: (m.created_at or "", m.message_uuid),
    )
    last_ts = conv.created_at
    for msg_index, m in enumerate(msgs):
        msg_created = m.created_at
        if not msg_created and last_ts:
            msg_created = _bump_iso(last_ts)
        if msg_created:
            last_ts = msg_created
        heading = (m.sender or "unknown").capitalize()
        parts.append(_msg_div_open(m.message_uuid, msg_index, "anthropic"))
        parts.append("")
        parts.append(f"## {heading}")
        if msg_created:
            parts.append("")
            parts.append(f"*{msg_created}*")
        parts.append("")
        for b in sorted(
            blocks_by_msg.get(m.message_uuid, []), key=lambda x: x.block_index
        ):
            parts.extend(
                _render_anthropic_block(
                    m.message_uuid, b.block_index, b.type, b.text, b.raw_json
                )
            )
        atts = sorted(
            atts_by_msg.get(m.message_uuid, []), key=lambda x: x.attachment_index
        )
        if atts:
            parts.append("**Attachments:**")
            parts.append("")
            for at in atts:
                raw_obj = at.raw_json
                label = (
                    raw_obj.get("file_name")
                    or raw_obj.get("name")
                    or raw_obj.get("file_kind")
                    or "(unnamed)"
                )
                parts.append(f"- [{at.kind}] {label}")
            parts.append("")
        parts.append(_MSG_DIV_CLOSE)
        parts.append("")

    body = "\n".join(parts).rstrip() + "\n"
    target.write_text(body)
    return target


def render_anthropic(parsed: ParsedExport, root: Path) -> RenderSummary:
    summary = RenderSummary()
    blocks_by_msg: dict[str, list] = {}
    for b in parsed.content_blocks:
        blocks_by_msg.setdefault(b.message_uuid, []).append(b)
    atts_by_msg: dict[str, list] = {}
    for at in parsed.attachments:
        atts_by_msg.setdefault(at.message_uuid, []).append(at)
    msgs_by_conv: dict[str, list] = {}
    for m in parsed.messages:
        msgs_by_conv.setdefault(m.conversation_uuid, []).append(m)

    log.info("rendering anthropic: %d conversations", len(parsed.conversations))
    live_uuids: set[str] = set()
    accounts: set[str] = set()
    for conv in tqdm(
        parsed.conversations, desc="render anthropic", unit="conv", leave=False
    ):
        live_uuids.add(conv.conversation_uuid)
        accounts.add(conv.account_uuid)
        _render_one_anthropic(
            parsed,
            conv.conversation_uuid,
            blocks_by_msg,
            atts_by_msg,
            msgs_by_conv,
            root,
        )
        summary.rendered += 1

    for acct in accounts:
        chats_dir = root / "anthropic" / acct / "llm_chats"
        if not chats_dir.is_dir():
            continue
        for f in chats_dir.glob("*.qmd"):
            if f.name.split("__", 1)[0] not in live_uuids:
                f.unlink()
                summary.orphans_removed += 1
    return summary


# ---------------- OpenAI ----------------


def _render_one_openai(
    conv: Any,
    msgs_by_conv: dict[str, list],
    parts_by_msg: dict[str, list],
    root: Path,
) -> Path:
    out_dir = root / "openai" / (conv.account_id or "unknown") / "llm_chats"
    out_dir.mkdir(parents=True, exist_ok=True)
    slug = _slugify(conv.title)
    target = out_dir / f"{conv.conversation_id}__{slug}.qmd"
    for existing in out_dir.glob(f"{conv.conversation_id}__*.qmd"):
        if existing != target:
            existing.unlink()

    msgs = msgs_by_conv.get(conv.conversation_id, [])
    msg_by_id = {m.message_id: m for m in msgs}

    # Walk current_node → root via parent_id to get the displayed path,
    # mirroring chatgpt.com's leaf-to-root render. Fallback: create_time
    # order if current_node is missing or unrooted.
    path: list = []
    seen: set[str] = set()
    cursor = conv.current_node
    while cursor and cursor in msg_by_id and cursor not in seen:
        seen.add(cursor)
        path.append(msg_by_id[cursor])
        cursor = msg_by_id[cursor].parent_id
    path.reverse()
    if not path:
        path = sorted(msgs, key=lambda m: m.create_time or "")

    parts: list[str] = []
    parts.append("---")
    parts.append("provider: openai")
    parts.append(f"id: {_yaml_scalar(conv.conversation_id)}")
    parts.append(f"title: {_yaml_scalar(conv.title)}")
    parts.append(f"account_id: {_yaml_scalar(conv.account_id)}")
    parts.append(f"create_time: {_yaml_scalar(conv.create_time)}")
    parts.append(f"update_time: {_yaml_scalar(conv.update_time)}")
    if conv.default_model_slug:
        parts.append(f"default_model_slug: {_yaml_scalar(conv.default_model_slug)}")
    parts.append("---")
    parts.append("")
    parts.append(f"# {conv.title or '(untitled)'}")
    parts.append("")

    last_ts = conv.create_time
    msg_index = 0
    for m in path:
        # Skip system / model_editable_context fluff in the rendered markdown.
        if m.role == "system" or m.content_type == "model_editable_context":
            continue
        msg_created = m.create_time
        if not msg_created and last_ts:
            msg_created = _bump_iso(last_ts)
        if msg_created:
            last_ts = msg_created
        heading = (m.role or "unknown").capitalize()
        parts.append(_msg_div_open(m.message_id, msg_index, "openai"))
        parts.append("")
        parts.append(f"## {heading}")
        msg_index += 1
        meta_bits = []
        if msg_created:
            meta_bits.append(msg_created)
        if m.model_slug:
            meta_bits.append(m.model_slug)
        if meta_bits:
            parts.append("")
            parts.append("*" + " · ".join(meta_bits) + "*")
        parts.append("")
        for p in sorted(parts_by_msg.get(m.message_id, []), key=lambda x: x.part_index):
            if not p.text and p.kind not in ("execution_output", "code"):
                continue
            anchor = f'<a id="b-{m.message_id}-{p.part_index}"></a>'
            if p.kind == "text":
                parts.append(anchor + (p.text or "").rstrip())
                parts.append("")
            elif p.kind == "code":
                parts.append(anchor)
                parts.append(f"```{p.language or ''}".rstrip())
                parts.append((p.text or "").rstrip())
                parts.append("```")
                parts.append("")
            elif p.kind == "execution_output":
                parts.append(anchor)
                parts.append("```")
                parts.append((p.text or "").rstrip())
                parts.append("```")
                parts.append("")
            elif p.kind in ("thoughts", "reasoning_recap"):
                parts.append(f"{anchor}<!-- {p.kind} -->")
                parts.append("> " + (p.text or "").replace("\n", "\n> "))
                parts.append("")
            else:
                parts.append(f"{anchor}<!-- {p.kind} -->")
                parts.append((p.text or "").rstrip())
                parts.append("")
        parts.append(_MSG_DIV_CLOSE)
        parts.append("")

    body = "\n".join(parts).rstrip() + "\n"
    target.write_text(body)
    return target


def render_openai(parsed: ParsedChatGPTApi, root: Path) -> RenderSummary:
    summary = RenderSummary()
    msgs_by_conv: dict[str, list] = {}
    for m in parsed.messages:
        msgs_by_conv.setdefault(m.conversation_id, []).append(m)
    parts_by_msg: dict[str, list] = {}
    for p in parsed.content_parts:
        parts_by_msg.setdefault(p.message_id, []).append(p)

    log.info("rendering openai: %d conversations", len(parsed.conversations))
    live_ids: set[str] = set()
    accts: set[str] = set()
    for conv in tqdm(
        parsed.conversations, desc="render openai", unit="conv", leave=False
    ):
        live_ids.add(conv.conversation_id)
        accts.add(conv.account_id or "unknown")
        _render_one_openai(conv, msgs_by_conv, parts_by_msg, root)
        summary.rendered += 1

    for acct in accts:
        chats_dir = root / "openai" / acct / "llm_chats"
        if not chats_dir.is_dir():
            continue
        for f in chats_dir.glob("*.qmd"):
            if f.name.split("__", 1)[0] not in live_ids:
                f.unlink()
                summary.orphans_removed += 1
    return summary


# ---------------- Slack ----------------


def _slack_message_link(team_id: str, channel_id: str, ts: str) -> str:
    """Slack web deep-link of the form
    `https://slack.com/archives/{channel}/p{ts_no_dot}`. The `team` param
    is appended so cross-workspace clicks land in the right org."""
    ts_no_dot = ts.replace(".", "")
    return f"https://slack.com/archives/{channel_id}/p{ts_no_dot}?team={team_id}"


def _publish_slack_image(
    file_obj: dict,
    media_dirs: list[Path],
    root: Path,
) -> str | None:
    """If `file_obj` is an image we can serve, ensure a symlink under
    `<root>/media/slack/<file_id>/<filename>` points at the source file
    and return the URL path the UI should use. Returns None for
    non-images, externals, or files we can't locate on disk."""
    mimetype = (file_obj.get("mimetype") or "").lower()
    if not mimetype.startswith("image/"):
        return None
    file_id = file_obj.get("id")
    if (
        not file_id
        or file_obj.get("mode") == "tombstone"
        or file_obj.get("is_external")
    ):
        return None
    name = file_obj.get("name") or file_id
    safe_name = (
        "".join(c if c.isalnum() or c in "-._ " else "_" for c in name).strip()
        or file_id
    )
    src: Path | None = None
    for md in media_dirs:
        candidate = md / file_id / safe_name
        if candidate.exists():
            src = candidate
            break
        # Fallback: any file in the file_id dir (filename mismatch).
        d = md / file_id
        if d.is_dir():
            files = [p for p in d.iterdir() if p.is_file()]
            if files:
                src = files[0]
                safe_name = files[0].name
                break
    if src is None:
        return None
    dst_dir = root / "media" / "slack" / file_id
    dst = dst_dir / safe_name
    if not dst.exists():
        dst_dir.mkdir(parents=True, exist_ok=True)
        try:
            dst.symlink_to(src.resolve())
        except OSError:
            # Fall back to copy on filesystems that disallow symlinks.
            import shutil

            shutil.copy2(src, dst)
    from urllib.parse import quote

    return f"/api/media/slack/{quote(file_id)}/{quote(safe_name)}"


def _render_one_slack_thread(
    thread_uuid: str,
    msgs: list,
    channel_name: str,
    user_labels: dict[str, str],
    reactions_by_msg: dict[str, list[tuple[str, str]]],
    root: Path,
    media_dirs: list[Path],
) -> Path:
    msgs = sorted(msgs, key=lambda m: (m.ts_iso, m.ts))
    root_msg = next((m for m in msgs if m.is_thread_root), msgs[0])
    team_id = root_msg.team_id
    channel_id = root_msg.channel_id

    snippet = (root_msg.text or "").strip().splitlines()
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
    parts.append(f"root_ts: {_yaml_scalar(root_msg.ts)}")
    parts.append(f"root_ts_iso: {_yaml_scalar(root_msg.ts_iso)}")
    parts.append(
        f"slack_link: {_yaml_scalar(_slack_message_link(team_id, channel_id, root_msg.ts))}"
    )
    parts.append("---")
    parts.append("")
    parts.append(f"# #{channel_name}: {title}")
    parts.append("")

    for msg_index, m in enumerate(msgs):
        author = user_labels.get(m.user_id or "", m.user_id or "unknown")
        link = _slack_message_link(team_id, channel_id, m.ts)
        parts.append(_msg_div_open(m.uuid, msg_index, "slack"))
        parts.append("")
        parts.append(f"## {author}")
        parts.append("")
        # Plain HTML for the meta line so the link renders even though the
        # surrounding span is italic — markdown's `*…[label](url)…*` parsed
        # the link inconsistently inside emphasis.
        parts.append(
            f'<div class="msg-meta"><em>{m.ts_iso}</em> · '
            f'<a href="{link}" target="_blank" rel="noopener noreferrer">view in Slack ↗</a></div>'
        )
        parts.append("")
        parts.append(_slack_to_commonmark((m.text or "").rstrip(), user_labels))
        parts.append("")
        files = (m.raw_json or {}).get("files") or []
        for f in files:
            url = _publish_slack_image(f, media_dirs, root)
            if url:
                alt = (f.get("title") or f.get("name") or "image").replace("]", "")
                parts.append(f"![{alt}]({url})")
                parts.append("")
        rxs = reactions_by_msg.get(m.uuid)
        if rxs:
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


def render_slack(
    parsed: ParsedSlackApi,
    root: Path,
    media_dirs: list[Path] | None = None,
) -> RenderSummary:
    """`media_dirs` lists `<slack_source>/media` directories whose image
    attachments should be symlinked into `<root>/media/slack/`. Empty/None
    disables image embedding."""
    media_dirs = media_dirs or []
    summary = RenderSummary()
    if not parsed.messages:
        return summary

    channels = {c.channel_id: c for c in parsed.channels}
    user_labels: dict[str, str] = {
        u.user_id: (u.real_name or u.name or u.user_id) for u in parsed.users
    }
    reactions_by_msg: dict[str, list[tuple[str, str]]] = {}
    for r in sorted(
        parsed.reactions, key=lambda x: (x.message_uuid, x.name, x.user_id)
    ):
        reactions_by_msg.setdefault(r.message_uuid, []).append((r.name, r.user_id))

    msgs_by_thread: dict[str, list] = {}
    for m in parsed.messages:
        msgs_by_thread.setdefault(m.thread_uuid, []).append(m)

    log.info("rendering slack: %d threads", len(msgs_by_thread))
    live_threads: set[str] = set()
    slack_dirs: set[tuple[str, str]] = set()
    for thread_uuid, msgs in tqdm(
        msgs_by_thread.items(), desc="render slack", unit="thr", leave=False
    ):
        live_threads.add(thread_uuid)
        ch = channels.get(msgs[0].channel_id)
        cname = (ch.name if ch and ch.name else None) or msgs[0].channel_id
        slack_dirs.add((msgs[0].team_id, cname))
        _render_one_slack_thread(
            thread_uuid,
            msgs,
            cname,
            user_labels,
            reactions_by_msg,
            root,
            media_dirs,
        )
        summary.rendered += 1

    for team_id, cname in slack_dirs:
        threads_dir = root / "slack" / team_id / cname / "threads"
        if not threads_dir.is_dir():
            continue
        for f in threads_dir.glob("*.qmd"):
            if f.name.split("__", 1)[0] not in live_threads:
                f.unlink()
                summary.orphans_removed += 1
    return summary


# ---------------- accounts.json ----------------


def write_accounts_json(
    anthropic: ParsedExport | None,
    openai: ParsedChatGPTApi | None,
    root: Path,
) -> None:
    """Emit `<root>/accounts.json` mapping account UUIDs → human labels.

    Read very late by the UI to render display names instead of opaque
    UUIDs in Author/Account columns. Keeping the lookup file alongside
    the QMDs lets the backend stay file-only without having to talk
    to Dolt."""
    accounts: dict[str, dict] = {}
    if anthropic is not None:
        for a in anthropic.accounts:
            accounts[a.account_uuid] = {
                "provider": "anthropic",
                "label": a.full_name or a.email or a.account_uuid,
                "email": a.email,
            }
    if openai is not None:
        for a in openai.accounts:
            accounts[a.account_id] = {
                "provider": "openai",
                "label": a.name or a.email or a.account_id,
                "email": a.email,
            }
    root.mkdir(parents=True, exist_ok=True)
    (root / "accounts.json").write_text(
        json.dumps(accounts, indent=2, sort_keys=True) + "\n"
    )
