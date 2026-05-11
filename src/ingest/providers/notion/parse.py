"""Parse the per-entity event-stream layout written by
`src/download/notion_web.py`:

    <api_dir>/<entity>/{created,updated}/events.jsonl

where each `<entity>` matches one of `notion_web.KNOWN_TABLES` (prefixed
with `notion_` if not already). We only read the `created` streams —
they're the cumulative superset; `updated` is an audit trail.

The downloader writes each row's `value` payload from the recordMap
verbatim under `raw.value`, plus a `raw.role` peer. Top-level fields are
just convenience keys for diffing (`id`, `space_id`, `version`,
`last_edited_time`). The parser here works off `raw.value` so it sees
the data exactly as Notion served it.
"""

from __future__ import annotations

import uuid as uuid_lib
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from jsonl_io import load_jsonl

# Stable namespace for Notion-derived ids that aren't already Notion UUIDs.
# Most Notion entities (page, block, discussion, comment) already have
# globally-unique UUIDs we use as-is. Headings are the exception: we mint
# our own UUID per (page_id, block_id) so a heading row's `uuid` does not
# collide with the underlying block row when both ever surface.
NOTION_UUID_NS = uuid_lib.UUID("c8e1f7a4-2b9d-4e6f-9c1d-7e2a3b4c5d6e")


def notion_heading_uuid(page_id: str, block_id: str) -> str:
    return str(uuid_lib.uuid5(NOTION_UUID_NS, f"notion:heading:{page_id}:{block_id}"))


def notion_ms_to_iso(ms: int | None) -> str:
    """Notion stores timestamps as milliseconds since unix epoch.
    Render as ISO-8601 UTC with explicit `+00:00`, microsecond precision —
    matches the project convention used by Slack/GitHub/GitLab."""
    if ms is None:
        return ""
    return datetime.fromtimestamp(ms / 1000.0, tz=timezone.utc).isoformat(
        timespec="microseconds"
    )


# ---------------------------------------------------------------------------
# Rich-text → plain text
# ---------------------------------------------------------------------------
#
# Notion's rich-text is a list of [text, marks?] tuples. `text` is usually
# a literal string; for inline references it's a sentinel ("‣" / "@" / etc.)
# and the first mark carries the reference type + id. We resolve those
# references against the user/page maps when we have them, otherwise we
# fall back to the sentinel so the output is still readable.


def rich_text_to_plain(
    rt: list | None,
    *,
    user_names: dict[str, str] | None = None,
    page_titles: dict[str, str] | None = None,
) -> str:
    if not rt:
        return ""
    user_names = user_names or {}
    page_titles = page_titles or {}
    out: list[str] = []
    for span in rt:
        if not isinstance(span, list) or not span:
            continue
        text = span[0]
        marks = span[1] if len(span) > 1 else []
        resolved: str | None = None
        for mark in marks or []:
            if not isinstance(mark, list) or not mark:
                continue
            tag = mark[0]
            arg = mark[1] if len(mark) > 1 else None
            if tag == "u" and isinstance(arg, str):
                resolved = "@" + (user_names.get(arg) or arg[:8])
                break
            if tag == "p" and isinstance(arg, str):
                resolved = page_titles.get(arg) or f"page:{arg[:8]}"
                break
            if tag == "d" and isinstance(arg, dict):
                resolved = arg.get("start_date") or ""
                break
        out.append(resolved if resolved is not None else str(text))
    return "".join(out)


def properties_title_to_plain(
    properties: dict | None,
    *,
    user_names: dict[str, str] | None = None,
    page_titles: dict[str, str] | None = None,
) -> str:
    if not properties:
        return ""
    return rich_text_to_plain(
        properties.get("title"),
        user_names=user_names,
        page_titles=page_titles,
    )


# ---------------------------------------------------------------------------
# Dataclasses
# ---------------------------------------------------------------------------


@dataclass
class SpaceRow:
    space_id: str
    name: str | None
    domain: str | None
    raw_json: dict[str, Any]


@dataclass
class UserRow:
    user_id: str
    name: str | None
    email: str | None
    raw_json: dict[str, Any]


@dataclass
class BlockRow:
    block_id: str
    space_id: str | None
    type: str | None
    properties: dict[str, Any]
    format: dict[str, Any]
    content: list[str]
    parent_id: str | None
    parent_table: str | None
    created_time_ms: int | None
    last_edited_time_ms: int | None
    created_by_id: str | None
    last_edited_by_id: str | None
    alive: bool
    collection_id: str | None
    view_ids: list[str]
    raw_json: dict[str, Any]


@dataclass
class CollectionRow:
    collection_id: str
    name_plain: str
    parent_id: str | None
    parent_table: str | None
    schema: dict[str, Any]
    raw_json: dict[str, Any]


@dataclass
class CollectionViewRow:
    view_id: str
    name: str | None
    type: str | None
    raw_json: dict[str, Any]


@dataclass
class DiscussionRow:
    discussion_id: str
    parent_id: str | None
    parent_table: str | None
    space_id: str | None
    resolved: bool
    comment_ids: list[str]
    context_plain: str
    raw_json: dict[str, Any]


@dataclass
class CommentRow:
    comment_id: str
    discussion_id: str | None
    space_id: str | None
    created_by_id: str | None
    created_time_ms: int | None
    last_edited_time_ms: int | None
    text_plain: str
    raw_json: dict[str, Any]


@dataclass
class ParsedNotionWeb:
    space: SpaceRow | None = None
    users: list[UserRow] = field(default_factory=list)
    blocks: list[BlockRow] = field(default_factory=list)
    collections: list[CollectionRow] = field(default_factory=list)
    views: list[CollectionViewRow] = field(default_factory=list)
    discussions: list[DiscussionRow] = field(default_factory=list)
    comments: list[CommentRow] = field(default_factory=list)


# ---------------------------------------------------------------------------
# Parse
# ---------------------------------------------------------------------------


def _value(event: dict) -> dict:
    raw = event.get("raw") or {}
    val = raw.get("value")
    return val if isinstance(val, dict) else {}


def parse_api_dir(api_dir: Path) -> ParsedNotionWeb:
    api_dir = Path(api_dir)
    out = ParsedNotionWeb()

    # Space — pick the first; there is one workspace per ingest in practice.
    for ev in load_jsonl(api_dir / "notion_space" / "created" / "events.jsonl"):
        v = _value(ev)
        if not v:
            continue
        out.space = SpaceRow(
            space_id=v.get("id") or ev.get("id") or "",
            name=v.get("name"),
            domain=v.get("domain"),
            raw_json=v,
        )
        break

    for ev in load_jsonl(api_dir / "notion_user" / "created" / "events.jsonl"):
        v = _value(ev)
        if not v:
            continue
        out.users.append(
            UserRow(
                user_id=v.get("id") or ev.get("id") or "",
                name=v.get("name"),
                email=v.get("email"),
                raw_json=v,
            )
        )

    for ev in load_jsonl(api_dir / "notion_block" / "created" / "events.jsonl"):
        v = _value(ev)
        if not v:
            continue
        out.blocks.append(
            BlockRow(
                block_id=v.get("id") or ev.get("id") or "",
                space_id=v.get("space_id") or ev.get("space_id"),
                type=v.get("type"),
                properties=v.get("properties") or {},
                format=v.get("format") or {},
                content=list(v.get("content") or []),
                parent_id=v.get("parent_id"),
                parent_table=v.get("parent_table"),
                created_time_ms=v.get("created_time"),
                last_edited_time_ms=v.get("last_edited_time"),
                created_by_id=v.get("created_by_id"),
                last_edited_by_id=v.get("last_edited_by_id"),
                alive=bool(v.get("alive", True)),
                collection_id=v.get("collection_id"),
                view_ids=list(v.get("view_ids") or []),
                raw_json=v,
            )
        )

    for ev in load_jsonl(api_dir / "notion_collection" / "created" / "events.jsonl"):
        v = _value(ev)
        if not v:
            continue
        out.collections.append(
            CollectionRow(
                collection_id=v.get("id") or ev.get("id") or "",
                name_plain=rich_text_to_plain(v.get("name")),
                parent_id=v.get("parent_id"),
                parent_table=v.get("parent_table"),
                schema=v.get("schema") or {},
                raw_json=v,
            )
        )

    for ev in load_jsonl(
        api_dir / "notion_collection_view" / "created" / "events.jsonl"
    ):
        v = _value(ev)
        if not v:
            continue
        out.views.append(
            CollectionViewRow(
                view_id=v.get("id") or ev.get("id") or "",
                name=v.get("name"),
                type=v.get("type"),
                raw_json=v,
            )
        )

    for ev in load_jsonl(api_dir / "notion_discussion" / "created" / "events.jsonl"):
        v = _value(ev)
        if not v:
            continue
        out.discussions.append(
            DiscussionRow(
                discussion_id=v.get("id") or ev.get("id") or "",
                parent_id=v.get("parent_id"),
                parent_table=v.get("parent_table"),
                space_id=v.get("space_id"),
                resolved=bool(v.get("resolved")),
                comment_ids=list(v.get("comments") or []),
                context_plain=rich_text_to_plain(v.get("context")),
                raw_json=v,
            )
        )

    for ev in load_jsonl(api_dir / "notion_comment" / "created" / "events.jsonl"):
        v = _value(ev)
        if not v:
            continue
        out.comments.append(
            CommentRow(
                comment_id=v.get("id") or ev.get("id") or "",
                discussion_id=v.get("parent_id")
                if v.get("parent_table") == "discussion"
                else None,
                space_id=v.get("space_id"),
                created_by_id=v.get("created_by_id"),
                created_time_ms=v.get("created_time"),
                last_edited_time_ms=v.get("last_edited_time"),
                text_plain=rich_text_to_plain(v.get("text")),
                raw_json=v,
            )
        )

    return out
