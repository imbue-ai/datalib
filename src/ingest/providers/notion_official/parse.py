"""Parse the event-stream layout written by `src/download/notion_official.py`:

    <api_dir>/notion_official_page/{created,updated}/events.jsonl
    <api_dir>/notion_official_block/{created,updated}/events.jsonl
    <api_dir>/notion_official_comment/{created,updated}/events.jsonl

We also opportunistically read the unofficial-API user table for display
names (`notion_user`) and the unofficial block table for media URLs +
bookmark titles (the official API doesn't sign `prod-files-secure` URLs
and serves bookmarks without titles). Those fixtures co-located here are
maintained by the same backups directory the official downloader writes
into; nothing in this module *requires* them.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

from jsonl_io import load_jsonl

ENTITY_PAGE = "notion_official_page"
ENTITY_BLOCK = "notion_official_block"
ENTITY_COMMENT = "notion_official_comment"


@dataclass
class ParsedNotionOfficial:
    pages: list[dict] = field(default_factory=list)
    blocks: list[dict] = field(default_factory=list)
    comments: list[dict] = field(default_factory=list)
    user_names: dict[str, str] = field(default_factory=dict)
    media_urls: dict[str, str] = field(default_factory=dict)
    bookmark_titles: dict[str, str] = field(default_factory=dict)


def _load_latest(api_dir: Path, entity: str) -> list[dict]:
    """Walk created + updated and return the latest `raw` per id."""
    latest: dict[str, dict] = {}
    for stream in ("created", "updated"):
        path = api_dir / entity / stream / "events.jsonl"
        if not path.exists():
            continue
        for rec in load_jsonl(path):
            rid = rec.get("id")
            if rid is None:
                continue
            latest[rid] = rec.get("raw") or {}
    return list(latest.values())


def _user_names_from_unofficial(api_dir: Path) -> dict[str, str]:
    """Lift display names from the unofficial-API `notion_user` table if
    present. Best-effort — comment authors with no match render as a short
    UUID."""
    out: dict[str, str] = {}
    for stream in ("created", "updated"):
        path = api_dir / "notion_user" / stream / "events.jsonl"
        if not path.exists():
            continue
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


def _block_lookups_from_unofficial(
    api_dir: Path,
) -> tuple[dict[str, str], dict[str, str]]:
    """Walk the unofficial `notion_block` table once and return:
      - media_urls: block id → source URL for image/video/audio/pdf/file
      - bookmark_titles: block id → title cached by Notion for bookmarks
    These fill gaps the official API leaves: PAT tokens can't sign
    `prod-files-secure` URLs, and bookmark blocks come back with only the
    raw URL (no title)."""
    media_urls: dict[str, str] = {}
    bookmark_titles: dict[str, str] = {}
    media_types = {"image", "video", "audio", "pdf", "file"}

    def _first(props: dict, key: str) -> str:
        v = (props or {}).get(key)
        if isinstance(v, list) and v and isinstance(v[0], list) and v[0]:
            return v[0][0] or ""
        return ""

    for stream in ("created", "updated"):
        path = api_dir / "notion_block" / stream / "events.jsonl"
        if not path.exists():
            continue
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


def parse_api_dir(api_dir: Path) -> ParsedNotionOfficial:
    media_urls, bookmark_titles = _block_lookups_from_unofficial(api_dir)
    return ParsedNotionOfficial(
        pages=_load_latest(api_dir, ENTITY_PAGE),
        blocks=_load_latest(api_dir, ENTITY_BLOCK),
        comments=_load_latest(api_dir, ENTITY_COMMENT),
        user_names=_user_names_from_unofficial(api_dir),
        media_urls=media_urls,
        bookmark_titles=bookmark_titles,
    )
