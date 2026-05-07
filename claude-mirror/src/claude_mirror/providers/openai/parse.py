from __future__ import annotations

import json
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


@dataclass
class OAAccountRow:
    account_id: str
    email: str | None
    name: str | None
    raw_json: dict[str, Any]


@dataclass
class OAConversationRow:
    account_id: str | None
    conversation_id: str
    title: str | None
    create_time: str | None
    update_time: str | None
    current_node: str | None
    default_model_slug: str | None
    gizmo_id: str | None
    gizmo_type: str | None
    is_archived: bool | None
    is_starred: bool | None
    raw_json: dict[str, Any]


@dataclass
class OAMessageRow:
    conversation_id: str
    message_id: str
    parent_id: str | None
    role: str | None
    recipient: str | None
    channel: str | None
    content_type: str | None
    text: str | None
    status: str | None
    end_turn: bool | None
    weight: float | None
    model_slug: str | None
    create_time: str | None
    update_time: str | None
    raw_json: dict[str, Any]


@dataclass
class OAContentPartRow:
    message_id: str
    part_index: int
    kind: str | None
    language: str | None
    text: str | None
    raw_json: dict[str, Any]


@dataclass
class ParsedChatGPTApi:
    accounts: list[OAAccountRow] = field(default_factory=list)
    conversations: list[OAConversationRow] = field(default_factory=list)
    messages: list[OAMessageRow] = field(default_factory=list)
    content_parts: list[OAContentPartRow] = field(default_factory=list)


def _epoch_to_iso(v: Any) -> str | None:
    """Normalize ChatGPT timestamps to ISO-8601 strings.

    The listing endpoint returns ISO strings ('2026-03-19T18:42:53.847024Z');
    the conversation detail endpoint returns float epochs. We coerce both to
    ISO so they round-trip into the schema's VARCHAR(40) columns and match
    the convention used by the anthropic_* tables."""
    if v is None or v == "":
        return None
    if isinstance(v, (int, float)):
        try:
            return datetime.fromtimestamp(float(v), tz=timezone.utc) \
                .strftime("%Y-%m-%dT%H:%M:%S.%fZ")
        except (OverflowError, OSError, ValueError):
            return None
    if isinstance(v, str):
        return v
    return None


def _synthesize_text(content: dict | None) -> str:
    """Flatten a ChatGPT message.content into a single text blob, mirroring
    what the export-style export of Anthropic produces. Best-effort: text
    parts are joined with newlines; code blocks are surfaced as text with a
    fenced wrapper; thoughts are joined; everything else falls back to JSON."""
    if not isinstance(content, dict):
        return ""
    ct = content.get("content_type")
    if ct == "text":
        parts = content.get("parts") or []
        out: list[str] = []
        for p in parts:
            if isinstance(p, str):
                out.append(p)
            elif isinstance(p, dict):
                # multimodal parts have shapes like {content_type, text, ...}
                if "text" in p and isinstance(p["text"], str):
                    out.append(p["text"])
        return "\n".join(out)
    if ct == "code":
        return content.get("text") or ""
    if ct == "execution_output":
        return content.get("text") or ""
    if ct == "thoughts":
        thoughts = content.get("thoughts") or []
        out2: list[str] = []
        for t in thoughts:
            if isinstance(t, dict):
                # thought entries typically have {summary, content}
                summary = t.get("summary")
                body = t.get("content")
                if summary:
                    out2.append(str(summary))
                if body:
                    out2.append(str(body))
        return "\n\n".join(out2)
    if ct == "reasoning_recap":
        c = content.get("content")
        return c if isinstance(c, str) else ""
    if ct == "model_editable_context":
        return content.get("model_set_context") or ""
    return ""


def _content_parts(message_id: str, content: dict | None) -> list[OAContentPartRow]:
    """Break a content payload into atoms suitable for one row each."""
    rows: list[OAContentPartRow] = []
    if not isinstance(content, dict):
        return rows
    ct = content.get("content_type")
    if ct == "text":
        for i, p in enumerate(content.get("parts") or []):
            if isinstance(p, str):
                rows.append(OAContentPartRow(
                    message_id=message_id, part_index=i,
                    kind="text", language=None, text=p,
                    raw_json={"content_type": "text", "value": p},
                ))
            else:
                txt = (p.get("text") if isinstance(p, dict) else None) or ""
                rows.append(OAContentPartRow(
                    message_id=message_id, part_index=i,
                    kind="text", language=None, text=txt,
                    raw_json=p if isinstance(p, dict) else {"value": p},
                ))
    elif ct == "code":
        rows.append(OAContentPartRow(
            message_id=message_id, part_index=0,
            kind="code", language=content.get("language"),
            text=content.get("text") or "",
            raw_json=content,
        ))
    elif ct == "execution_output":
        rows.append(OAContentPartRow(
            message_id=message_id, part_index=0,
            kind="execution_output", language=None,
            text=content.get("text") or "",
            raw_json=content,
        ))
    elif ct == "thoughts":
        for i, t in enumerate(content.get("thoughts") or []):
            if not isinstance(t, dict):
                continue
            txt = "\n\n".join(
                str(t.get(k) or "") for k in ("summary", "content") if t.get(k)
            )
            rows.append(OAContentPartRow(
                message_id=message_id, part_index=i,
                kind="thoughts", language=None, text=txt,
                raw_json=t,
            ))
    elif ct == "reasoning_recap":
        rows.append(OAContentPartRow(
            message_id=message_id, part_index=0,
            kind="reasoning_recap", language=None,
            text=content.get("content") if isinstance(content.get("content"), str) else "",
            raw_json=content,
        ))
    elif ct == "model_editable_context":
        rows.append(OAContentPartRow(
            message_id=message_id, part_index=0,
            kind="model_editable_context", language=None,
            text=content.get("model_set_context") or "",
            raw_json=content,
        ))
    else:
        # Unknown shape — store one opaque row so we don't lose data.
        rows.append(OAContentPartRow(
            message_id=message_id, part_index=0,
            kind=ct or "unknown", language=None,
            text=None, raw_json=content,
        ))
    return rows


def parse_api_dir(api_dir: Path) -> ParsedChatGPTApi:
    """Parse a directory produced by scripts/sync_chatgpt_web.py.

    Layout:
        api_dir/me.json                    (optional but expected)
        api_dir/conversations.json         (listing index)
        api_dir/conversations/<id>.json    (per-conversation tree)
    """
    api_dir = Path(api_dir)
    out = ParsedChatGPTApi()

    me_path = api_dir / "me.json"
    account_id: str | None = None
    if me_path.exists():
        me = json.loads(me_path.read_text())
        account_id = me.get("id")
        if account_id:
            out.accounts.append(OAAccountRow(
                account_id=account_id,
                email=me.get("email"),
                name=me.get("name"),
                raw_json=me,
            ))

    listing_path = api_dir / "conversations.json"
    listing_by_id: dict[str, dict] = {}
    if listing_path.exists():
        listing = json.loads(listing_path.read_text())
        if isinstance(listing, list):
            listing_by_id = {c.get("id"): c for c in listing if c.get("id")}

    convs_dir = api_dir / "conversations"
    if not convs_dir.is_dir():
        return out

    for f in sorted(convs_dir.glob("*.json")):
        try:
            d = json.loads(f.read_text())
        except json.JSONDecodeError:
            continue
        cid = d.get("conversation_id") or d.get("id") or f.stem
        listing_row = listing_by_id.get(cid, {})

        out.conversations.append(OAConversationRow(
            account_id=account_id,
            conversation_id=cid,
            title=d.get("title") or listing_row.get("title"),
            create_time=_epoch_to_iso(d.get("create_time"))
                or _epoch_to_iso(listing_row.get("create_time")),
            update_time=_epoch_to_iso(d.get("update_time"))
                or _epoch_to_iso(listing_row.get("update_time")),
            current_node=d.get("current_node"),
            default_model_slug=d.get("default_model_slug"),
            gizmo_id=d.get("gizmo_id"),
            gizmo_type=d.get("gizmo_type"),
            is_archived=d.get("is_archived"),
            is_starred=d.get("is_starred"),
            # raw_json carries everything *except* the mapping (which we
            # explode into rows). Keeps the row payload reasonable.
            raw_json={k: v for k, v in d.items() if k != "mapping"},
        ))

        mapping = d.get("mapping") or {}
        for node_id, node in mapping.items():
            m = node.get("message")
            if not m:
                continue
            mid = m.get("id") or node_id
            content = m.get("content")
            author = m.get("author") or {}
            meta = m.get("metadata") or {}
            out.messages.append(OAMessageRow(
                conversation_id=cid,
                message_id=mid,
                parent_id=node.get("parent"),
                role=author.get("role"),
                recipient=m.get("recipient"),
                channel=m.get("channel"),
                content_type=(content or {}).get("content_type")
                    if isinstance(content, dict) else None,
                text=_synthesize_text(content),
                status=m.get("status"),
                end_turn=m.get("end_turn"),
                weight=m.get("weight"),
                model_slug=meta.get("model_slug"),
                create_time=_epoch_to_iso(m.get("create_time")),
                update_time=_epoch_to_iso(m.get("update_time")),
                raw_json={k: v for k, v in m.items() if k != "content"},
            ))
            out.content_parts.extend(_content_parts(mid, content))
    return out
