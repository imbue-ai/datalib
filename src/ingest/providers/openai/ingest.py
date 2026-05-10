"""Parse one ChatGPT api dir into an in-memory `ParsedChatGPTApi`.

We no longer maintain provider-specific Dolt tables; the parsed dataclass
is the source of truth for both QMD rendering and grid_rows population.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from pathlib import Path

from ingest.providers.openai.parse import ParsedChatGPTApi, parse_api_dir

log = logging.getLogger(__name__)


@dataclass
class OpenAIIngestStats:
    accounts: int = 0
    projects: int = 0  # always 0 — kept for shape parity with anthropic stats
    conversations: int = 0
    messages: int = 0
    content_blocks: int = 0  # populated from content_parts; kept named for parity
    attachments: int = 0  # always 0 today (chatgpt API surfaces none)


def ingest_api_dir(
    api_dir: Path,
    source: str = "api",
) -> tuple[ParsedChatGPTApi, OpenAIIngestStats]:
    if source not in ("export", "api"):
        raise ValueError(f"source must be 'export' or 'api', got {source!r}")
    parsed = parse_api_dir(api_dir)
    log.info(
        "openai[%s]: parsed accounts=%d conversations=%d messages=%d content_parts=%d",
        source,
        len(parsed.accounts),
        len(parsed.conversations),
        len(parsed.messages),
        len(parsed.content_parts),
    )
    stats = OpenAIIngestStats(
        accounts=len(parsed.accounts),
        conversations=len(parsed.conversations),
        messages=len(parsed.messages),
        content_blocks=len(parsed.content_parts),
    )
    return parsed, stats


def merge_openai(items: list[ParsedChatGPTApi]) -> ParsedChatGPTApi:
    """Concatenate parsed ChatGPT inputs, last write wins by primary key.

    Today only one ChatGPT source is configured, so this is mostly a
    no-op; the function exists so the orchestration code stays uniform
    across providers."""
    if not items:
        return ParsedChatGPTApi()
    if len(items) == 1:
        return items[0]
    accounts: dict = {}
    convs: dict = {}
    msgs: dict = {}
    parts: dict = {}
    for p in items:
        for a in p.accounts:
            accounts[a.account_id] = a
        for c in p.conversations:
            convs[c.conversation_id] = c
        for m in p.messages:
            msgs[m.message_id] = m
        for cp in p.content_parts:
            parts[(cp.message_id, cp.part_index)] = cp
    return ParsedChatGPTApi(
        accounts=list(accounts.values()),
        conversations=list(convs.values()),
        messages=list(msgs.values()),
        content_parts=list(parts.values()),
    )
