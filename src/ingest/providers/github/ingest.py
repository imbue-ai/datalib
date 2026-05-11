"""Parse one GitHub api dir into an in-memory `ParsedGithubApi`."""

from __future__ import annotations

import logging
from dataclasses import dataclass
from pathlib import Path

from ingest.providers.github.parse import ParsedGithubApi, parse_api_dir

log = logging.getLogger(__name__)


@dataclass
class GithubIngestStats:
    pull_requests: int = 0
    comments: int = 0


def ingest_api_dir(api_dir: Path) -> tuple[ParsedGithubApi, GithubIngestStats]:
    parsed = parse_api_dir(api_dir)
    log.info(
        "github: parsed pull_requests=%d comments=%d",
        len(parsed.pull_requests),
        len(parsed.comments),
    )
    stats = GithubIngestStats(
        pull_requests=len(parsed.pull_requests),
        comments=len(parsed.comments),
    )
    return parsed, stats


def merge_github(items: list[ParsedGithubApi]) -> ParsedGithubApi:
    if not items:
        return ParsedGithubApi()
    if len(items) == 1:
        return items[0]
    prs: dict = {}
    comments: dict = {}
    self_identity = None
    for p in items:
        if p.self_identity is not None:
            self_identity = p.self_identity
        for pr in p.pull_requests:
            prs[pr.uuid] = pr
        for c in p.comments:
            comments[c.uuid] = c
    return ParsedGithubApi(
        self_identity=self_identity,
        pull_requests=list(prs.values()),
        comments=list(comments.values()),
    )
