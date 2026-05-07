from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any

from claude_mirror.config import (
    AnthropicExportDirSource,
    ChatGPTApiDirSource,
    Config,
)
from claude_mirror.dolt_service import DoltService
from claude_mirror.providers.anthropic.ingest import (
    AnthropicIngestStats,
    ingest_export_dir,
)
from claude_mirror.providers.openai.ingest import (
    OpenAIIngestStats,
    ingest_api_dir,
)
from claude_mirror.render import render_all


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


def ingest(config: Config) -> IngestSummary:
    # Project convention: ISO-8601 with explicit timezone offset (see AGENTS.md).
    started_at = datetime.now(timezone.utc).isoformat(timespec="seconds")
    summary = IngestSummary()

    with DoltService(config) as dolt:
        with dolt.connect() as conn:
            for src in config.enabled_sources:
                if isinstance(src, AnthropicExportDirSource):
                    _, stats = ingest_export_dir(
                        conn, src.path, started_at, source=src.provenance
                    )
                elif isinstance(src, ChatGPTApiDirSource):
                    _, stats = ingest_api_dir(
                        conn, src.path, started_at, source=src.provenance
                    )
                else:
                    raise NotImplementedError(f"unknown source: {src!r}")
                summary.sources.append(
                    SourceResult(
                        name=src.name,
                        provider=src.provider,
                        kind=src.kind,
                        stats=stats,
                    )
                )

        names = ",".join(s.name for s in summary.sources) or "<none>"
        summary.commit_hash = dolt.commit(f"ingest {names} {started_at}")

        with dolt.connect() as conn:
            r = render_all(conn, config.root)
            summary.rendered = r.rendered
            summary.rendered_orphans_removed = r.orphans_removed

    return summary
