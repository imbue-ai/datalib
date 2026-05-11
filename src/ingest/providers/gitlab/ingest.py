"""Parse one GitLab api dir into an in-memory `ParsedGitlabApi`."""

from __future__ import annotations

import logging
from dataclasses import dataclass
from pathlib import Path

from ingest.providers.gitlab.parse import ParsedGitlabApi, parse_api_dir

log = logging.getLogger(__name__)


@dataclass
class GitlabIngestStats:
    merge_requests: int = 0
    notes: int = 0


def ingest_api_dir(api_dir: Path) -> tuple[ParsedGitlabApi, GitlabIngestStats]:
    parsed = parse_api_dir(api_dir)
    log.info(
        "gitlab: parsed merge_requests=%d notes=%d",
        len(parsed.merge_requests),
        len(parsed.notes),
    )
    stats = GitlabIngestStats(
        merge_requests=len(parsed.merge_requests),
        notes=len(parsed.notes),
    )
    return parsed, stats


def merge_gitlab(items: list[ParsedGitlabApi]) -> ParsedGitlabApi:
    if not items:
        return ParsedGitlabApi()
    if len(items) == 1:
        return items[0]
    mrs: dict = {}
    notes: dict = {}
    self_identity = None
    for p in items:
        if p.self_identity is not None:
            self_identity = p.self_identity
        for mr in p.merge_requests:
            mrs[mr.uuid] = mr
        for n in p.notes:
            notes[n.uuid] = n
    return ParsedGitlabApi(
        self_identity=self_identity,
        merge_requests=list(mrs.values()),
        notes=list(notes.values()),
    )
