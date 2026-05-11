from __future__ import annotations

import logging
from dataclasses import dataclass
from pathlib import Path

from ingest.providers.notion.parse import ParsedNotionWeb, parse_api_dir

log = logging.getLogger(__name__)


@dataclass
class NotionIngestStats:
    space: int = 0
    users: int = 0
    blocks: int = 0
    collections: int = 0
    views: int = 0
    discussions: int = 0
    comments: int = 0


def ingest_web_dir(api_dir: Path) -> tuple[ParsedNotionWeb, NotionIngestStats]:
    parsed = parse_api_dir(api_dir)
    stats = NotionIngestStats(
        space=1 if parsed.space else 0,
        users=len(parsed.users),
        blocks=len(parsed.blocks),
        collections=len(parsed.collections),
        views=len(parsed.views),
        discussions=len(parsed.discussions),
        comments=len(parsed.comments),
    )
    log.info(
        "notion: parsed space=%d users=%d blocks=%d collections=%d views=%d discussions=%d comments=%d",
        stats.space,
        stats.users,
        stats.blocks,
        stats.collections,
        stats.views,
        stats.discussions,
        stats.comments,
    )
    return parsed, stats


def merge_notion(items: list[ParsedNotionWeb]) -> ParsedNotionWeb:
    if not items:
        return ParsedNotionWeb()
    if len(items) == 1:
        return items[0]
    merged = ParsedNotionWeb()
    users: dict = {}
    blocks: dict = {}
    collections: dict = {}
    views: dict = {}
    discussions: dict = {}
    comments: dict = {}
    for p in items:
        if merged.space is None and p.space is not None:
            merged.space = p.space
        for u in p.users:
            users[u.user_id] = u
        for b in p.blocks:
            blocks[b.block_id] = b
        for c in p.collections:
            collections[c.collection_id] = c
        for v in p.views:
            views[v.view_id] = v
        for d in p.discussions:
            discussions[d.discussion_id] = d
        for cm in p.comments:
            comments[cm.comment_id] = cm
    merged.users = list(users.values())
    merged.blocks = list(blocks.values())
    merged.collections = list(collections.values())
    merged.views = list(views.values())
    merged.discussions = list(discussions.values())
    merged.comments = list(comments.values())
    return merged
