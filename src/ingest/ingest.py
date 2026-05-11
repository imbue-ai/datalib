from __future__ import annotations

import logging
import time
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any

from ingest.config import (
    AnthropicExportDirSource,
    ChatGPTApiDirSource,
    Config,
    GithubApiDirSource,
    GitlabApiDirSource,
    NotionWebDirSource,
    SlackApiDirSource,
)
from ingest.dolt_service import DoltService
from ingest.grid_rows import populate_grid_rows
from ingest.providers.anthropic.ingest import (
    ingest_export_dir,
    merge_anthropic,
)
from ingest.providers.anthropic.parse import ParsedExport
from ingest.providers.github.ingest import (
    ingest_api_dir as ingest_github_api_dir,
)
from ingest.providers.github.ingest import (
    merge_github,
)
from ingest.providers.github.parse import ParsedGithubApi
from ingest.providers.gitlab.ingest import (
    ingest_api_dir as ingest_gitlab_api_dir,
)
from ingest.providers.gitlab.ingest import (
    merge_gitlab,
)
from ingest.providers.gitlab.parse import ParsedGitlabApi
from ingest.providers.notion.ingest import (
    ingest_web_dir as ingest_notion_web_dir,
)
from ingest.providers.notion.ingest import (
    merge_notion,
)
from ingest.providers.notion.parse import ParsedNotionWeb
from ingest.providers.openai.ingest import (
    ingest_api_dir,
    merge_openai,
)
from ingest.providers.openai.parse import ParsedChatGPTApi
from ingest.providers.slack.ingest import (
    ingest_api_dir as ingest_slack_api_dir,
)
from ingest.providers.slack.ingest import (
    merge_slack,
)
from ingest.providers.slack.parse import ParsedSlackApi
from ingest.render import (
    render_anthropic,
    render_github,
    render_gitlab,
    render_notion,
    render_openai,
    render_slack,
    write_accounts_json,
)

log = logging.getLogger(__name__)


@dataclass
class SourceResult:
    name: str
    provider: str
    kind: str
    stats: Any  # AnthropicIngestStats | OpenAIIngestStats | SlackIngestStats


@dataclass
class IngestSummary:
    sources: list[SourceResult] = field(default_factory=list)
    commit_hash: str | None = None
    rendered: int = 0
    rendered_orphans_removed: int = 0
    grid_rows: int = 0


def ingest(config: Config, now: str | None = None) -> IngestSummary:
    # Project convention: ISO-8601 with explicit timezone offset, in *local*
    # time so the offset preserves the human-meaningful "wall clock" of when
    # the ingest happened (see AGENTS.md).
    started_at = now or datetime.now().astimezone().isoformat(timespec="seconds")
    summary = IngestSummary()

    anthropic_inputs: list[tuple[ParsedExport, str]] = []
    openai_inputs: list[ParsedChatGPTApi] = []
    slack_inputs: list[ParsedSlackApi] = []
    slack_media_dirs: list = []
    github_inputs: list[ParsedGithubApi] = []
    gitlab_inputs: list[ParsedGitlabApi] = []
    notion_inputs: list[ParsedNotionWeb] = []

    log.info("ingest start: %d enabled source(s)", len(config.enabled_sources))
    for src in config.enabled_sources:
        log.info(
            "[%s] %s/%s: parsing from %s",
            src.name,
            src.provider,
            src.kind,
            src.path,
        )
        t0 = time.monotonic()
        if isinstance(src, AnthropicExportDirSource):
            parsed_a, stats = ingest_export_dir(src.path, source=src.provenance)
            anthropic_inputs.append((parsed_a, src.provenance))
        elif isinstance(src, ChatGPTApiDirSource):
            parsed_o, stats = ingest_api_dir(src.path, source=src.provenance)
            openai_inputs.append(parsed_o)
        elif isinstance(src, SlackApiDirSource):
            parsed_s, stats = ingest_slack_api_dir(src.path)
            slack_inputs.append(parsed_s)
            media = src.path / "media"
            if media.is_dir():
                slack_media_dirs.append(media)
        elif isinstance(src, GithubApiDirSource):
            parsed_gh, stats = ingest_github_api_dir(src.path)
            github_inputs.append(parsed_gh)
        elif isinstance(src, GitlabApiDirSource):
            parsed_gl, stats = ingest_gitlab_api_dir(src.path)
            gitlab_inputs.append(parsed_gl)
        elif isinstance(src, NotionWebDirSource):
            parsed_n, stats = ingest_notion_web_dir(src.path)
            notion_inputs.append(parsed_n)
        else:
            raise NotImplementedError(f"unknown source: {src!r}")
        log.info("[%s] parsed in %.1fs", src.name, time.monotonic() - t0)
        summary.sources.append(
            SourceResult(
                name=src.name,
                provider=src.provider,
                kind=src.kind,
                stats=stats,
            )
        )

    anthropic = merge_anthropic(anthropic_inputs) if anthropic_inputs else None
    openai = merge_openai(openai_inputs) if openai_inputs else None
    slack = merge_slack(slack_inputs) if slack_inputs else None
    github = merge_github(github_inputs) if github_inputs else None
    gitlab = merge_gitlab(gitlab_inputs) if gitlab_inputs else None
    notion = merge_notion(notion_inputs) if notion_inputs else None

    # Render QMDs and accounts.json directly from parsed data — no SQL.
    if anthropic is not None:
        r = render_anthropic(anthropic, config.root)
        summary.rendered += r.rendered
        summary.rendered_orphans_removed += r.orphans_removed
    if openai is not None:
        r = render_openai(openai, config.root)
        summary.rendered += r.rendered
        summary.rendered_orphans_removed += r.orphans_removed
    if slack is not None:
        r = render_slack(slack, config.root, media_dirs=slack_media_dirs)
        summary.rendered += r.rendered
        summary.rendered_orphans_removed += r.orphans_removed
    if github is not None:
        r = render_github(github, config.root)
        summary.rendered += r.rendered
        summary.rendered_orphans_removed += r.orphans_removed
    if gitlab is not None:
        r = render_gitlab(gitlab, config.root)
        summary.rendered += r.rendered
        summary.rendered_orphans_removed += r.orphans_removed
    if notion is not None:
        r = render_notion(notion, config.root)
        summary.rendered += r.rendered
        summary.rendered_orphans_removed += r.orphans_removed
    write_accounts_json(anthropic, openai, config.root)

    # Write grid_rows to Dolt — the only structured table that survives.
    with DoltService(config) as dolt:
        with dolt.connect() as conn:
            conn.autocommit(False)
            log.info("populating grid_rows")
            t0 = time.monotonic()
            summary.grid_rows = populate_grid_rows(
                conn, anthropic, openai, slack, github, gitlab, notion
            )
            conn.commit()
            log.info(
                "grid_rows: %d rows in %.1fs",
                summary.grid_rows,
                time.monotonic() - t0,
            )

        names = ",".join(s.name for s in summary.sources) or "<none>"
        summary.commit_hash = dolt.commit(f"ingest {names} {started_at}")

    return summary
