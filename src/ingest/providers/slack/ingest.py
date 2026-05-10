"""Parse one Slack api dir into an in-memory `ParsedSlackApi`.

We no longer maintain provider-specific Dolt tables; the parsed dataclass
is the source of truth for both QMD rendering and grid_rows population.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from pathlib import Path

from ingest.providers.slack.parse import ParsedSlackApi, parse_api_dir

log = logging.getLogger(__name__)


@dataclass
class SlackIngestStats:
    workspaces: int = 0
    users: int = 0
    channels: int = 0
    messages: int = 0
    reactions: int = 0


def ingest_api_dir(api_dir: Path) -> tuple[ParsedSlackApi, SlackIngestStats]:
    """Parse a Slack-API events directory (the layout produced by
    `src/download/slack_web.py`)."""
    parsed = parse_api_dir(api_dir)
    log.info(
        "slack: parsed workspaces=%d users=%d channels=%d messages=%d reactions=%d",
        len(parsed.workspaces),
        len(parsed.users),
        len(parsed.channels),
        len(parsed.messages),
        len(parsed.reactions),
    )
    stats = SlackIngestStats(
        workspaces=len(parsed.workspaces),
        users=len(parsed.users),
        channels=len(parsed.channels),
        messages=len(parsed.messages),
        reactions=len(parsed.reactions),
    )
    return parsed, stats


def merge_slack(items: list[ParsedSlackApi]) -> ParsedSlackApi:
    """Concatenate parsed Slack inputs. Today only one source is
    configured, so single-input is the common case."""
    if not items:
        return ParsedSlackApi()
    if len(items) == 1:
        return items[0]
    workspaces: dict = {}
    users: dict = {}
    channels: dict = {}
    messages: dict = {}
    reactions: dict = {}
    for p in items:
        for w in p.workspaces:
            workspaces[w.team_id] = w
        for u in p.users:
            users[(u.team_id, u.user_id)] = u
        for c in p.channels:
            channels[(c.team_id, c.channel_id)] = c
        for m in p.messages:
            messages[m.uuid] = m
        for r in p.reactions:
            reactions[r.uuid] = r
    return ParsedSlackApi(
        workspaces=list(workspaces.values()),
        users=list(users.values()),
        channels=list(channels.values()),
        messages=list(messages.values()),
        reactions=list(reactions.values()),
    )
