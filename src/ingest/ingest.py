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
    SlackApiDirSource,
)
from ingest.dolt_service import DoltService
from ingest.grid_rows import populate_grid_rows
from ingest.providers.anthropic.ingest import (
    ingest_export_dir,
)
from ingest.providers.openai.ingest import (
    ingest_api_dir,
)
from ingest.providers.slack.ingest import (
    ingest_api_dir as ingest_slack_api_dir,
)
from ingest.render import render_all

log = logging.getLogger(__name__)


@dataclass
class SourceResult:
    name: str
    provider: str
    kind: str
    stats: Any  # AnthropicIngestStats | OpenAIIngestStats


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
    #
    # `now` may be passed explicitly to make the run deterministic (used by
    # the Bazel fixture genrule and by tests). Production runs leave it None
    # and pick up wall-clock time.
    started_at = now or datetime.now().astimezone().isoformat(timespec="seconds")
    summary = IngestSummary()

    with DoltService(config) as dolt:
        with dolt.connect() as conn:
            # Batch each source's writes into one transaction. Per-row
            # autocommit against Dolt's SQL server adds ~8ms/row of
            # round-trip + flush; for ~5k rows that's tens of seconds we
            # don't need to spend.
            conn.autocommit(False)
            log.info("ingest start: %d enabled source(s)", len(config.enabled_sources))
            for src in config.enabled_sources:
                log.info(
                    "[%s] %s/%s: ingesting from %s",
                    src.name,
                    src.provider,
                    src.kind,
                    src.path,
                )
                t0 = time.monotonic()
                if isinstance(src, AnthropicExportDirSource):
                    _, stats = ingest_export_dir(
                        conn, src.path, started_at, source=src.provenance
                    )
                elif isinstance(src, ChatGPTApiDirSource):
                    _, stats = ingest_api_dir(
                        conn, src.path, started_at, source=src.provenance
                    )
                elif isinstance(src, SlackApiDirSource):
                    _, stats = ingest_slack_api_dir(conn, src.path, started_at)
                else:
                    raise NotImplementedError(f"unknown source: {src!r}")
                conn.commit()
                log.info("[%s] done in %.1fs", src.name, time.monotonic() - t0)
                summary.sources.append(
                    SourceResult(
                        name=src.name,
                        provider=src.provider,
                        kind=src.kind,
                        stats=stats,
                    )
                )

            log.info("populating grid_rows")
            t0 = time.monotonic()
            summary.grid_rows = populate_grid_rows(conn)
            conn.commit()
            log.info(
                "grid_rows: %d rows in %.1fs",
                summary.grid_rows,
                time.monotonic() - t0,
            )

        names = ",".join(s.name for s in summary.sources) or "<none>"
        summary.commit_hash = dolt.commit(f"ingest {names} {started_at}")

        with dolt.connect() as conn:
            r = render_all(conn, config.root)
            summary.rendered = r.rendered
            summary.rendered_orphans_removed = r.orphans_removed

    return summary
