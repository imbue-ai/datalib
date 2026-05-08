#!/usr/bin/env python3
"""Incrementally fetch Slack channels, messages, threads, and reactions to JSONL.

The Slack counterpart to download/{claude,chatgpt}_web.py. Where those write a
flat directory of full conversation JSON, this writes per-entity event streams
(channel / user / message / reply / reaction / self_identity), each split into
two JSONL files: `created/events.jsonl` (one line the first time we see a key)
and `updated/events.jsonl` (one line every time the raw payload changed —
including the first sighting). Tail `updated` to get the latest snapshot;
scan `created` to see when each entity first appeared.

Auth: assumes `latchkey curl` is configured for the `slack` service. Slack's
API accepts the latchkey-injected `Authorization: Bearer <token>` directly, so
we just shell out to `latchkey curl` per request — no curl_cffi / cookie
extraction needed (Slack doesn't run Cloudflare in front of api.slack.com).

Usage:
    uv run python -m download.slack_web                      # all member channels
    uv run python -m download.slack_web --channels general engineering
    uv run python -m download.slack_web --channels general --since 2024-06-01
    uv run python -m download.slack_web --refresh-window-days 0  # disable refresh pass
"""

from __future__ import annotations

import json
import logging
import subprocess
import sys
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any
from urllib.parse import urlencode

import typer
from tqdm import tqdm

DEFAULT_OUT_DIR = Path.home() / "backups" / "slack"
DEFAULT_SINCE = "2024-01-01"
DEFAULT_REFRESH_WINDOW_DAYS = 30
LATCHKEY_TIMEOUT = 60
RATE_LIMIT_MAX_RETRIES = 7
RATE_LIMIT_INITIAL_BACKOFF = 2.0
RATE_LIMIT_MAX_BACKOFF = 60.0

logger = logging.getLogger("slack_web")

# ---------------------------------------------------------------------------
# Auth + transport: latchkey curl with rate-limit + transient-error retries.
# ---------------------------------------------------------------------------

class SlackError(RuntimeError):
    """Raised when api.slack.com returns ok:false (excluding rate limits, which retry)."""


def _call_slack_once(method: str, params: dict[str, str] | None) -> dict[str, Any]:
    url = f"https://slack.com/api/{method}"
    if params:
        url = f"{url}?{urlencode(params)}"
    proc = subprocess.run(
        ["latchkey", "curl", url],
        capture_output=True, text=True, timeout=LATCHKEY_TIMEOUT, check=False,
    )
    if proc.returncode != 0:
        # curl exit codes 7/28/35/56 are transient; the caller retries.
        raise SlackError(f"latchkey curl {method} exit={proc.returncode} "
                         f"stderr={proc.stderr[-200:]!r}")
    try:
        data: dict[str, Any] = json.loads(proc.stdout)
    except json.JSONDecodeError as e:
        raise SlackError(f"{method}: invalid JSON: {proc.stdout[:200]!r}") from e
    if not data.get("ok"):
        err = data.get("error", "unknown")
        if err == "ratelimited":
            # signal to the retry loop without conflating with terminal errors
            raise _RateLimited(method)
        raise SlackError(f"{method}: ok=false error={err!r}")
    return data


class _RateLimited(Exception):
    pass


def call_slack(method: str, params: dict[str, str] | None = None) -> dict[str, Any]:
    """Call a Slack web method with exponential backoff on rate-limit / network blips."""
    backoff = RATE_LIMIT_INITIAL_BACKOFF
    for attempt in range(RATE_LIMIT_MAX_RETRIES + 1):
        try:
            return _call_slack_once(method, params)
        except _RateLimited:
            if attempt == RATE_LIMIT_MAX_RETRIES:
                raise SlackError(f"{method}: rate-limited after {attempt} retries")
            logger.warning("rate-limited on %s; sleeping %.0fs (attempt %d/%d)",
                           method, backoff, attempt + 1, RATE_LIMIT_MAX_RETRIES)
        except SlackError as e:
            # Retry only on transient curl exit codes (subset of network errors).
            if "exit=7" in str(e) or "exit=28" in str(e) or "exit=35" in str(e) \
                    or "exit=56" in str(e):
                if attempt == RATE_LIMIT_MAX_RETRIES:
                    raise
                logger.warning("transient on %s (%s); sleeping %.0fs",
                               method, e, backoff)
            else:
                raise
        time.sleep(backoff)
        backoff = min(backoff * 2, RATE_LIMIT_MAX_BACKOFF)
    raise AssertionError("unreachable")


def paginate(method: str, params: dict[str, str], response_key: str) -> list[dict]:
    """Walk Slack cursor pagination, returning all items concatenated."""
    items: list[dict] = []
    cursor: str | None = None
    while True:
        page_params = dict(params)
        if cursor:
            page_params["cursor"] = cursor
        data = call_slack(method, page_params)
        items.extend(data.get(response_key, []))
        if "has_more" in data and not data["has_more"]:
            break
        meta = data.get("response_metadata") or {}
        cursor = meta.get("next_cursor") or None
        if not cursor:
            break
    return items


# ---------------------------------------------------------------------------
# Event store: per-entity created/ + updated/ JSONL streams.
# ---------------------------------------------------------------------------

# Each entity type → (subdir name, key fields used to identify a record).
# The key fields are also pulled out of each record into top-level columns for
# easy grep'ing; the full Slack payload lives under `raw`.
ENTITY_CHANNEL = "channel"
ENTITY_USER = "user"
ENTITY_MESSAGE = "message"
ENTITY_REPLY = "reply"
ENTITY_REACTION = "reaction"
ENTITY_SELF_IDENTITY = "self_identity"


def _events_path(out_dir: Path, entity: str, stream: str) -> Path:
    return out_dir / entity / stream / "events.jsonl"


def _load_jsonl(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    out: list[dict[str, Any]] = []
    for line in path.read_text().splitlines():
        if line.strip():
            out.append(json.loads(line))
    return out


def _append_jsonl(path: Path, records: list[dict[str, Any]]) -> None:
    if not records:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a") as f:
        for r in records:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")


def _now_iso() -> str:
    return datetime.now().astimezone().isoformat()


def _make_record(entity: str, key: dict[str, Any], raw: dict[str, Any]) -> dict[str, Any]:
    return {"_recorded_at": _now_iso(), **key, "raw": raw}


def _diff_and_save(
    out_dir: Path,
    entity: str,
    fresh: list[dict[str, Any]],
    existing_by_key: dict[Any, dict[str, Any]],
    key_of: Any,
) -> tuple[int, int]:
    """Append new records to created/ and (new+changed) records to updated/.

    Returns (new_count, updated_count). Mirrors the reference's diff-save
    semantics: created/ is append-only "first-sighting" stream, updated/
    captures every change including first-sighting.
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
    _append_jsonl(_events_path(out_dir, entity, "created"), new_records)
    _append_jsonl(_events_path(out_dir, entity, "updated"), new_records + updated_records)
    if new_records:
        logger.info("  + %d new %s", len(new_records), entity)
    if updated_records:
        logger.info("  ~ %d updated %s", len(updated_records), entity)
    return len(new_records), len(updated_records)


def _load_latest_by_key(out_dir: Path, entity: str, key_of: Any) -> dict[Any, dict[str, Any]]:
    """Walk created/ then updated/ so updated/ entries shadow earlier ones."""
    latest: dict[Any, dict[str, Any]] = {}
    for stream in ("created", "updated"):
        for rec in _load_jsonl(_events_path(out_dir, entity, stream)):
            latest[key_of(rec)] = rec
    return latest


# ---------------------------------------------------------------------------
# Per-entity helpers (key extraction + record assembly).
# ---------------------------------------------------------------------------

def _key_channel(rec: dict) -> str:
    return rec["channel_id"]

def _key_user(rec: dict) -> str:
    return rec["user_id"]

def _key_message(rec: dict) -> tuple[str, str]:
    return (rec["channel_id"], rec["message_ts"])

def _key_reply(rec: dict) -> tuple[str, str, str]:
    return (rec["channel_id"], rec["thread_ts"], rec["reply_ts"])

def _key_reaction(rec: dict) -> tuple[str, str, str | None]:
    # A reaction snapshot is keyed by (channel, message_ts, thread_ts). The
    # thread_ts is part of the key because Slack returns "broadcast replies"
    # twice (once via conversations.history with thread_ts=null, once via
    # conversations.replies with thread_ts set) — each is a distinct viewpoint
    # of the same emoji counts and we want both cached.
    return (rec["channel_id"], rec["message_ts"], rec.get("thread_ts"))

def _key_self(rec: dict) -> str:
    return rec["user_id"]


# ---------------------------------------------------------------------------
# Fetch logic: channels, users, messages, replies, reactions.
# ---------------------------------------------------------------------------

def _fetch_self(out_dir: Path) -> dict[str, Any]:
    data = call_slack("auth.test")
    rec = _make_record(
        ENTITY_SELF_IDENTITY,
        {"user_id": data["user_id"], "user_name": data["user"]},
        data,
    )
    existing = _load_latest_by_key(out_dir, ENTITY_SELF_IDENTITY, _key_self)
    _diff_and_save(out_dir, ENTITY_SELF_IDENTITY, [rec], existing, _key_self)
    return rec


def _fetch_channels(out_dir: Path, members_only: bool) -> list[dict[str, Any]]:
    raw_channels = paginate(
        "conversations.list",
        {"exclude_archived": "true", "limit": "200",
         "types": "public_channel,private_channel"},
        "channels",
    )
    if members_only:
        raw_channels = [c for c in raw_channels if c.get("is_member")]
    records = [
        _make_record(ENTITY_CHANNEL,
                     {"channel_id": c["id"], "channel_name": c["name"]}, c)
        for c in raw_channels
    ]
    existing = _load_latest_by_key(out_dir, ENTITY_CHANNEL, _key_channel)
    _diff_and_save(out_dir, ENTITY_CHANNEL, records, existing, _key_channel)
    return records


def _fetch_users(out_dir: Path) -> list[dict[str, Any]]:
    raw_users = paginate("users.list", {"limit": "200"}, "members")
    records = [
        _make_record(ENTITY_USER, {"user_id": u["id"]}, u) for u in raw_users
    ]
    existing = _load_latest_by_key(out_dir, ENTITY_USER, _key_user)
    _diff_and_save(out_dir, ENTITY_USER, records, existing, _key_user)
    return records


def _datetime_to_slack_ts(dt: datetime) -> str:
    return f"{dt.timestamp():.6f}"


def _fetch_history(channel_id: str, oldest_ts: str, *, inclusive: bool,
                   latest_ts: str | None = None) -> list[dict]:
    params = {
        "channel": channel_id,
        "oldest": oldest_ts,
        "inclusive": "true" if inclusive else "false",
        "include_all_metadata": "true",
        "limit": "200",
    }
    if latest_ts is not None:
        params["latest"] = latest_ts
    return paginate("conversations.history", params, "messages")


def _fetch_replies(channel_id: str, thread_ts: str) -> list[dict]:
    return paginate(
        "conversations.replies",
        {"channel": channel_id, "ts": thread_ts, "limit": "200"},
        "messages",
    )


def _make_message_records(channel_id: str, channel_name: str,
                          raws: list[dict]) -> list[dict[str, Any]]:
    return [
        _make_record(
            ENTITY_MESSAGE,
            {"channel_id": channel_id, "channel_name": channel_name,
             "message_ts": raw["ts"]},
            raw,
        )
        for raw in raws if raw.get("ts")
    ]


def _make_reply_records(channel_id: str, channel_name: str, thread_ts: str,
                        raws: list[dict]) -> list[dict[str, Any]]:
    return [
        _make_record(
            ENTITY_REPLY,
            {"channel_id": channel_id, "channel_name": channel_name,
             "thread_ts": thread_ts, "reply_ts": raw["ts"]},
            raw,
        )
        for raw in raws if raw.get("ts") and raw["ts"] != thread_ts
    ]


def _extract_reactions(channel_id: str, channel_name: str,
                       message_records: list[dict[str, Any]],
                       thread_ts: str | None = None) -> list[dict[str, Any]]:
    """Pull inline reaction snapshots out of message/reply payloads (free)."""
    out: list[dict[str, Any]] = []
    for r in message_records:
        reactions = r["raw"].get("reactions")
        if not reactions:
            continue
        out.append(_make_record(
            ENTITY_REACTION,
            {"channel_id": channel_id, "channel_name": channel_name,
             "message_ts": r["raw"]["ts"], "thread_ts": thread_ts},
            {"reactions": reactions},
        ))
    return out


def _channel_latest_ts_map(
    existing_messages: dict[tuple[str, str], dict],
) -> dict[str, str]:
    """For each channel, find the most recent message_ts we already have."""
    latest: dict[str, str] = {}
    for (cid, ts) in existing_messages.keys():
        if cid not in latest or ts > latest[cid]:
            latest[cid] = ts
    return latest


def _latest_reply_ts_map(
    existing_replies: dict[tuple[str, str, str], dict],
) -> dict[tuple[str, str], str]:
    """For each (channel, thread), find the most recent reply_ts we already have."""
    latest: dict[tuple[str, str], str] = {}
    for (cid, thread_ts, reply_ts) in existing_replies.keys():
        key = (cid, thread_ts)
        if key not in latest or reply_ts > latest[key]:
            latest[key] = reply_ts
    return latest


def _export_channel(
    out_dir: Path,
    channel_id: str,
    channel_name: str,
    since_ts: str,
    refresh_window_days: int,
    existing_messages: dict[tuple[str, str], dict],
    existing_replies: dict[tuple[str, str, str], dict],
    existing_reactions: dict[tuple[str, str], dict],
    channel_latest_ts: str | None,
    latest_reply_by_thread: dict[tuple[str, str], str],
    now: datetime,
) -> tuple[int, int, int]:
    """Forward-fetch new messages, refresh-window pass, then replies + reactions.

    Returns (new_messages, new_replies, new_reactions).
    """
    # 1. Forward fetch from cursor (or `since_ts` if no prior data for channel).
    if channel_latest_ts:
        forward_oldest, inclusive = channel_latest_ts, False
        logger.info("  resuming from %s", forward_oldest)
    else:
        forward_oldest, inclusive = since_ts, True

    forward_raws = _fetch_history(channel_id, forward_oldest, inclusive=inclusive)
    forward_records = _make_message_records(channel_id, channel_name, forward_raws)

    # 2. Refresh window: re-fetch [now - N days, channel_latest_ts] so edits and
    #    new replies on already-exported parents are surfaced. Skipped on first
    #    run for this channel (forward fetch already covers the window) and when
    #    the user disables it via --refresh-window-days 0.
    refresh_records: list[dict[str, Any]] = []
    if refresh_window_days > 0 and channel_latest_ts:
        window_oldest_dt = now - timedelta(days=refresh_window_days)
        window_oldest_ts = _datetime_to_slack_ts(window_oldest_dt)
        if window_oldest_ts < channel_latest_ts:
            effective = max(window_oldest_ts, since_ts)
            logger.info("  refresh window [%s, %s]", effective, channel_latest_ts)
            refresh_raws = _fetch_history(
                channel_id, effective, inclusive=True, latest_ts=channel_latest_ts,
            )
            refresh_records = _make_message_records(channel_id, channel_name, refresh_raws)

    # 3. Persist messages (forward = always-new; refresh may produce updates).
    new_msgs, _ = _diff_and_save(
        out_dir, ENTITY_MESSAGE, forward_records + refresh_records,
        existing_messages, _key_message,
    )

    # 4. For each thread parent we just fetched, fetch missing replies.
    #    Skip threads where stored latest_reply >= API's latest_reply.
    seen_ts: set[str] = set()
    all_parents: list[dict[str, Any]] = []
    for r in forward_records + refresh_records:
        ts = r["raw"]["ts"]
        if ts in seen_ts:
            continue
        seen_ts.add(ts)
        if r["raw"].get("reply_count", 0) > 0:
            all_parents.append(r)

    new_reply_count = 0
    all_reply_records: list[dict[str, Any]] = []
    for parent in all_parents:
        thread_ts = parent["raw"]["ts"]
        api_latest = parent["raw"].get("latest_reply")
        stored_latest = latest_reply_by_thread.get((channel_id, thread_ts))
        if api_latest and stored_latest and stored_latest >= api_latest:
            continue
        raws = _fetch_replies(channel_id, thread_ts)
        reply_records = _make_reply_records(channel_id, channel_name, thread_ts, raws)
        all_reply_records.extend(reply_records)
        n_new, _ = _diff_and_save(
            out_dir, ENTITY_REPLY, reply_records, existing_replies, _key_reply,
        )
        new_reply_count += n_new

    # 5. Reactions: free extraction from messages and replies we already loaded.
    msg_reactions = _extract_reactions(channel_id, channel_name,
                                       forward_records + refresh_records, thread_ts=None)
    # group reply reactions per thread
    reply_reactions: list[dict[str, Any]] = []
    by_thread: dict[str, list[dict[str, Any]]] = {}
    for rr in all_reply_records:
        by_thread.setdefault(rr["thread_ts"], []).append(rr)
    for thread_ts, replies in by_thread.items():
        reply_reactions.extend(
            _extract_reactions(channel_id, channel_name, replies, thread_ts=thread_ts)
        )
    new_react, _ = _diff_and_save(
        out_dir, ENTITY_REACTION, msg_reactions + reply_reactions,
        existing_reactions, _key_reaction,
    )
    return new_msgs, new_reply_count, new_react


# ---------------------------------------------------------------------------
# Entry point: typer-driven CLI.
# ---------------------------------------------------------------------------

def fetch(
    out_dir: Path = typer.Option(
        DEFAULT_OUT_DIR, "--out-dir",
        help=f"Where to write the JSONL streams (default {DEFAULT_OUT_DIR}).",
    ),
    channels: list[str] = typer.Option(
        None, "--channels", "-c",
        help=("Channel names to export (e.g. 'general' or '#general'). May be "
              "passed multiple times. Default: all channels you are a member of."),
    ),
    since: str = typer.Option(
        DEFAULT_SINCE, "--since",
        help=("ISO date for how far back to look on first export of each "
              "channel (default 2024-01-01). Ignored once a channel has prior data."),
    ),
    refresh_window_days: int = typer.Option(
        DEFAULT_REFRESH_WINDOW_DAYS, "--refresh-window-days",
        help=("Re-fetch the last N days of history each run, so edits / new "
              "replies / new reactions on already-exported messages are noticed. "
              "Pass 0 to disable. Default: 30."),
    ),
    all_channels: bool = typer.Option(
        False, "--all/--members-only",
        help="Include channels you are not a member of (default: members-only).",
    ),
    verbose: bool = typer.Option(False, "--verbose", "-v", help="Debug logging."),
) -> None:
    """Incrementally export Slack to JSONL event streams."""
    logging.basicConfig(
        level=logging.DEBUG if verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    out_dir = out_dir.expanduser()
    out_dir.mkdir(parents=True, exist_ok=True)

    # Parse --since to a Slack ts (project convention: local tz with offset).
    since_dt = datetime.fromisoformat(since)
    if since_dt.tzinfo is None:
        since_dt = since_dt.replace(tzinfo=timezone.utc)
    since_ts = _datetime_to_slack_ts(since_dt)
    now = datetime.now(timezone.utc)

    # Load all existing state up front so per-channel diffs are O(1) lookups.
    existing_channels = _load_latest_by_key(out_dir, ENTITY_CHANNEL, _key_channel)
    existing_users = _load_latest_by_key(out_dir, ENTITY_USER, _key_user)
    existing_messages = _load_latest_by_key(out_dir, ENTITY_MESSAGE, _key_message)
    existing_replies = _load_latest_by_key(out_dir, ENTITY_REPLY, _key_reply)
    existing_reactions = _load_latest_by_key(out_dir, ENTITY_REACTION, _key_reaction)

    channel_latest_ts = _channel_latest_ts_map(existing_messages)
    latest_reply_by_thread = _latest_reply_ts_map(existing_replies)

    typer.echo(f"out: {out_dir}")
    typer.echo(f"existing: channels={len(existing_channels)} users={len(existing_users)} "
               f"messages={len(existing_messages)} replies={len(existing_replies)} "
               f"reactions={len(existing_reactions)}")

    # Self identity, channels, users (always re-fetched: cheap & monotonic).
    self_rec = _fetch_self(out_dir)
    typer.echo(f"auth: {self_rec['user_name']} ({self_rec['user_id']})")

    fresh_channels = _fetch_channels(out_dir, members_only=not all_channels)
    name_to_id = {c["channel_name"]: c["channel_id"] for c in fresh_channels}
    # Backfill from cache for channels we have on disk but didn't refetch.
    for prior in existing_channels.values():
        name_to_id.setdefault(prior["channel_name"], prior["channel_id"])

    _fetch_users(out_dir)

    # Resolve --channels (or all member channels) to (id, name) pairs.
    if channels:
        targets: list[tuple[str, str]] = []
        for spec in channels:
            name = spec.lstrip("#")
            cid = name_to_id.get(name)
            if cid is None:
                typer.echo(f"warning: channel '{name}' not found in listing — skipping",
                           err=True)
                continue
            targets.append((cid, name))
    else:
        targets = [(c["channel_id"], c["channel_name"]) for c in fresh_channels]

    typer.echo(f"exporting {len(targets)} channels")
    totals = {"messages": 0, "replies": 0, "reactions": 0}
    pbar = tqdm(targets, unit="ch")
    for cid, name in pbar:
        pbar.set_postfix_str(name[:30])
        try:
            n_msg, n_reply, n_react = _export_channel(
                out_dir=out_dir,
                channel_id=cid,
                channel_name=name,
                since_ts=since_ts,
                refresh_window_days=refresh_window_days,
                existing_messages=existing_messages,
                existing_replies=existing_replies,
                existing_reactions=existing_reactions,
                channel_latest_ts=channel_latest_ts.get(cid),
                latest_reply_by_thread=latest_reply_by_thread,
                now=now,
            )
        except SlackError as e:
            tqdm.write(f"  ! {name}: {e}")
            continue
        totals["messages"] += n_msg
        totals["replies"] += n_reply
        totals["reactions"] += n_react

    typer.echo(f"\nnew: {totals['messages']} messages  {totals['replies']} replies  "
               f"{totals['reactions']} reactions")


def main() -> None:
    typer.run(fetch)


if __name__ == "__main__":
    sys.exit(main() or 0)
