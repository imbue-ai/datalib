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
import re
import subprocess
import sys
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any
from urllib.parse import urlencode

import typer
from tqdm import tqdm

from event_store import (
    diff_and_save as _diff_and_save,
    load_latest_by_key as _load_latest_by_key,
    make_record as _make_record,
)

DEFAULT_OUT_DIR = Path.home() / "backups" / "slack"
DEFAULT_SINCE = "2024-01-01"
DEFAULT_REFRESH_WINDOW_DAYS = 30
LATCHKEY_TIMEOUT = 60
LATCHKEY_FILE_TIMEOUT = 600  # large attachments can take a while
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
        capture_output=True,
        text=True,
        timeout=LATCHKEY_TIMEOUT,
        check=False,
    )
    if proc.returncode != 0:
        # curl exit codes 7/28/35/56 are transient; the caller retries.
        raise SlackError(
            f"latchkey curl {method} exit={proc.returncode} "
            f"stderr={proc.stderr[-200:]!r}"
        )
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
            logger.warning(
                "rate-limited on %s; sleeping %.0fs (attempt %d/%d)",
                method,
                backoff,
                attempt + 1,
                RATE_LIMIT_MAX_RETRIES,
            )
        except SlackError as e:
            # Retry only on transient curl exit codes (subset of network errors).
            if (
                "exit=7" in str(e)
                or "exit=28" in str(e)
                or "exit=35" in str(e)
                or "exit=56" in str(e)
            ):
                if attempt == RATE_LIMIT_MAX_RETRIES:
                    raise
                logger.warning(
                    "transient on %s (%s); sleeping %.0fs", method, e, backoff
                )
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
# Media download: walk `files` arrays in messages/replies, fetch via latchkey.
# ---------------------------------------------------------------------------


def _extract_file_auth() -> dict[str, str]:
    """Pull Authorization/Cookie/User-Agent out of `latchkey curl -v` stderr.

    Slack's `files.slack.com` host is not in latchkey's URL-pattern allowlist
    for the `slack` service ("No service matches URL"), so we cannot just
    `latchkey curl <file_url>`. Instead, harvest the Bearer + d-cookie that
    latchkey injects on a known-good slack.com call and replay them ourselves
    with plain curl. The d-cookie is required by some workspaces; the Bearer
    alone is enough for most.
    """
    proc = subprocess.run(
        [
            "latchkey",
            "curl",
            "-v",
            "-o",
            "/dev/null",
            "-s",
            "https://slack.com/api/auth.test",
        ],
        capture_output=True,
        text=True,
        timeout=LATCHKEY_TIMEOUT,
        check=False,
    )
    headers: dict[str, str] = {}
    for name in ("Authorization", "Cookie", "User-Agent"):
        m = re.search(rf"^> {name}: (.+)$", proc.stderr, flags=re.MULTILINE)
        if m:
            headers[name] = m.group(1).strip()
    if "Authorization" not in headers:
        raise SlackError(
            "could not extract Authorization header from `latchkey curl -v`. "
            "Is the `slack` service registered with `latchkey auth set`?"
        )
    return headers


def _safe_filename(name: str | None, fallback: str) -> str:
    """Sanitize a Slack-supplied filename: keep it filesystem-safe, drop slashes,
    fall back to the file id if Slack didn't give us a name."""
    if not name:
        return fallback
    cleaned = "".join(c if c.isalnum() or c in "-._ " else "_" for c in name).strip()
    return cleaned or fallback


def _download_one_file(
    file_obj: dict[str, Any],
    media_dir: Path,
    headers: dict[str, str],
) -> str:
    """Download a single Slack file to media/<file_id>/<filename>.

    Returns: 'skipped' (already on disk), 'tombstone' (deleted/no URL),
    'external' (hosted elsewhere — can't auth), 'downloaded', or 'error'.
    """
    file_id = file_obj.get("id")
    if not file_id:
        return "tombstone"
    if file_obj.get("mode") == "tombstone":
        return "tombstone"
    # External files (Google Drive etc.) point url_private at a third-party
    # host that doesn't accept our Slack Bearer — skip them rather than 401.
    if file_obj.get("is_external"):
        return "external"
    url = file_obj.get("url_private_download") or file_obj.get("url_private")
    if not url:
        return "external"

    name = _safe_filename(file_obj.get("name"), fallback=file_id)
    target_dir = media_dir / file_id
    target = target_dir / name
    if target.exists() and target.stat().st_size > 0:
        return "skipped"
    target_dir.mkdir(parents=True, exist_ok=True)

    # files.slack.com isn't in latchkey's `slack` URL allowlist, so we replay
    # the headers latchkey injects (Bearer + d-cookie) using plain curl. -L
    # follows the redirect to the signed S3 URL; -f turns HTTP errors into a
    # non-zero exit instead of writing the error body to disk.
    args = ["curl", "-fSL", "-o", str(target)]
    for k, v in headers.items():
        args += ["-H", f"{k}: {v}"]
    args.append(url)
    proc = subprocess.run(
        args,
        capture_output=True,
        text=True,
        timeout=LATCHKEY_FILE_TIMEOUT,
        check=False,
    )
    if proc.returncode != 0:
        target.unlink(missing_ok=True)
        logger.warning(
            "media: %s (%s) failed exit=%d %s",
            file_id,
            name,
            proc.returncode,
            proc.stderr[-200:].strip(),
        )
        return "error"
    return "downloaded"


def _download_files_for_records(
    records: list[dict[str, Any]],
    media_dir: Path,
    headers: dict[str, str],
) -> dict[str, int]:
    """Walk each record's raw['files'] array and download each file."""
    counts = {"downloaded": 0, "skipped": 0, "tombstone": 0, "external": 0, "error": 0}
    targets: list[dict[str, Any]] = []
    for rec in records:
        for f in rec["raw"].get("files") or []:
            targets.append(f)
    if not targets:
        return counts
    for f in targets:
        outcome = _download_one_file(f, media_dir, headers)
        counts[outcome] = counts.get(outcome, 0) + 1
    return counts


# ---------------------------------------------------------------------------
# Fetch logic: channels, users, messages, replies, reactions.
# ---------------------------------------------------------------------------


def _fetch_self(out_dir: Path) -> dict[str, Any]:
    data = call_slack("auth.test")
    rec = _make_record(
        {"user_id": data["user_id"], "user_name": data["user"]},
        data,
    )
    existing = _load_latest_by_key(out_dir, ENTITY_SELF_IDENTITY, _key_self)
    _diff_and_save(out_dir, ENTITY_SELF_IDENTITY, [rec], existing, _key_self)
    return rec


def _fetch_channels(
    out_dir: Path, members_only: bool, include_archived: bool = False
) -> list[dict[str, Any]]:
    raw_channels = paginate(
        "conversations.list",
        {
            "exclude_archived": "false" if include_archived else "true",
            "limit": "200",
            "types": "public_channel,private_channel",
        },
        "channels",
    )
    if members_only:
        raw_channels = [c for c in raw_channels if c.get("is_member")]
    records = [
        _make_record({"channel_id": c["id"], "channel_name": c["name"]}, c)
        for c in raw_channels
    ]
    existing = _load_latest_by_key(out_dir, ENTITY_CHANNEL, _key_channel)
    _diff_and_save(out_dir, ENTITY_CHANNEL, records, existing, _key_channel)
    return records


def _fetch_users(out_dir: Path) -> list[dict[str, Any]]:
    raw_users = paginate("users.list", {"limit": "200"}, "members")
    records = [_make_record({"user_id": u["id"]}, u) for u in raw_users]
    existing = _load_latest_by_key(out_dir, ENTITY_USER, _key_user)
    _diff_and_save(out_dir, ENTITY_USER, records, existing, _key_user)
    return records


def _datetime_to_slack_ts(dt: datetime) -> str:
    return f"{dt.timestamp():.6f}"


def _fetch_history(
    channel_id: str, oldest_ts: str, *, inclusive: bool, latest_ts: str | None = None
) -> list[dict]:
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


def _make_message_records(
    channel_id: str, channel_name: str, raws: list[dict]
) -> list[dict[str, Any]]:
    return [
        _make_record(
            {
                "channel_id": channel_id,
                "channel_name": channel_name,
                "message_ts": raw["ts"],
            },
            raw,
        )
        for raw in raws
        if raw.get("ts")
    ]


def _make_reply_records(
    channel_id: str, channel_name: str, thread_ts: str, raws: list[dict]
) -> list[dict[str, Any]]:
    return [
        _make_record(
            {
                "channel_id": channel_id,
                "channel_name": channel_name,
                "thread_ts": thread_ts,
                "reply_ts": raw["ts"],
            },
            raw,
        )
        for raw in raws
        if raw.get("ts") and raw["ts"] != thread_ts
    ]


def _extract_reactions(
    channel_id: str,
    channel_name: str,
    message_records: list[dict[str, Any]],
    thread_ts: str | None = None,
) -> list[dict[str, Any]]:
    """Pull inline reaction snapshots out of message/reply payloads (free)."""
    out: list[dict[str, Any]] = []
    for r in message_records:
        reactions = r["raw"].get("reactions")
        if not reactions:
            continue
        out.append(
            _make_record(
                {
                    "channel_id": channel_id,
                    "channel_name": channel_name,
                    "message_ts": r["raw"]["ts"],
                    "thread_ts": thread_ts,
                },
                {"reactions": reactions},
            )
        )
    return out


def _channel_latest_ts_map(
    existing_messages: dict[tuple[str, str], dict],
) -> dict[str, str]:
    """For each channel, find the most recent message_ts we already have."""
    latest: dict[str, str] = {}
    for cid, ts in existing_messages.keys():
        if cid not in latest or ts > latest[cid]:
            latest[cid] = ts
    return latest


def _latest_reply_ts_map(
    existing_replies: dict[tuple[str, str, str], dict],
) -> dict[tuple[str, str], str]:
    """For each (channel, thread), find the most recent reply_ts we already have."""
    latest: dict[tuple[str, str], str] = {}
    for cid, thread_ts, reply_ts in existing_replies.keys():
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
    media_dir: Path | None,
    media_headers: dict[str, str] | None,
) -> tuple[int, int, int, dict[str, int]]:
    """Forward-fetch new messages, refresh-window pass, then replies + reactions.

    When `media_dir` is set, also walk `files` on each message/reply and
    download any not yet on disk.

    Returns (new_messages, new_replies, new_reactions, media_counts).
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
                channel_id,
                effective,
                inclusive=True,
                latest_ts=channel_latest_ts,
            )
            refresh_records = _make_message_records(
                channel_id, channel_name, refresh_raws
            )

    # 3. Persist messages (forward = always-new; refresh may produce updates).
    new_msgs, _ = _diff_and_save(
        out_dir,
        ENTITY_MESSAGE,
        forward_records + refresh_records,
        existing_messages,
        _key_message,
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
            out_dir,
            ENTITY_REPLY,
            reply_records,
            existing_replies,
            _key_reply,
        )
        new_reply_count += n_new

    # 5. Reactions: free extraction from messages and replies we already loaded.
    msg_reactions = _extract_reactions(
        channel_id, channel_name, forward_records + refresh_records, thread_ts=None
    )
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
        out_dir,
        ENTITY_REACTION,
        msg_reactions + reply_reactions,
        existing_reactions,
        _key_reaction,
    )

    media_counts: dict[str, int] = {}
    if media_dir is not None:
        assert media_headers is not None
        media_counts = _download_files_for_records(
            forward_records + refresh_records + all_reply_records,
            media_dir,
            media_headers,
        )
        if any(v for k, v in media_counts.items() if k != "skipped"):
            logger.info(
                "  media: %s",
                " ".join(f"{k}={v}" for k, v in media_counts.items() if v),
            )
    return new_msgs, new_reply_count, new_react, media_counts


# ---------------------------------------------------------------------------
# Entry point: typer-driven CLI.
# ---------------------------------------------------------------------------


def fetch(
    out_dir: Path = typer.Option(
        DEFAULT_OUT_DIR,
        "--out-dir",
        help=f"Where to write the JSONL streams (default {DEFAULT_OUT_DIR}).",
    ),
    channels: list[str] = typer.Option(
        None,
        "--channels",
        "-c",
        help=(
            "Channel names to export (e.g. 'general' or '#general'). May be "
            "passed multiple times. Default: all channels you are a member of."
        ),
    ),
    since: str = typer.Option(
        DEFAULT_SINCE,
        "--since",
        help=(
            "ISO date for how far back to look on first export of each "
            "channel (default 2024-01-01). Ignored once a channel has prior data."
        ),
    ),
    refresh_window_days: int = typer.Option(
        DEFAULT_REFRESH_WINDOW_DAYS,
        "--refresh-window-days",
        help=(
            "Re-fetch the last N days of history each run, so edits / new "
            "replies / new reactions on already-exported messages are noticed. "
            "Pass 0 to disable. Default: 30."
        ),
    ),
    all_channels: bool = typer.Option(
        False,
        "--all/--members-only",
        help="Include channels you are not a member of (default: members-only).",
    ),
    media: bool = typer.Option(
        True,
        "--media/--no-media",
        help=(
            "Download attached files (images, PDFs, etc.) to "
            "<out-dir>/media/<file_id>/<filename>. Tombstones and externally-"
            "hosted files are skipped. Default: on."
        ),
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
    typer.echo(
        f"existing: channels={len(existing_channels)} users={len(existing_users)} "
        f"messages={len(existing_messages)} replies={len(existing_replies)} "
        f"reactions={len(existing_reactions)}"
    )

    # Self identity, channels, users (always re-fetched: cheap & monotonic).
    self_rec = _fetch_self(out_dir)
    typer.echo(f"auth: {self_rec['user_name']} ({self_rec['user_id']})")

    fresh_channels = _fetch_channels(
        out_dir,
        members_only=not all_channels,
        include_archived=bool(channels),
    )
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
                typer.echo(
                    f"warning: channel '{name}' not found in listing — skipping",
                    err=True,
                )
                continue
            targets.append((cid, name))
    else:
        targets = [(c["channel_id"], c["channel_name"]) for c in fresh_channels]

    media_dir = (out_dir / "media") if media else None
    media_headers: dict[str, str] | None = None
    if media:
        media_headers = _extract_file_auth()
    typer.echo(f"exporting {len(targets)} channels (media: {'on' if media else 'off'})")
    totals = {"messages": 0, "replies": 0, "reactions": 0}
    media_totals: dict[str, int] = {}
    pbar = tqdm(targets, unit="ch")
    for cid, name in pbar:
        pbar.set_postfix_str(name[:30])
        try:
            n_msg, n_reply, n_react, m_counts = _export_channel(
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
                media_dir=media_dir,
                media_headers=media_headers,
            )
        except SlackError as e:
            tqdm.write(f"  ! {name}: {e}")
            continue
        totals["messages"] += n_msg
        totals["replies"] += n_reply
        totals["reactions"] += n_react
        for k, v in m_counts.items():
            media_totals[k] = media_totals.get(k, 0) + v

    # Self-healing media sweep: walk every cached message/reply for each
    # target channel and (re)download any file not on disk. Idempotent — files
    # already on disk return 'skipped'. This catches files that errored on a
    # prior run (e.g. before the auth fix) without re-fetching the events.
    if media_dir is not None and media_headers is not None:
        target_cids = {cid for cid, _ in targets}
        cached: list[dict[str, Any]] = []
        for (cid, _ts), rec in existing_messages.items():
            if cid in target_cids:
                cached.append(rec)
        for (cid, _tts, _rts), rec in existing_replies.items():
            if cid in target_cids:
                cached.append(rec)
        if cached:
            typer.echo(f"media sweep: {len(cached)} cached records")
            sweep = _download_files_for_records(cached, media_dir, media_headers)
            for k, v in sweep.items():
                media_totals[k] = media_totals.get(k, 0) + v

    typer.echo(
        f"\nnew: {totals['messages']} messages  {totals['replies']} replies  "
        f"{totals['reactions']} reactions"
    )
    if media_dir is not None and media_totals:
        typer.echo(
            "media: " + " ".join(f"{k}={v}" for k, v in media_totals.items() if v)
        )


def main() -> None:
    typer.run(fetch)


if __name__ == "__main__":
    sys.exit(main() or 0)
