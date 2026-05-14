#!/usr/bin/env python3
"""Render official-API Notion pages (mirrored by `download/notion_official.py`)
to markdown.

Sibling to the unofficial-API renderer in `ingest/render.py`. Lives in its
own module so the two pipelines can run side-by-side during the migration.

Input: event-store entities `notion_official_page` + `notion_official_block`.
Output: one `<slug>__<id8>/index.md` per page under `--out`. Sub-pages are
materialized as siblings at the same level and linked via relative paths.

Block-type coverage: paragraph / heading_{1,2,3} / bulleted_list_item /
numbered_list_item / to_do / toggle / quote / callout / code / divider /
image / video / file / pdf / audio / embed / bookmark / link_preview /
table / table_row / child_page / child_database / synced_block /
column_list / column / table_of_contents / link_to_page / equation /
breadcrumb / unsupported.

For rich text we reuse notion2md's annotation map (bold/italic/code/etc.)
but route mention spans through our own resolver so user / page / database
mentions render as readable names instead of opaque IDs.
"""

from __future__ import annotations

import json
import logging
import re
import sys
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import typer

from ingest.providers.notion_official.parse import (
    ParsedNotionOfficial,
    parse_api_dir,
)

logger = logging.getLogger(__name__)

SLUG_MAX_LEN = 60
_SLUG_RE = re.compile(r"[^a-z0-9]+")


@dataclass
class RenderSummary:
    rendered: int = 0
    orphans_removed: int = 0
    skipped: int = 0


# ---------------------------------------------------------------------------
# Slug / path helpers (mirror those in ingest/render.py so output is
# comparable side-by-side).
# ---------------------------------------------------------------------------


def _slugify(name: str | None) -> str:
    if not name:
        return "untitled"
    s = _SLUG_RE.sub("-", name.lower()).strip("-")
    if not s:
        return "untitled"
    return s[:SLUG_MAX_LEN].rstrip("-") or "untitled"


def _short_id(uuid_str: str) -> str:
    return (uuid_str or "").split("-", 1)[0][:8] or "00000000"


def _page_dir_segment(page_id: str, title: str | None) -> str:
    return f"{_slugify(title)}__{_short_id(page_id)}"


def _notion_url(page_id: str) -> str:
    return f"https://www.notion.so/{page_id.replace('-', '')}"


def _notion_thread_url(
    page_id: str, discussion_id: str | None, anchor_block_id: str | None
) -> str:
    """Open the page with the comment side panel pinned to the thread.
    Mirrors the format produced by the unofficial-API renderer (Notion
    accepts both dashed and undashed)."""
    pg = page_id.replace("-", "")
    url = f"https://www.notion.so/{pg}"
    if discussion_id:
        url += f"?d={discussion_id.replace('-', '')}"
        if anchor_block_id:
            url += f"#{anchor_block_id.replace('-', '')}"
    elif anchor_block_id:
        url += f"#{anchor_block_id.replace('-', '')}"
    return url


def _block_anchor(block_id: str) -> str:
    """HTML anchor placed before each block's rendered content so thread
    .md files can deep-link back to the block."""
    return f'<a id="b-{_short_id(block_id)}"></a>'


def _yaml_scalar(v: object) -> str:
    if v is None:
        return "null"
    s = str(v)
    if any(c in s for c in ":#\n\"'") or s != s.strip():
        return json.dumps(s, ensure_ascii=False)
    return s


# ---------------------------------------------------------------------------
# Rich text → markdown. Reuses notion2md's annotation conventions but our
# own mention resolver (notion2md outputs `(name)` for users and `([t](url])`
# for pages — both broken/ugly).
# ---------------------------------------------------------------------------


def _rich_text_plain(rt: list[dict] | None) -> str:
    if not rt:
        return ""
    return "".join(span.get("plain_text", "") for span in rt)


def _wrap_annotations(text: str, ann: dict[str, Any] | None) -> str:
    if not ann or not text:
        return text
    if ann.get("code"):
        text = f"`{text}`"
    if ann.get("bold"):
        text = f"**{text}**"
    if ann.get("italic"):
        text = f"*{text}*"
    if ann.get("strikethrough"):
        text = f"~~{text}~~"
    if ann.get("underline"):
        text = f"<u>{text}</u>"
    color = ann.get("color")
    if color and color != "default":
        text = f"<span style='color:{color}'>{text}</span>"
    return text


def _render_rich_text(
    rt: list[dict] | None,
    user_names: dict[str, str],
    page_titles: dict[str, str],
) -> str:
    if not rt:
        return ""
    out: list[str] = []
    for span in rt:
        t = span.get("type")
        plain = span.get("plain_text", "")
        ann = span.get("annotations") or {}
        if t == "mention":
            m = span.get("mention") or {}
            mtype = m.get("type")
            if mtype == "user":
                uid = (m.get("user") or {}).get("id", "")
                name = user_names.get(uid) or plain or uid[:8]
                rendered = f"@{name}"
            elif mtype == "page":
                pid = (m.get("page") or {}).get("id", "")
                title = page_titles.get(pid) or plain or "(untitled page)"
                rendered = f"[{title}]({_notion_url(pid)})"
            elif mtype == "database":
                did = (m.get("database") or {}).get("id", "")
                title = page_titles.get(did) or plain or "(untitled db)"
                rendered = f"[{title}]({_notion_url(did)})"
            elif mtype == "date":
                d = m.get("date") or {}
                rendered = d.get("start") or plain
            elif mtype == "link_preview":
                url = (m.get("link_preview") or {}).get("url", "")
                rendered = f"[{plain or url}]({url})"
            else:
                rendered = plain
            out.append(_wrap_annotations(rendered, ann))
            continue
        if t == "equation":
            expr = (span.get("equation") or {}).get("expression", plain)
            out.append(f"${expr}$")
            continue
        # Default: text span. May have href.
        href = span.get("href")
        if href:
            out.append(_wrap_annotations(f"[{plain}]({href})", ann))
        else:
            out.append(_wrap_annotations(plain, ann))
    return "".join(out)


# ---------------------------------------------------------------------------
# Block dispatch.
#
# A block dict is the raw API payload. Its "type" key names a sub-dict with
# the type-specific fields (rich_text, url, etc.). has_children is set when
# the block has descendants — we look them up in `children_by_parent`.
# ---------------------------------------------------------------------------


def _block_payload(block: dict) -> dict:
    t = block.get("type") or ""
    return block.get(t) or {}


def _children_of(
    block_id: str, children_by_parent: dict[str, list[dict]]
) -> list[dict]:
    return children_by_parent.get(block_id, [])


def _render_block(
    block: dict,
    *,
    children_by_parent: dict[str, list[dict]],
    user_names: dict[str, str],
    page_titles: dict[str, str],
    sub_pages_dir: dict[str, str],  # child_page id → relative dir of its index.md
    media_urls: dict[str, str],
    bookmark_titles: dict[str, str],
    depth: int = 0,
) -> list[str]:
    """Return markdown lines (no trailing newline) for one block + its
    descendants. Caller joins with '\\n'.

    The first line emitted is an HTML anchor so threads anchored to this
    block can deep-link back."""
    btype = block.get("type") or ""
    payload = _block_payload(block)
    indent = "    " * depth
    lines: list[str] = [_block_anchor(block.get("id", ""))]

    def rt(field: str = "rich_text") -> str:
        return _render_rich_text(payload.get(field), user_names, page_titles)

    def recurse_children(extra_depth: int = 0) -> list[str]:
        out: list[str] = []
        for ch in _children_of(block["id"], children_by_parent):
            out.extend(
                _render_block(
                    ch,
                    children_by_parent=children_by_parent,
                    user_names=user_names,
                    page_titles=page_titles,
                    sub_pages_dir=sub_pages_dir,
                    media_urls=media_urls,
                    bookmark_titles=bookmark_titles,
                    depth=depth + extra_depth,
                )
            )
        return out

    if btype == "paragraph":
        text = rt()
        if text:
            lines.append(f"{indent}{text}")
        lines.append("")
        lines.extend(recurse_children(extra_depth=1))
    elif btype == "heading_1":
        lines.append(f"# {rt()}")
        lines.append("")
    elif btype == "heading_2":
        lines.append(f"## {rt()}")
        lines.append("")
    elif btype == "heading_3":
        lines.append(f"### {rt()}")
        lines.append("")
    elif btype == "bulleted_list_item":
        lines.append(f"{indent}- {rt()}")
        lines.extend(recurse_children(extra_depth=1))
    elif btype == "numbered_list_item":
        lines.append(f"{indent}1. {rt()}")
        lines.extend(recurse_children(extra_depth=1))
    elif btype == "to_do":
        box = "[x]" if payload.get("checked") else "[ ]"
        lines.append(f"{indent}- {box} {rt()}")
        lines.extend(recurse_children(extra_depth=1))
    elif btype == "toggle":
        lines.append(f"{indent}<details><summary>{rt()}</summary>")
        lines.append("")
        lines.extend(recurse_children(extra_depth=1))
        lines.append(f"{indent}</details>")
        lines.append("")
    elif btype == "quote":
        lines.append(f"> {rt()}")
        lines.append("")
        lines.extend(recurse_children(extra_depth=1))
    elif btype == "callout":
        icon_obj = payload.get("icon") or {}
        icon = icon_obj.get("emoji") or "💡"
        lines.append(f"> {icon} {rt()}")
        lines.append("")
        lines.extend(recurse_children(extra_depth=1))
    elif btype == "code":
        lang = (payload.get("language") or "").lower()
        text = _rich_text_plain(payload.get("rich_text"))
        lines.append(f"```{lang}")
        lines.append(text)
        lines.append("```")
        caption = rt("caption")
        if caption:
            lines.append("")
            lines.append(f"*{caption}*")
        lines.append("")
    elif btype == "divider":
        lines.append("---")
        lines.append("")
    elif btype == "image":
        url = _media_url(payload) or media_urls.get(block["id"], "")
        caption = rt("caption") or "image"
        if url:
            lines.append(f"![{caption}]({url})")
        else:
            lines.append(f"*(image: {caption})*")
        lines.append("")
    elif btype in ("video", "audio", "pdf", "file"):
        url = _media_url(payload) or media_urls.get(block["id"], "")
        caption = rt("caption") or btype
        name = (payload.get("name") if btype == "file" else None) or caption
        icon = {"video": "🎬", "audio": "🎵", "pdf": "📄", "file": "📎"}[btype]
        if url:
            lines.append(f"[{icon} {name}]({url})")
        else:
            lines.append(f"*({btype}: {name})*")
        lines.append("")
    elif btype == "embed":
        url = payload.get("url") or ""
        caption = rt("caption")
        lines.append(f"[{caption or url}]({url})")
        lines.append("")
    elif btype == "bookmark":
        url = payload.get("url") or ""
        caption = rt("caption") or bookmark_titles.get(block["id"], "")
        lines.append(f"[{caption or url}]({url})")
        lines.append("")
    elif btype == "link_preview":
        url = payload.get("url") or ""
        # link_preview is what the unofficial API called external_object_instance:
        # an embedded reference to Google Doc / Drive / Linear / Figma / etc.
        # We have no nice preview text — show the URL.
        lines.append(f"🔗 [{url}]({url})")
        lines.append("")
    elif btype == "link_to_page":
        target_type = payload.get("type")
        target_id = payload.get(target_type or "") if target_type else None
        title = page_titles.get(target_id or "") or "(linked page)"
        if target_id and target_id in sub_pages_dir:
            href = f"../{sub_pages_dir[target_id]}/index.md"
        else:
            href = _notion_url(target_id) if target_id else "#"
        lines.append(f"{indent}🔗 [{title}]({href})")
        lines.append("")
    elif btype == "child_page":
        title = payload.get("title") or "(untitled)"
        target_id = block["id"]
        href = f"../{sub_pages_dir.get(target_id, _page_dir_segment(target_id, title))}/index.md"
        lines.append(f"{indent}- 📄 [{title}]({href})")
        lines.append("")
    elif btype == "child_database":
        title = payload.get("title") or "(database)"
        lines.append(f"{indent}*[📊 Database: {title}]*")
        lines.append("")
    elif btype == "synced_block":
        synced_from = payload.get("synced_from")
        if synced_from is None:
            # original — render its children inline.
            lines.extend(recurse_children(extra_depth=0))
        else:
            # reference to another block; its children carry the resolved
            # content (Notion populates `has_children` either way).
            src_id = synced_from.get("block_id")
            lines.append(f"{indent}<!-- synced from {src_id} -->")
            lines.extend(recurse_children(extra_depth=0))
    elif btype == "column_list":
        lines.extend(recurse_children(extra_depth=0))
    elif btype == "column":
        lines.extend(recurse_children(extra_depth=0))
    elif btype == "table":
        # Children are table_row blocks. Build a markdown table.
        rows = _children_of(block["id"], children_by_parent)
        has_header = bool(payload.get("has_column_header"))
        rendered_rows: list[list[str]] = []
        for r in rows:
            if r.get("type") != "table_row":
                continue
            cells = (r.get("table_row") or {}).get("cells") or []
            rendered_rows.append(
                [_render_rich_text(c, user_names, page_titles) for c in cells]
            )
        if rendered_rows:
            ncols = max(len(r) for r in rendered_rows)
            header = rendered_rows[0] if has_header else [""] * ncols
            lines.append("| " + " | ".join(header) + " |")
            lines.append("| " + " | ".join(["---"] * ncols) + " |")
            for r in rendered_rows[1:] if has_header else rendered_rows:
                lines.append("| " + " | ".join(r) + " |")
            lines.append("")
    elif btype == "table_of_contents":
        # We don't have a robust way to compute headings ahead of time;
        # leave a placeholder. Most consumers (Quartz, Hugo) generate their
        # own TOC anyway.
        lines.append("*[table of contents]*")
        lines.append("")
    elif btype == "breadcrumb":
        lines.append("*[breadcrumb]*")
        lines.append("")
    elif btype == "equation":
        expr = payload.get("expression") or ""
        lines.append(f"$$ {expr} $$")
        lines.append("")
    elif btype == "unsupported":
        lines.append("*[unsupported block type]*")
        lines.append("")
    else:
        # Fallback: emit any rich_text content; warn for visibility.
        text = rt()
        if text:
            lines.append(f"{indent}{text}")
        else:
            lines.append(f"{indent}*[unhandled block: {btype}]*")
        lines.append("")
        logger.warning("unhandled official-API block type: %s", btype)

    return lines


def _media_url(payload: dict) -> str:
    """Image/video/file/audio/pdf blocks have either `external.url` or
    `file.url` (Notion-hosted, signed)."""
    ext = payload.get("external") or {}
    if ext.get("url"):
        return ext["url"]
    f = payload.get("file") or {}
    return f.get("url") or ""


# ---------------------------------------------------------------------------
# Page title resolution.
#
# A page record from /v1/pages has properties: { title: { title: [rt...] } }
# or { Name: { title: [rt...] } } for database rows. A child_page block
# carries the title under block.child_page.title (already plain text).
# ---------------------------------------------------------------------------


def _page_title(page: dict) -> str:
    props = page.get("properties") or {}
    for prop in props.values():
        if prop.get("type") == "title":
            return _rich_text_plain(prop.get("title")) or "(untitled)"
    return "(untitled)"


def _build_page_titles(pages: list[dict], blocks: list[dict]) -> dict[str, str]:
    out: dict[str, str] = {}
    for p in pages:
        out[p["id"]] = _page_title(p)
    # child_page blocks carry the title too — useful when we have the parent
    # block but not the child's own page record (e.g. partial subtrees).
    for b in blocks:
        if b.get("type") == "child_page":
            out.setdefault(b["id"], (b.get("child_page") or {}).get("title") or "")
    return out


# ---------------------------------------------------------------------------
# Top-level render.
# ---------------------------------------------------------------------------


def _index_children(blocks: list[dict]) -> dict[str, list[dict]]:
    """Map parent (block or page) id → its direct children, preserving the
    fetcher's insertion order (which mirrors Notion's API order)."""
    by_parent: dict[str, list[dict]] = defaultdict(list)
    for b in blocks:
        parent = b.get("parent") or {}
        ptype = parent.get("type")
        if ptype == "page_id":
            by_parent[parent["page_id"]].append(b)
        elif ptype == "block_id":
            by_parent[parent["block_id"]].append(b)
    return dict(by_parent)


def _render_page(
    page: dict,
    *,
    children_by_parent: dict[str, list[dict]],
    user_names: dict[str, str],
    page_titles: dict[str, str],
    media_urls: dict[str, str],
    bookmark_titles: dict[str, str],
    pages_root: Path,
) -> Path:
    pid = page["id"]
    title = page_titles.get(pid) or "(untitled)"
    seg = _page_dir_segment(pid, title)
    page_dir = pages_root / seg
    page_dir.mkdir(parents=True, exist_ok=True)

    # Build a flat map of every child_page (any depth) → its directory
    # segment, so cross-page links resolve to the right path.
    sub_pages_dir: dict[str, str] = {}
    stack = list(children_by_parent.get(pid, []))
    while stack:
        b = stack.pop()
        if b.get("type") == "child_page":
            sub_pages_dir[b["id"]] = _page_dir_segment(
                b["id"], (b.get("child_page") or {}).get("title")
            )
        else:
            stack.extend(children_by_parent.get(b["id"], []))

    parts: list[str] = []
    parts.append("---")
    parts.append("provider: notion_official")
    parts.append(f"page_id: {_yaml_scalar(pid)}")
    parts.append(f"title: {_yaml_scalar(title)}")
    parts.append(f"created_time: {_yaml_scalar(page.get('created_time'))}")
    parts.append(f"last_edited_time: {_yaml_scalar(page.get('last_edited_time'))}")
    parts.append("---")
    parts.append("")
    icon_obj = page.get("icon") or {}
    icon = icon_obj.get("emoji") or ""
    parts.append(f"# {icon + ' ' if icon else ''}{title}")
    parts.append("")
    parts.append(f"[View on Notion ↗]({_notion_url(pid)})")
    parts.append("")

    for ch in children_by_parent.get(pid, []):
        parts.extend(
            _render_block(
                ch,
                children_by_parent=children_by_parent,
                user_names=user_names,
                page_titles=page_titles,
                sub_pages_dir=sub_pages_dir,
                media_urls=media_urls,
                bookmark_titles=bookmark_titles,
            )
        )

    target = page_dir / "index.md"
    target.write_text("\n".join(parts).rstrip() + "\n")
    return target


# ---------------------------------------------------------------------------
# Comment threads.
#
# Notion's official API exposes comments as a flat list per page (or block);
# each carries a `discussion_id` that groups thread members. We render one
# `.md` per discussion, located under the page's directory, deep-linked back
# to the anchor block via the `<a id="b-<id8>">` markers _render_block emits.
# ---------------------------------------------------------------------------


def _block_to_page_id(blocks: list[dict]) -> dict[str, str]:
    out: dict[str, str] = {}
    for b in blocks:
        parent = b.get("parent") or {}
        if parent.get("type") == "page_id":
            out[b["id"]] = parent["page_id"]
    return out


def _resolve_comment_page_id(
    comment: dict, blocks: list[dict], block_owning_page: dict[str, str]
) -> str | None:
    parent = comment.get("parent") or {}
    ptype = parent.get("type")
    if ptype == "page_id":
        return parent.get("page_id")
    if ptype == "block_id":
        bid = parent.get("block_id")
        if not bid:
            return None
        # Walk up the block hierarchy: the comment fixture might point at a
        # block whose parent.type is `block_id`, not `page_id`.
        block_parent: dict[str, str] = {}
        for b in blocks:
            par = b.get("parent") or {}
            if par.get("type") == "block_id":
                block_parent[b["id"]] = par["block_id"]
        cur: str | None = bid
        seen: set[str] = set()
        while cur and cur not in seen:
            seen.add(cur)
            if cur in block_owning_page:
                return block_owning_page[cur]
            cur = block_parent.get(cur)
    return None


def _comment_text_plain(comment: dict) -> str:
    return _rich_text_plain(comment.get("rich_text"))


def _thread_filename(discussion_id: str, snippet: str) -> str:
    snip = _slugify(snippet) or "thread"
    return f"{_short_id(discussion_id)}__{snip}.md"


def _format_iso_short(ts: str | None) -> str:
    """Notion timestamps are ISO 8601 with trailing 'Z'; pass through if so,
    else just return as-is. Don't try to be cute about timezones."""
    return ts or ""


def _render_thread(
    *,
    discussion_id: str,
    page_id: str,
    page_title: str,
    parent_block_id: str | None,
    comments: list[dict],
    user_names: dict[str, str],
    page_titles: dict[str, str],
    page_dir: Path,
) -> Path | None:
    """Write one thread .md under `<page_dir>/threads/`. Returns the path
    (absolute) on success, or None if no on-disk file should exist."""
    if not comments:
        return None
    comments = sorted(comments, key=lambda c: c.get("created_time") or "")
    first_text = _comment_text_plain(comments[0])
    snippet = (first_text.splitlines()[0] if first_text else "thread")[:60]

    threads_dir = page_dir / "threads"
    threads_dir.mkdir(parents=True, exist_ok=True)
    target = threads_dir / _thread_filename(discussion_id, snippet)

    thread_url = _notion_thread_url(page_id, discussion_id, parent_block_id)

    parts: list[str] = []
    parts.append("---")
    parts.append("provider: notion_official")
    parts.append(f"discussion_id: {_yaml_scalar(discussion_id)}")
    parts.append(f"page_id: {_yaml_scalar(page_id)}")
    if parent_block_id:
        parts.append(f"parent_block_id: {_yaml_scalar(parent_block_id)}")
    parts.append("---")
    parts.append("")
    parts.append(f"# Comment thread on “{page_title}”")
    parts.append("")
    parts.append(f"[View thread on Notion ↗]({thread_url})")
    parts.append("")
    if parent_block_id:
        anchor = f"../index.md#b-{_short_id(parent_block_id)}"
        parts.append(f"Anchored to [block ↩]({anchor})")
        parts.append("")

    for i, c in enumerate(comments):
        cid = c.get("id", "")
        author_id = (c.get("created_by") or {}).get("id") or ""
        author = user_names.get(author_id) or author_id[:8] or "unknown"
        created = _format_iso_short(c.get("created_time"))
        parts.append(f'<a id="c-{_short_id(cid)}"></a>')
        parts.append("")
        parts.append(f"## {author}")
        parts.append("")
        parts.append(f"*{created}* — [↗]({thread_url})")
        parts.append("")
        body = _render_rich_text(c.get("rich_text"), user_names, page_titles)
        parts.append(body or "")
        parts.append("")

    target.write_text("\n".join(parts).rstrip() + "\n")
    return target


# ---------------------------------------------------------------------------
# Library entry point (called from ingest.py) and CLI shim.
# ---------------------------------------------------------------------------


_PAGES_SUBDIR = Path("rendered_md") / "notion" / "pages"


def thread_qmd_path_rel(
    *, page_id: str, page_title: str, discussion_id: str, snippet: str
) -> str:
    """Return the rel path (under root) we'd write a thread to. Used by
    grid_rows to populate `qmd_path` without duplicating directory layout
    logic."""
    seg = _page_dir_segment(page_id, page_title)
    return str(
        _PAGES_SUBDIR / seg / "threads" / _thread_filename(discussion_id, snippet)
    )


def page_qmd_path_rel(*, page_id: str, page_title: str) -> str:
    seg = _page_dir_segment(page_id, page_title)
    return str(_PAGES_SUBDIR / seg / "index.md")


def thread_snippet(comment_rich_text_plain: str) -> str:
    return (
        comment_rich_text_plain.splitlines()[0] if comment_rich_text_plain else "thread"
    )[:60]


def render_notion_official(
    parsed: ParsedNotionOfficial,
    root: Path,
    skip: set[str] | None = None,
) -> RenderSummary:
    """Render every page in `parsed` to markdown under
    `<root>/rendered_md/notion/pages/<page-dir>/index.md`, with comment
    threads as siblings under `threads/`.

    `skip` is a set of `document_uuid` values whose corresponding rendered
    output is still fresh. For the official path, the only `document_uuid`s
    in the grid_rows stream are discussion IDs (pages don't currently
    produce grid rows under the new scheme), so `skip` only affects
    thread rendering.

    Orphan cleanup: pages/threads on disk that aren't in `parsed` get
    deleted so the rendered tree mirrors the source of truth."""
    summary = RenderSummary()
    skip = skip or set()
    pages_root = root / _PAGES_SUBDIR
    pages_root.mkdir(parents=True, exist_ok=True)

    pages = parsed.pages
    blocks = parsed.blocks
    comments = parsed.comments
    if not pages and not blocks and not comments:
        return summary

    children_by_parent = _index_children(blocks)
    page_titles = _build_page_titles(pages, blocks)
    pages_by_id = {p["id"]: p for p in pages}
    block_owning_page = _block_to_page_id(blocks)

    live_page_dirs: set[str] = set()
    live_thread_paths: set[Path] = set()

    for page in pages:
        pid = page["id"]
        title = page_titles.get(pid) or "(untitled)"
        seg = _page_dir_segment(pid, title)
        live_page_dirs.add(seg)
        _render_page(
            page,
            children_by_parent=children_by_parent,
            user_names=parsed.user_names,
            page_titles=page_titles,
            media_urls=parsed.media_urls,
            bookmark_titles=parsed.bookmark_titles,
            pages_root=pages_root,
        )
        summary.rendered += 1

    # Group comments by discussion_id, render one .md per group.
    by_disc: dict[str, list[dict]] = {}
    for c in comments:
        did = c.get("discussion_id")
        if not did:
            continue
        by_disc.setdefault(did, []).append(c)

    for disc_id, members in by_disc.items():
        # Use the first comment's parent to derive the anchor block + page.
        first = sorted(members, key=lambda c: c.get("created_time") or "")[0]
        page_id = _resolve_comment_page_id(first, blocks, block_owning_page)
        if page_id is None:
            continue
        page = pages_by_id.get(page_id)
        if page is None:
            continue
        title = page_titles.get(page_id) or "(untitled)"
        parent = first.get("parent") or {}
        parent_block_id = (
            parent.get("block_id") if parent.get("type") == "block_id" else None
        )
        page_dir = pages_root / _page_dir_segment(page_id, title)
        if disc_id in skip:
            # Still need to record the path so orphan cleanup doesn't delete it.
            snippet = thread_snippet(
                _comment_text_plain(
                    sorted(members, key=lambda c: c.get("created_time") or "")[0]
                )
            )
            live_thread_paths.add(
                page_dir / "threads" / _thread_filename(disc_id, snippet)
            )
            summary.skipped += 1
            continue
        path = _render_thread(
            discussion_id=disc_id,
            page_id=page_id,
            page_title=title,
            parent_block_id=parent_block_id,
            comments=members,
            user_names=parsed.user_names,
            page_titles=page_titles,
            page_dir=page_dir,
        )
        if path is not None:
            live_thread_paths.add(path)
            summary.rendered += 1

    # Orphan cleanup. Walk pages_root and delete page dirs / thread files
    # not in the live set. Skip the root itself.
    if pages_root.is_dir():
        for sub in pages_root.iterdir():
            if not sub.is_dir():
                continue
            if sub.name not in live_page_dirs:
                _rmtree(sub)
                summary.orphans_removed += 1
                continue
            threads_dir = sub / "threads"
            if threads_dir.is_dir():
                for f in threads_dir.iterdir():
                    if f.is_file() and f not in live_thread_paths:
                        f.unlink()
                        summary.orphans_removed += 1
                # Remove empty threads dir
                if not any(threads_dir.iterdir()):
                    threads_dir.rmdir()

    return summary


def _rmtree(path: Path) -> None:
    for child in path.iterdir():
        if child.is_dir():
            _rmtree(child)
        else:
            child.unlink()
    path.rmdir()


def render(
    backups_dir: Path = typer.Option(
        Path.home() / "backups" / "notion",
        "--backups-dir",
    ),
    out: Path = typer.Option(
        Path("/tmp/notion_official_render"),
        "--out",
        help="Project root for `rendered_md/notion/pages/...` output.",
    ),
    verbose: bool = typer.Option(False, "--verbose", "-v"),
) -> None:
    """Render every page+comment-thread in the backups dir to markdown."""
    logging.basicConfig(
        level=logging.DEBUG if verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    parsed = parse_api_dir(backups_dir.expanduser())
    typer.echo(
        f"loaded pages={len(parsed.pages)} blocks={len(parsed.blocks)} "
        f"comments={len(parsed.comments)}"
    )
    summary = render_notion_official(parsed, out.expanduser())
    typer.echo(
        f"rendered {summary.rendered}  skipped {summary.skipped}  "
        f"orphans removed {summary.orphans_removed}"
    )


def main() -> None:
    typer.run(render)


if __name__ == "__main__":
    sys.exit(main() or 0)
