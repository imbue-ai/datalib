"""Parse one Anthropic export/api dir into an in-memory `ParsedExport`.

We no longer maintain provider-specific Dolt tables; the parsed dataclass
is the source of truth for both QMD rendering and grid_rows population.
A separate merge step in `ingest.ingest` collapses multiple parsed
results from the same provider before downstream consumers run.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from pathlib import Path

from ingest.providers.anthropic.parse import (
    AttachmentRow,
    ContentBlockRow,
    ParsedExport,
    parse_export,
)

log = logging.getLogger(__name__)


@dataclass
class AnthropicIngestStats:
    accounts: int = 0
    projects: int = 0
    conversations: int = 0
    messages: int = 0
    content_blocks: int = 0
    attachments: int = 0


def ingest_export_dir(
    export_dir: Path,
    source: str = "export",
) -> tuple[ParsedExport, AnthropicIngestStats]:
    """Parse an Anthropic export/api dir. `source` is one of
    {'export', 'api'} and is preserved on the returned tuple so the
    pipeline merge step can apply api-wins precedence."""
    if source not in ("export", "api"):
        raise ValueError(f"source must be 'export' or 'api', got {source!r}")
    parsed = parse_export(export_dir)
    log.info(
        "anthropic[%s]: parsed accounts=%d projects=%d conversations=%d "
        "messages=%d content_blocks=%d attachments=%d",
        source,
        len(parsed.accounts),
        len(parsed.projects),
        len(parsed.conversations),
        len(parsed.messages),
        len(parsed.content_blocks),
        len(parsed.attachments),
    )
    stats = AnthropicIngestStats(
        accounts=len(parsed.accounts),
        projects=len(parsed.projects),
        conversations=len(parsed.conversations),
        messages=len(parsed.messages),
        content_blocks=len(parsed.content_blocks),
        attachments=len(parsed.attachments),
    )
    return parsed, stats


def merge_anthropic(
    items: list[tuple[ParsedExport, str]],
) -> ParsedExport:
    """Collapse multiple parsed results into one, applying api-wins:

    - For every keyed entity (account, project, conversation, message),
      an "api"-sourced row beats an "export"-sourced row. Within the same
      precedence class, later in the list wins.
    - Content blocks and attachments are owned wholesale by the api
      source for any message_uuid the api ever provided. For messages
      not seen by api, blocks/attachments accumulate across export
      sources keyed by (message_uuid, index)."""
    if not items:
        return ParsedExport()

    api_msg_uuids: set[str] = set()
    for parsed, src in items:
        if src == "api":
            api_msg_uuids.update(m.message_uuid for m in parsed.messages)

    def _merge_keyed(field_name: str, key_fn) -> list:
        # key -> (row, was_api_sourced)
        out: dict = {}
        for parsed, src in items:
            for row in getattr(parsed, field_name):
                k = key_fn(row)
                prev = out.get(k)
                if src == "api":
                    out[k] = (row, True)
                elif prev is None or not prev[1]:
                    out[k] = (row, False)
        return [v[0] for v in out.values()]

    accounts = _merge_keyed("accounts", lambda r: r.account_uuid)
    projects = _merge_keyed("projects", lambda r: (r.account_uuid, r.project_uuid))
    conversations = _merge_keyed("conversations", lambda r: r.conversation_uuid)
    messages = _merge_keyed("messages", lambda r: r.message_uuid)

    blocks_by_msg: dict[str, dict[int, ContentBlockRow]] = {}
    atts_by_msg: dict[str, dict[int, AttachmentRow]] = {}
    api_owned: set[str] = set()
    for parsed, src in items:
        if src == "api":
            this_msgs = {m.message_uuid for m in parsed.messages}
            # Wipe slots for newly-api-owned messages so this api ingest's
            # block list fully replaces any earlier export blocks (mirrors
            # the old DELETE+INSERT under api).
            for muuid in this_msgs:
                if muuid not in api_owned:
                    blocks_by_msg[muuid] = {}
                    atts_by_msg[muuid] = {}
                    api_owned.add(muuid)
            for b in parsed.content_blocks:
                if b.message_uuid in this_msgs:
                    blocks_by_msg.setdefault(b.message_uuid, {})[b.block_index] = b
            for a in parsed.attachments:
                if a.message_uuid in this_msgs:
                    atts_by_msg.setdefault(a.message_uuid, {})[a.attachment_index] = a
        else:
            for b in parsed.content_blocks:
                if b.message_uuid in api_owned:
                    continue
                blocks_by_msg.setdefault(b.message_uuid, {})[b.block_index] = b
            for a in parsed.attachments:
                if a.message_uuid in api_owned:
                    continue
                atts_by_msg.setdefault(a.message_uuid, {})[a.attachment_index] = a

    # Drop blocks/attachments for messages no longer in the merged set
    # (an export message clobbered by an api ingest that omitted it).
    msg_uuids = {m.message_uuid for m in messages}
    blocks = [
        b
        for muuid, bd in blocks_by_msg.items()
        if muuid in msg_uuids
        for b in sorted(bd.values(), key=lambda x: x.block_index)
    ]
    atts = [
        a
        for muuid, ad in atts_by_msg.items()
        if muuid in msg_uuids
        for a in sorted(ad.values(), key=lambda x: x.attachment_index)
    ]
    # Mark unused arg to satisfy lints (kept for API parity / future use).
    del api_msg_uuids

    return ParsedExport(
        accounts=accounts,
        projects=projects,
        conversations=conversations,
        messages=messages,
        content_blocks=blocks,
        attachments=atts,
    )
