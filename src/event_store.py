"""Per-entity append-only event store shared by the provider downloaders.

Layout:
    <out_dir>/<entity>/<stream>/events.jsonl

where `stream` is `created` (append-only first-sightings) or `updated`
(every first-sighting plus every subsequent change). Tail `updated` to
get the latest snapshot; scan `created` for first-seen timestamps.

This module collects the helpers that github/gitlab/slack downloaders
all need: path layout, append, recorded-at stamp, key-keyed diff, and
loading the latest-by-key snapshot. Centralizing them keeps the three
downloaders trivially comparable when the shape evolves.
"""

from __future__ import annotations

import json
import logging
from datetime import datetime
from pathlib import Path
from typing import Any, Callable

from jsonl_io import load_jsonl

logger = logging.getLogger(__name__)


def events_path(out_dir: Path, entity: str, stream: str) -> Path:
    return out_dir / entity / stream / "events.jsonl"


def append_jsonl(path: Path, records: list[dict[str, Any]]) -> None:
    if not records:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a") as f:
        for r in records:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")


def now_iso() -> str:
    return datetime.now().astimezone().isoformat()


def make_record(key: dict[str, Any], raw: dict[str, Any]) -> dict[str, Any]:
    """Wrap a provider payload with its denormalized key + a recorded-at
    stamp. Top-level key fields are spread alongside `raw` so the JSONL
    is greppable without jq."""
    return {"_recorded_at": now_iso(), **key, "raw": raw}


def diff_and_save(
    out_dir: Path,
    entity: str,
    fresh: list[dict[str, Any]],
    existing_by_key: dict[Any, dict[str, Any]],
    key_of: Callable[[dict[str, Any]], Any],
) -> tuple[int, int]:
    """Append new records to created/ and (new+changed) records to updated/.

    Returns (new_count, updated_count). `created/` is the first-sighting
    stream; `updated/` carries first-sightings plus every subsequent
    change so tailing it yields the latest snapshot per key.
    """
    new_records: list[dict[str, Any]] = []
    updated_records: list[dict[str, Any]] = []
    for rec in fresh:
        k = key_of(rec)
        prior = existing_by_key.get(k)
        if prior is None:
            new_records.append(rec)
        elif prior.get("raw") != rec.get("raw"):
            updated_records.append(rec)
    append_jsonl(events_path(out_dir, entity, "created"), new_records)
    append_jsonl(events_path(out_dir, entity, "updated"), new_records + updated_records)
    if new_records:
        logger.info("  + %d new %s", len(new_records), entity)
    if updated_records:
        logger.info("  ~ %d updated %s", len(updated_records), entity)
    return len(new_records), len(updated_records)


def load_latest_by_key(
    out_dir: Path,
    entity: str,
    key_of: Callable[[dict[str, Any]], Any],
) -> dict[Any, dict[str, Any]]:
    """Walk created/ then updated/ so updated/ entries shadow earlier ones."""
    latest: dict[Any, dict[str, Any]] = {}
    for stream in ("created", "updated"):
        for rec in load_jsonl(events_path(out_dir, entity, stream)):
            latest[key_of(rec)] = rec
    return latest
