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
from ingest.providers.github.parse import ParsedGithubApi
from ingest.providers.gitlab.parse import ParsedGitlabApi
from ingest.providers.notion.parse import (
    BlockRow as _NotionBlockRow,
)
from ingest.providers.notion.parse import (
    CommentRow as _NotionCommentRow,
)
from ingest.providers.notion.parse import (
    ParsedNotionWeb,
    notion_ms_to_iso,
    notion_url,
    rich_text_to_plain,
)
from ingest.providers.openai.parse import ParsedChatGPTApi
from ingest.providers.slack.mrkdwn import emojize_shortcodes, resolve_user_mentions
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


def _slack_message_link(
    team_id: str, channel_id: str, ts: str, thread_ts: str | None = None
) -> str:
    """Slack web deep-link of the form
    `https://slack.com/archives/{channel}/p{ts_no_dot}`. The `team` param
    routes cross-workspace clicks to the right org. For thread replies,
    pass `thread_ts` (the dotted root ts) so the URL opens the side pane
    on the reply rather than landing on the channel view."""
    ts_no_dot = ts.replace(".", "")
    url = f"https://slack.com/archives/{channel_id}/p{ts_no_dot}?team={team_id}"
    if thread_ts is not None and thread_ts != ts:
        url += f"&thread_ts={thread_ts}&cid={channel_id}"
    return url


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

    snippet = (
        resolve_user_mentions(root_msg.text or "", user_labels).strip().splitlines()
    )
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
        link = _slack_message_link(team_id, channel_id, m.ts, thread_ts=root_msg.ts)
        parts.append(_msg_div_open(m.uuid, msg_index, "slack"))
        parts.append("")
        parts.append(f"## {author}")
        parts.append("")
        # Plain HTML for the meta line so the link renders even though the
        # surrounding span is italic — markdown's `*…[label](url)…*` parsed
        # the link inconsistently inside emphasis.
        parts.append(
            f'<div class="msg-meta"><em>{m.ts_iso}</em> · '
            f'<a href="{link}" target="_blank" rel="noopener noreferrer" title="View in Slack">↗</a></div>'
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
                f"{emojize_shortcodes(f':{n}:')} ×{c}"
                if c > 1
                else emojize_shortcodes(f":{n}:")
                for n, c in counts.items()
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


# ---------------- GitHub ----------------


def _github_pr_dir(repo_full_name: str, pr_number: int, title: str) -> tuple[str, str]:
    """Return (`relative dir`, `slug`) for a PR's per-PR directory.

    Layout: `github/{owner}/{repo}/pr-{number}__{slug}/`. Owner/repo is split
    on '/' so the tree mirrors GitHub's URL structure.
    """
    owner, _, repo = repo_full_name.partition("/")
    repo = repo or "_"
    slug = _slugify(title)
    return f"github/{owner}/{repo}/pr-{pr_number}__{slug}", slug


def _thread_filename_slug(thread_key: str, body: str) -> str:
    if thread_key == "general":
        return "general"
    # diff thread "path:line" → "{slug(path)}-L{line}"
    path, _, line = thread_key.rpartition(":")
    return f"{_slugify(path)}-L{line or '0'}"


def render_github(parsed: ParsedGithubApi, root: Path) -> RenderSummary:
    summary = RenderSummary()
    if not parsed.pull_requests:
        return summary

    comments_by_pr: dict[tuple[str, int], list] = {}
    for c in parsed.comments:
        comments_by_pr.setdefault((c.repo_full_name, c.pr_number), []).append(c)

    log.info("rendering github: %d PRs", len(parsed.pull_requests))
    for pr in tqdm(parsed.pull_requests, desc="render github", unit="pr", leave=False):
        rel_dir, _slug = _github_pr_dir(pr.repo_full_name, pr.pr_number, pr.title)
        pr_dir = root / rel_dir
        pr_dir.mkdir(parents=True, exist_ok=True)
        threads_dir = pr_dir / "threads"
        threads_dir.mkdir(exist_ok=True)

        pr_comments = sorted(
            comments_by_pr.get((pr.repo_full_name, pr.pr_number), []),
            key=lambda c: (c.created_at, c.external_id),
        )
        by_thread: dict[str, list] = {}
        for c in pr_comments:
            by_thread.setdefault(c.thread_key, []).append(c)

        # Per-thread QMDs
        thread_links: list[tuple[str, str, int]] = []  # (label, rel-path, count)
        for thread_key, items in by_thread.items():
            fname = _thread_filename_slug(thread_key, "")
            target = threads_dir / f"{fname}.qmd"
            parts: list[str] = []
            parts.append("---")
            parts.append("provider: github")
            parts.append(f"repo: {_yaml_scalar(pr.repo_full_name)}")
            parts.append(f"pr_number: {pr.pr_number}")
            parts.append(f"thread: {_yaml_scalar(thread_key)}")
            parts.append("---")
            parts.append("")
            if thread_key == "general":
                parts.append(f"# {pr.title}: discussion")
            else:
                path, _, line = thread_key.rpartition(":")
                parts.append(f"# {pr.title}: `{path}`:{line}")
            parts.append("")
            for i, c in enumerate(items):
                author = c.user_login or "unknown"
                parts.append(_msg_div_open(c.uuid, i, "github"))
                parts.append("")
                parts.append(f"## {author} ({c.kind})")
                meta = [c.created_at]
                if c.html_url:
                    meta.append(f"[view on GitHub]({c.html_url})")
                parts.append("")
                parts.append("*" + " · ".join(m for m in meta if m) + "*")
                parts.append("")
                parts.append((c.body or "").rstrip())
                parts.append("")
                parts.append(_MSG_DIV_CLOSE)
                parts.append("")
            body = "\n".join(parts).rstrip() + "\n"
            target.write_text(body)
            thread_links.append((thread_key, f"threads/{fname}.qmd", len(items)))
            summary.rendered += 1

        # PR index.qmd (metadata + TOC of thread links)
        idx_parts: list[str] = []
        idx_parts.append("---")
        idx_parts.append("provider: github")
        idx_parts.append(f"repo: {_yaml_scalar(pr.repo_full_name)}")
        idx_parts.append(f"pr_number: {pr.pr_number}")
        idx_parts.append(f"title: {_yaml_scalar(pr.title)}")
        idx_parts.append(f"state: {_yaml_scalar(pr.state)}")
        idx_parts.append(f"author: {_yaml_scalar(pr.user_login)}")
        idx_parts.append(f"created_at: {_yaml_scalar(pr.created_at)}")
        idx_parts.append(f"merged_at: {_yaml_scalar(pr.merged_at)}")
        idx_parts.append(f"head_sha: {_yaml_scalar(pr.head_sha)}")
        idx_parts.append(f"base_sha: {_yaml_scalar(pr.base_sha)}")
        idx_parts.append(f"html_url: {_yaml_scalar(pr.html_url)}")
        idx_parts.append("---")
        idx_parts.append("")
        idx_parts.append(f"# {pr.title}")
        idx_parts.append("")
        if pr.html_url:
            idx_parts.append(f"[View PR #{pr.pr_number} on GitHub]({pr.html_url})")
            idx_parts.append("")
        if pr.body:
            idx_parts.append(pr.body.rstrip())
            idx_parts.append("")
        if thread_links:
            idx_parts.append("## Threads")
            idx_parts.append("")
            for key, rel, n in sorted(thread_links):
                label = "General discussion" if key == "general" else f"`{key}`"
                idx_parts.append(
                    f"- [{label}]({rel}) ({n} comment{'s' if n != 1 else ''})"
                )
            idx_parts.append("")
        (pr_dir / "index.qmd").write_text("\n".join(idx_parts).rstrip() + "\n")
        summary.rendered += 1
    return summary


# ---------------- GitLab ----------------


def _gitlab_mr_dir(project_path: str, mr_iid: int, title: str) -> tuple[str, str]:
    parts = project_path.split("/")
    if len(parts) >= 2:
        group = "/".join(parts[:-1])
        project = parts[-1]
    else:
        group = "_"
        project = project_path or "_"
    slug = _slugify(title)
    return f"gitlab/{group}/{project}/mr-{mr_iid}__{slug}", slug


def render_gitlab(parsed: ParsedGitlabApi, root: Path) -> RenderSummary:
    summary = RenderSummary()
    if not parsed.merge_requests:
        return summary

    notes_by_mr: dict[tuple[str, int], list] = {}
    for n in parsed.notes:
        notes_by_mr.setdefault((n.project_path, n.mr_iid), []).append(n)

    log.info("rendering gitlab: %d MRs", len(parsed.merge_requests))
    for mr in tqdm(parsed.merge_requests, desc="render gitlab", unit="mr", leave=False):
        rel_dir, _slug = _gitlab_mr_dir(mr.project_path, mr.mr_iid, mr.title)
        mr_dir = root / rel_dir
        mr_dir.mkdir(parents=True, exist_ok=True)
        threads_dir = mr_dir / "threads"
        threads_dir.mkdir(exist_ok=True)

        mr_notes = sorted(
            notes_by_mr.get((mr.project_path, mr.mr_iid), []),
            key=lambda n: (n.created_at, n.external_id),
        )
        by_thread: dict[str, list] = {}
        for n in mr_notes:
            by_thread.setdefault(n.thread_key, []).append(n)

        thread_links: list[tuple[str, str, int]] = []
        for thread_key, items in by_thread.items():
            fname = _thread_filename_slug(thread_key, "")
            target = threads_dir / f"{fname}.qmd"
            parts: list[str] = []
            parts.append("---")
            parts.append("provider: gitlab")
            parts.append(f"project: {_yaml_scalar(mr.project_path)}")
            parts.append(f"mr_iid: {mr.mr_iid}")
            parts.append(f"thread: {_yaml_scalar(thread_key)}")
            parts.append("---")
            parts.append("")
            if thread_key == "general":
                parts.append(f"# {mr.title}: discussion")
            else:
                path, _, line = thread_key.rpartition(":")
                parts.append(f"# {mr.title}: `{path}`:{line}")
            parts.append("")
            for i, n in enumerate(items):
                author = n.user_login or "unknown"
                parts.append(_msg_div_open(n.uuid, i, "gitlab"))
                parts.append("")
                parts.append(f"## {author}")
                meta = [n.created_at]
                if n.web_url:
                    meta.append(f"[view on GitLab]({n.web_url})")
                parts.append("")
                parts.append("*" + " · ".join(m for m in meta if m) + "*")
                parts.append("")
                parts.append((n.body or "").rstrip())
                parts.append("")
                parts.append(_MSG_DIV_CLOSE)
                parts.append("")
            target.write_text("\n".join(parts).rstrip() + "\n")
            thread_links.append((thread_key, f"threads/{fname}.qmd", len(items)))
            summary.rendered += 1

        idx_parts: list[str] = []
        idx_parts.append("---")
        idx_parts.append("provider: gitlab")
        idx_parts.append(f"project: {_yaml_scalar(mr.project_path)}")
        idx_parts.append(f"mr_iid: {mr.mr_iid}")
        idx_parts.append(f"title: {_yaml_scalar(mr.title)}")
        idx_parts.append(f"state: {_yaml_scalar(mr.state)}")
        idx_parts.append(f"author: {_yaml_scalar(mr.author_username)}")
        idx_parts.append(f"created_at: {_yaml_scalar(mr.created_at)}")
        idx_parts.append(f"merged_at: {_yaml_scalar(mr.merged_at)}")
        idx_parts.append(f"head_sha: {_yaml_scalar(mr.head_sha)}")
        idx_parts.append(f"base_sha: {_yaml_scalar(mr.base_sha)}")
        idx_parts.append(f"web_url: {_yaml_scalar(mr.web_url)}")
        idx_parts.append("---")
        idx_parts.append("")
        idx_parts.append(f"# {mr.title}")
        idx_parts.append("")
        if mr.web_url:
            idx_parts.append(f"[View MR !{mr.mr_iid} on GitLab]({mr.web_url})")
            idx_parts.append("")
        if mr.description:
            idx_parts.append(mr.description.rstrip())
            idx_parts.append("")
        if thread_links:
            idx_parts.append("## Threads")
            idx_parts.append("")
            for key, rel, n in sorted(thread_links):
                label = "General discussion" if key == "general" else f"`{key}`"
                idx_parts.append(
                    f"- [{label}]({rel}) ({n} note{'s' if n != 1 else ''})"
                )
            idx_parts.append("")
        (mr_dir / "index.qmd").write_text("\n".join(idx_parts).rstrip() + "\n")
        summary.rendered += 1
    return summary


# ---------------- Notion ----------------


_NOTION_HEADING_TYPES = {"header": 1, "sub_header": 2, "sub_sub_header": 3}


def _notion_short_id(uuid_str: str) -> str:
    return (uuid_str or "").split("-", 1)[0][:8] or "00000000"


def _notion_page_slug(title: str | None) -> str:
    return _slugify(title) if title else "untitled"


def _notion_page_dir_segment(page_id: str, title: str | None) -> str:
    return f"{_notion_page_slug(title)}__{_notion_short_id(page_id)}"


def _notion_space_segment(space) -> str:
    name = space.name if space else None
    sid = space.space_id if space else ""
    return f"{_slugify(name)}__{_notion_short_id(sid)}"


def _notion_page_titles(
    blocks: list[_NotionBlockRow],
    collections: list | None = None,
) -> dict[str, str]:
    """Plain-text title for every page-like block (page + collection_view_page).
    Used both for rich-text mention resolution and for path slugs.

    For `collection_view_page` the title lives on the linked collection,
    not on the block itself; fall back to the collection's `name` when the
    block has no own title."""
    coll_names: dict[str, str] = {}
    for c in collections or []:
        coll_names[c.collection_id] = c.name_plain or "(untitled)"
    out: dict[str, str] = {}
    for b in blocks:
        if b.type not in ("page", "collection_view_page"):
            continue
        t = rich_text_to_plain((b.properties or {}).get("title"))
        if not t and b.type == "collection_view_page" and b.collection_id:
            t = coll_names.get(b.collection_id, "")
        out[b.block_id] = t or "(untitled)"
    return out


def _notion_user_names(parsed: ParsedNotionWeb) -> dict[str, str]:
    return {u.user_id: (u.name or u.user_id) for u in parsed.users}


def _notion_page_path_segments(
    page_id: str,
    blocks_by_id: dict[str, _NotionBlockRow],
    page_titles: dict[str, str],
    collection_owner_block: dict[str, str] | None = None,
) -> list[str]:
    """Walk parent chain up while parent_table=='block' (i.e. nested pages).
    Stops at the root page (whose parent_table is 'space'). The space
    segment is added by the caller.

    `collection_owner_block` maps collection_id → the block (typically a
    `collection_view_page`) that owns it, so DB rows (which have
    parent_table='collection') can chain up into the page hierarchy."""
    collection_owner_block = collection_owner_block or {}
    chain: list[str] = []
    cur_id: str | None = page_id
    seen: set[str] = set()
    while cur_id and cur_id not in seen:
        seen.add(cur_id)
        block = blocks_by_id.get(cur_id)
        if block is None:
            break
        chain.append(_notion_page_dir_segment(cur_id, page_titles.get(cur_id)))
        if block.parent_table == "block":
            parent = blocks_by_id.get(block.parent_id or "")
            if parent is None or parent.type not in ("page", "collection_view_page"):
                break
            cur_id = parent.block_id
        elif block.parent_table == "collection":
            owner_id = collection_owner_block.get(block.parent_id or "")
            if not owner_id:
                break
            cur_id = owner_id
        else:
            break
    return list(reversed(chain))


def _notion_collection_owner_block_map(
    blocks_by_id: dict[str, _NotionBlockRow],
) -> dict[str, str]:
    """For each `collection_id`, the block that 'hosts' it in the page
    hierarchy — prefer a `collection_view_page` (full-page DB), fall back
    to an inline `collection_view`. Used to graft DB rows
    (parent_table='collection') into the page tree."""
    out: dict[str, str] = {}
    for b in blocks_by_id.values():
        if not b.collection_id:
            continue
        if b.type == "collection_view_page":
            out[b.collection_id] = b.block_id
        elif b.type == "collection_view" and b.collection_id not in out:
            out[b.collection_id] = b.block_id
    return out


def _notion_page_rel_dir(
    space,
    page_id: str,
    blocks_by_id: dict[str, _NotionBlockRow],
    page_titles: dict[str, str],
) -> str:
    coll_owner = _notion_collection_owner_block_map(blocks_by_id)
    segs = [_notion_space_segment(space)] + _notion_page_path_segments(
        page_id, blocks_by_id, page_titles, collection_owner_block=coll_owner
    )
    return "notion/" + "/".join(segs)


def _notion_page_qmd_path(
    space,
    page_id: str,
    blocks_by_id: dict[str, _NotionBlockRow],
    page_titles: dict[str, str],
) -> str:
    """Each page renders to `<page_rel_dir>/index.qmd` so its sub-pages can
    live in sibling directories beneath the same path."""
    return (
        f"{_notion_page_rel_dir(space, page_id, blocks_by_id, page_titles)}/index.qmd"
    )


def _notion_thread_qmd_path(
    space,
    page_id: str,
    discussion_id: str,
    blocks_by_id: dict[str, _NotionBlockRow],
    page_titles: dict[str, str],
    snippet: str,
) -> str:
    page_dir = _notion_page_rel_dir(space, page_id, blocks_by_id, page_titles)
    name = f"{_notion_short_id(discussion_id)}__{_slugify(snippet)}.qmd"
    return f"{page_dir}/comments/{name}"


def _notion_render_rich_text(
    rt: list | None,
    user_names: dict[str, str],
    page_titles: dict[str, str],
) -> str:
    """Render Notion rich-text to inline CommonMark. Preserves bold / italic /
    code marks and rewrites user / page / link references."""
    if not rt:
        return ""
    out: list[str] = []
    for span in rt:
        if not isinstance(span, list) or not span:
            continue
        text = span[0]
        marks = span[1] if len(span) > 1 else []
        rendered: str | None = None
        wrap_bold = False
        wrap_italic = False
        wrap_code = False
        link_href: str | None = None
        for mark in marks or []:
            if not isinstance(mark, list) or not mark:
                continue
            tag = mark[0]
            arg = mark[1] if len(mark) > 1 else None
            if tag == "b":
                wrap_bold = True
            elif tag == "i":
                wrap_italic = True
            elif tag == "c":
                wrap_code = True
            elif tag == "a" and isinstance(arg, str):
                link_href = arg
            elif tag == "u" and isinstance(arg, str):
                rendered = "@" + (user_names.get(arg) or arg[:8])
            elif tag == "p" and isinstance(arg, str):
                title = page_titles.get(arg) or f"page:{arg[:8]}"
                rendered = title
            elif tag == "d" and isinstance(arg, dict):
                rendered = arg.get("start_date") or ""
            elif tag == "sm" and isinstance(arg, str):
                rendered = arg.split("/", 1)[-1]
        s = rendered if rendered is not None else str(text)
        if wrap_code:
            s = f"`{s}`"
        if wrap_bold:
            s = f"**{s}**"
        if wrap_italic:
            s = f"*{s}*"
        if link_href and not wrap_code:
            s = f"[{s}]({link_href})"
        out.append(s)
    return "".join(out)


def _notion_render_block(
    block: _NotionBlockRow,
    blocks_by_id: dict[str, _NotionBlockRow],
    user_names: dict[str, str],
    page_titles: dict[str, str],
    *,
    depth: int = 0,
) -> list[str]:
    """Recursively render one block to markdown lines. Returns lines with no
    trailing newlines so the caller can join with '\\n'."""
    btype = block.type or ""
    props = block.properties or {}
    title_md = _notion_render_rich_text(props.get("title"), user_names, page_titles)
    indent = "    " * depth
    lines: list[str] = []

    if btype == "text":
        if title_md:
            lines.append(f"{indent}{title_md}")
        lines.append("")
    elif btype == "header":
        lines.append(f"# {title_md}")
        lines.append("")
    elif btype == "sub_header":
        lines.append(f"## {title_md}")
        lines.append("")
    elif btype == "sub_sub_header":
        lines.append(f"### {title_md}")
        lines.append("")
    elif btype == "bulleted_list":
        lines.append(f"{indent}- {title_md}")
    elif btype == "numbered_list":
        lines.append(f"{indent}1. {title_md}")
    elif btype == "to_do":
        checked = (props.get("checked") or [[""]])[0][0] == "Yes"
        box = "[x]" if checked else "[ ]"
        lines.append(f"{indent}- {box} {title_md}")
    elif btype == "toggle":
        lines.append(f"{indent}<details><summary>{title_md}</summary>")
        lines.append("")
        for child_id in block.content:
            child = blocks_by_id.get(child_id)
            if child:
                lines.extend(
                    _notion_render_block(
                        child, blocks_by_id, user_names, page_titles, depth=depth
                    )
                )
        lines.append(f"{indent}</details>")
        lines.append("")
        return lines  # children already rendered
    elif btype == "quote":
        lines.append(f"> {title_md}")
        lines.append("")
    elif btype == "callout":
        icon = (block.format or {}).get("page_icon") or "💡"
        lines.append(f"> {icon} {title_md}")
        lines.append("")
    elif btype == "code":
        lang_rt = props.get("language") or [[""]]
        lang = (lang_rt[0][0] if lang_rt and lang_rt[0] else "").lower()
        code_text = (props.get("title") or [[""]])[0][0]
        lines.append(f"```{lang}")
        lines.append(code_text)
        lines.append("```")
        lines.append("")
    elif btype == "divider":
        lines.append("---")
        lines.append("")
    elif btype == "image":
        src = (props.get("source") or [[""]])[0][0]
        caption = _notion_render_rich_text(
            props.get("caption"), user_names, page_titles
        )
        alt = caption or "image"
        lines.append(f"![{alt}]({src})")
        lines.append("")
    elif btype == "file":
        src = (props.get("source") or [[""]])[0][0]
        name = (props.get("title") or [["file"]])[0][0]
        lines.append(f"[📎 {name}]({src})")
        lines.append("")
    elif btype == "embed":
        src = (props.get("source") or [[""]])[0][0]
        lines.append(f'<iframe src="{src}"></iframe>')
        lines.append("")
    elif btype == "table":
        col_order = (block.format or {}).get("table_block_column_order") or []
        has_header = bool((block.format or {}).get("table_block_column_header"))
        rows: list[list[str]] = []
        for row_id in block.content:
            row_block = blocks_by_id.get(row_id)
            if row_block is None or row_block.type != "table_row":
                continue
            cells = []
            for col in col_order:
                cell_rt = (row_block.properties or {}).get(col) or []
                cells.append(_notion_render_rich_text(cell_rt, user_names, page_titles))
            rows.append(cells)
        if rows:
            if not has_header:
                rows.insert(0, ["" for _ in col_order])
            lines.append("| " + " | ".join(rows[0]) + " |")
            lines.append("| " + " | ".join(["---"] * len(col_order)) + " |")
            for r in rows[1:]:
                lines.append("| " + " | ".join(r) + " |")
            lines.append("")
        return lines
    elif btype in ("column_list", "column"):
        for child_id in block.content:
            child = blocks_by_id.get(child_id)
            if child:
                lines.extend(
                    _notion_render_block(
                        child, blocks_by_id, user_names, page_titles, depth=depth
                    )
                )
        return lines
    elif btype == "page" and depth > 0:
        # Nested page link — render as a link rather than inlining.
        sub_title = page_titles.get(block.block_id, "(untitled)")
        slug = _notion_page_dir_segment(block.block_id, sub_title)
        lines.append(f"{indent}- 📄 [{sub_title}]({slug}/index.qmd)")
        lines.append("")
        return lines
    elif btype == "collection_view" or btype == "collection_view_page":
        coll_title = (
            page_titles.get(block.block_id) if btype == "collection_view_page" else None
        )
        label = coll_title or "(inline database)"
        lines.append(f"{indent}*[Database: {label}]*")
        lines.append("")
        return lines
    else:
        if title_md:
            lines.append(f"{indent}{title_md}")
            lines.append("")

    # Render children for the simple block types (lists / to_do can have
    # nested children via `content`).
    if btype in ("bulleted_list", "numbered_list", "to_do"):
        for child_id in block.content:
            child = blocks_by_id.get(child_id)
            if child:
                lines.extend(
                    _notion_render_block(
                        child,
                        blocks_by_id,
                        user_names,
                        page_titles,
                        depth=depth + 1,
                    )
                )
    return lines


def _notion_render_one_page(
    page: _NotionBlockRow,
    parsed: ParsedNotionWeb,
    blocks_by_id: dict[str, _NotionBlockRow],
    page_titles: dict[str, str],
    user_names: dict[str, str],
    root: Path,
) -> Path:
    rel_dir = _notion_page_rel_dir(
        parsed.space, page.block_id, blocks_by_id, page_titles
    )
    out_dir = root / rel_dir
    out_dir.mkdir(parents=True, exist_ok=True)
    target = out_dir / "index.qmd"

    title = page_titles.get(page.block_id) or "(untitled)"
    icon = (page.format or {}).get("page_icon")

    parts: list[str] = []
    parts.append("---")
    parts.append("provider: notion")
    parts.append(f"page_id: {_yaml_scalar(page.block_id)}")
    parts.append(f"title: {_yaml_scalar(title)}")
    parts.append(
        f"space_id: {_yaml_scalar(parsed.space.space_id if parsed.space else None)}"
    )
    parts.append(
        f"created_time: {_yaml_scalar(notion_ms_to_iso(page.created_time_ms))}"
    )
    parts.append(
        f"last_edited_time: {_yaml_scalar(notion_ms_to_iso(page.last_edited_time_ms))}"
    )
    parts.append("---")
    parts.append("")
    parts.append(f"# {icon + ' ' if icon else ''}{title}")
    parts.append("")
    parts.append(f"[View on Notion ↗]({notion_url(page.block_id)})")
    parts.append("")

    for child_id in page.content:
        child = blocks_by_id.get(child_id)
        if child is None or not child.alive:
            continue
        parts.extend(_notion_render_block(child, blocks_by_id, user_names, page_titles))

    # Sub-pages: any block whose parent is this page and type=='page'.
    subpages = [
        b
        for b in parsed.blocks
        if b.parent_id == page.block_id
        and b.parent_table == "block"
        and b.type in ("page", "collection_view_page")
        and b.alive
    ]
    if subpages:
        parts.append("## Sub-pages")
        parts.append("")
        for sp in subpages:
            sub_title = page_titles.get(sp.block_id, "(untitled)")
            seg = _notion_page_dir_segment(sp.block_id, sub_title)
            parts.append(f"- [📄 {sub_title}]({seg}/index.qmd)")
        parts.append("")

    target.write_text("\n".join(parts).rstrip() + "\n")
    return target


def _notion_render_one_thread(
    discussion,
    parsed: ParsedNotionWeb,
    blocks_by_id: dict[str, _NotionBlockRow],
    page_titles: dict[str, str],
    user_names: dict[str, str],
    page_id: str,
    comments: list,
    root: Path,
    ordered_ids: list[str] | None = None,
) -> Path:
    first_text = comments[0].text_plain if comments else ""
    snippet = first_text or discussion.context_plain or ""
    snippet = snippet.strip().splitlines()[0][:60] if snippet.strip() else "thread"

    rel = _notion_thread_qmd_path(
        parsed.space,
        page_id,
        discussion.discussion_id,
        blocks_by_id,
        page_titles,
        snippet,
    )
    target = root / rel
    target.parent.mkdir(parents=True, exist_ok=True)

    page_title = page_titles.get(page_id, "(untitled)")
    block_anchor = discussion.parent_id if discussion.parent_table == "block" else None
    thread_url = notion_url(
        page_id,
        discussion_id=discussion.discussion_id,
        block_anchor=block_anchor,
    )
    parts: list[str] = []
    parts.append("---")
    parts.append("provider: notion")
    parts.append(f"discussion_id: {_yaml_scalar(discussion.discussion_id)}")
    parts.append(f"page_id: {_yaml_scalar(page_id)}")
    parts.append(f"resolved: {str(bool(discussion.resolved)).lower()}")
    if discussion.context_plain:
        parts.append(f"context: {_yaml_scalar(discussion.context_plain)}")
    parts.append("---")
    parts.append("")
    parts.append(f"# Comment thread on “{page_title}”")
    parts.append("")
    parts.append(f"[View thread on Notion ↗]({thread_url})")
    parts.append("")
    if discussion.context_plain:
        parts.append(f"> Anchored to: *{discussion.context_plain}*")
        parts.append("")

    # Notion doesn't expose stable per-comment permalinks; the discussion
    # URL opens the side panel scrolled to the full thread.
    matched_by_id: dict[str, _NotionCommentRow] = {c.comment_id: c for c in comments}
    iterable: list[tuple[str, _NotionCommentRow | None]]
    if ordered_ids:
        iterable = [(cid, matched_by_id.get(cid)) for cid in ordered_ids]
    else:
        iterable = [(c.comment_id, c) for c in comments]
    for i, (cid, c) in enumerate(iterable):
        if c is None:
            # Referenced by discussion.comment_ids but no CommentRow on disk
            # (e.g. backup race, deleted comment, or downloader gap). Surface
            # a placeholder so the thread isn't silently truncated.
            parts.append(_msg_div_open(cid, i, "notion"))
            parts.append("")
            parts.append("## (missing comment)")
            parts.append("")
            parts.append(
                f"*comment `{cid}` referenced by this discussion "
                f"but not present in backup* — [↗]({thread_url})"
            )
            parts.append("")
            parts.append(_MSG_DIV_CLOSE)
            parts.append("")
            continue
        author = user_names.get(c.created_by_id or "", c.created_by_id or "unknown")
        parts.append(_msg_div_open(c.comment_id, i, "notion"))
        parts.append("")
        parts.append(f"## {author}")
        parts.append("")
        parts.append(f"*{notion_ms_to_iso(c.created_time_ms)}* — [↗]({thread_url})")
        parts.append("")
        body_md = _notion_render_rich_text(
            (c.raw_json or {}).get("text"), user_names, page_titles
        )
        parts.append(body_md or c.text_plain)
        parts.append("")
        parts.append(_MSG_DIV_CLOSE)
        parts.append("")

    target.write_text("\n".join(parts).rstrip() + "\n")
    return target


def render_notion(parsed: ParsedNotionWeb, root: Path) -> RenderSummary:
    summary = RenderSummary()
    if not parsed.blocks and not parsed.comments:
        return summary

    blocks_by_id = {b.block_id: b for b in parsed.blocks}
    page_titles = _notion_page_titles(parsed.blocks, parsed.collections)
    user_names = _notion_user_names(parsed)

    pages = [
        b
        for b in parsed.blocks
        if b.type in ("page", "collection_view_page") and b.alive
    ]
    log.info(
        "rendering notion: %d pages, %d discussions",
        len(pages),
        len(parsed.discussions),
    )

    for p in tqdm(pages, desc="render notion pages", unit="pg", leave=False):
        _notion_render_one_page(p, parsed, blocks_by_id, page_titles, user_names, root)
        summary.rendered += 1

    # Threads: group comments by discussion_id; place under the page that
    # contains the discussion's anchor block.
    comments_by_disc: dict[str, list] = {}
    for c in parsed.comments:
        if c.discussion_id:
            comments_by_disc.setdefault(c.discussion_id, []).append(c)
    for disc in parsed.discussions:
        items = sorted(
            comments_by_disc.get(disc.discussion_id, []),
            key=lambda c: (c.created_time_ms or 0, c.comment_id),
        )
        ordered_ids = list(disc.comment_ids) if disc.comment_ids else None
        if not items and not ordered_ids:
            continue
        page_id = _notion_thread_page_id(disc, blocks_by_id)
        if not page_id:
            continue
        _notion_render_one_thread(
            disc,
            parsed,
            blocks_by_id,
            page_titles,
            user_names,
            page_id,
            items,
            root,
            ordered_ids=ordered_ids,
        )
        summary.rendered += 1

    return summary


def _notion_thread_page_id(
    discussion, blocks_by_id: dict[str, _NotionBlockRow]
) -> str | None:
    """Walk up from the discussion's parent block until we hit a page-type
    block. Returns the containing page's block_id, or None if we can't
    find one (orphaned thread)."""
    if discussion.parent_table != "block":
        return None
    cur_id: str | None = discussion.parent_id
    seen: set[str] = set()
    while cur_id and cur_id not in seen:
        seen.add(cur_id)
        block = blocks_by_id.get(cur_id)
        if block is None:
            return None
        if block.type in ("page", "collection_view_page"):
            return block.block_id
        cur_id = block.parent_id
    return None


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
