from __future__ import annotations

import logging
from dataclasses import dataclass
from pathlib import Path

from ingest.providers.notion_official.parse import (
    ParsedNotionOfficial,
    parse_api_dir,
)

log = logging.getLogger(__name__)


@dataclass
class NotionOfficialIngestStats:
    pages: int = 0
    blocks: int = 0
    comments: int = 0
    users: int = 0


def ingest_official_dir(
    api_dir: Path,
) -> tuple[ParsedNotionOfficial, NotionOfficialIngestStats]:
    parsed = parse_api_dir(api_dir)
    stats = NotionOfficialIngestStats(
        pages=len(parsed.pages),
        blocks=len(parsed.blocks),
        comments=len(parsed.comments),
        users=len(parsed.user_names),
    )
    log.info(
        "notion_official: parsed pages=%d blocks=%d comments=%d users=%d",
        stats.pages,
        stats.blocks,
        stats.comments,
        stats.users,
    )
    return parsed, stats


def merge_notion_official(
    items: list[ParsedNotionOfficial],
) -> ParsedNotionOfficial:
    if not items:
        return ParsedNotionOfficial()
    if len(items) == 1:
        return items[0]
    pages: dict[str, dict] = {}
    blocks: dict[str, dict] = {}
    comments: dict[str, dict] = {}
    user_names: dict[str, str] = {}
    media_urls: dict[str, str] = {}
    bookmark_titles: dict[str, str] = {}
    for p in items:
        for pg in p.pages:
            pages[pg["id"]] = pg
        for b in p.blocks:
            blocks[b["id"]] = b
        for c in p.comments:
            comments[c["id"]] = c
        user_names.update(p.user_names)
        media_urls.update(p.media_urls)
        bookmark_titles.update(p.bookmark_titles)
    return ParsedNotionOfficial(
        pages=list(pages.values()),
        blocks=list(blocks.values()),
        comments=list(comments.values()),
        user_names=user_names,
        media_urls=media_urls,
        bookmark_titles=bookmark_titles,
    )
