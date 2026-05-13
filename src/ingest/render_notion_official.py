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
from pathlib import Path
from typing import Any

import typer

from jsonl_io import load_jsonl

logger = logging.getLogger(__name__)

ENTITY_PAGE = "notion_official_page"
ENTITY_BLOCK = "notion_official_block"

SLUG_MAX_LEN = 60
_SLUG_RE = re.compile(r"[^a-z0-9]+")


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
    descendants. Caller joins with '\\n'."""
    btype = block.get("type") or ""
    payload = _block_payload(block)
    indent = "    " * depth
    lines: list[str] = []

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


def _load_all_blocks(out_dir: Path) -> list[dict]:
    """Walk created + updated to assemble the latest snapshot per block."""
    latest: dict[str, dict] = {}
    for stream in ("created", "updated"):
        path = out_dir / ENTITY_BLOCK / stream / "events.jsonl"
        if not path.exists():
            continue
        for rec in load_jsonl(path):
            latest[rec["id"]] = rec["raw"]
    return list(latest.values())


def _load_all_pages(out_dir: Path) -> list[dict]:
    latest: dict[str, dict] = {}
    for stream in ("created", "updated"):
        path = out_dir / ENTITY_PAGE / stream / "events.jsonl"
        if not path.exists():
            continue
        for rec in load_jsonl(path):
            latest[rec["id"]] = rec["raw"]
    return list(latest.values())


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
    out_root: Path,
) -> Path:
    pid = page["id"]
    title = page_titles.get(pid) or "(untitled)"
    seg = _page_dir_segment(pid, title)
    page_dir = out_root / seg
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


def render(
    subtree: str = typer.Option(
        ...,
        "--subtree",
        help="Root page id (UUID) to render. Renders this page + every "
        "descendant child_page found in the event store.",
    ),
    out: Path = typer.Option(
        Path("/tmp/notion_official_render"),
        "--out",
        help="Output directory (default /tmp/notion_official_render).",
    ),
    backups_dir: Path = typer.Option(
        Path.home() / "backups" / "notion",
        "--backups-dir",
    ),
    verbose: bool = typer.Option(False, "--verbose", "-v"),
) -> None:
    logging.basicConfig(
        level=logging.DEBUG if verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    out = out.expanduser()
    backups_dir = backups_dir.expanduser()

    blocks = _load_all_blocks(backups_dir)
    pages = _load_all_pages(backups_dir)
    typer.echo(f"loaded {len(pages)} pages, {len(blocks)} blocks")

    children_by_parent = _index_children(blocks)
    page_titles = _build_page_titles(pages, blocks)
    user_names = _user_names_from_unofficial(backups_dir)
    media_urls, bookmark_titles = _unofficial_block_lookups(backups_dir)

    # BFS from the requested root, visiting only pages we actually have.
    raw = subtree.replace("-", "")
    root = f"{raw[0:8]}-{raw[8:12]}-{raw[12:16]}-{raw[16:20]}-{raw[20:32]}"
    pages_by_id = {p["id"]: p for p in pages}

    out.mkdir(parents=True, exist_ok=True)
    rendered = 0
    queue: list[str] = [root]
    seen: set[str] = set()
    while queue:
        pid = queue.pop(0)
        if pid in seen:
            continue
        seen.add(pid)
        page = pages_by_id.get(pid)
        if page is None:
            typer.echo(f"  ! page {pid[:8]} not in event store; skipping")
            continue
        _render_page(
            page,
            children_by_parent=children_by_parent,
            user_names=user_names,
            page_titles=page_titles,
            media_urls=media_urls,
            bookmark_titles=bookmark_titles,
            out_root=out,
        )
        rendered += 1
        # Enqueue every child_page anywhere under this page.
        stack = list(children_by_parent.get(pid, []))
        while stack:
            b = stack.pop()
            if b.get("type") == "child_page":
                if b["id"] not in seen:
                    queue.append(b["id"])
            else:
                stack.extend(children_by_parent.get(b["id"], []))
    typer.echo(f"rendered {rendered} pages → {out}")


def _user_names_from_unofficial(backups_dir: Path) -> dict[str, str]:
    """Borrow the user table from the unofficial-API mirror — the official
    API would also serve /v1/users, but we already have names locally and
    this avoids re-fetching them."""
    path = backups_dir / "notion_user" / "updated" / "events.jsonl"
    if not path.exists():
        return {}
    out: dict[str, str] = {}
    for rec in load_jsonl(path):
        raw = rec.get("raw") or {}
        val = raw.get("value") or {}
        if "value" in val and isinstance(val["value"], dict):
            val = val["value"]
        uid = val.get("id") or rec.get("id")
        name = val.get("name") or val.get("given_name") or ""
        if uid and name:
            out[uid] = name
    return out


def _unofficial_block_lookups(
    backups_dir: Path,
) -> tuple[dict[str, str], dict[str, str]]:
    """Walk the unofficial-API mirror once and return:
      - media_urls: block id → source URL for image/video/audio/pdf/file
      - bookmark_titles: block id → title cached by Notion for bookmarks

    Both fill gaps in the official API's response: PAT tokens can't sign
    `prod-files-secure` URLs, and bookmark blocks come back with only the
    raw URL (no title)."""
    path = backups_dir / "notion_block" / "updated" / "events.jsonl"
    media_urls: dict[str, str] = {}
    bookmark_titles: dict[str, str] = {}
    if not path.exists():
        return media_urls, bookmark_titles
    media_types = {"image", "video", "audio", "pdf", "file"}

    def _first(props: dict, key: str) -> str:
        v = (props or {}).get(key)
        if isinstance(v, list) and v and isinstance(v[0], list) and v[0]:
            return v[0][0] or ""
        return ""

    for rec in load_jsonl(path):
        raw = rec.get("raw") or {}
        val = raw.get("value") or {}
        if "value" in val and isinstance(val["value"], dict):
            val = val["value"]
        t = val.get("type")
        bid = val.get("id") or rec.get("id")
        if not bid:
            continue
        props = val.get("properties") or {}
        if t in media_types:
            url = _first(props, "source")
            if url:
                media_urls[bid] = url
        elif t == "bookmark":
            title = _first(props, "title")
            if title:
                bookmark_titles[bid] = title
    return media_urls, bookmark_titles


def main() -> None:
    typer.run(render)


if __name__ == "__main__":
    sys.exit(main() or 0)
