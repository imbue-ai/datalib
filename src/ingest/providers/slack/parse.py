from __future__ import annotations

import json
import uuid as uuid_lib
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

# Stable UUID namespace for Slack-derived ids. v5 hashes derived from
# `slack:{team}:{channel}:{ts}` and `slack:reaction:{message_uuid}:{name}:{user}`
# so a re-ingest of the same source events produces identical row uuids.
SLACK_UUID_NS = uuid_lib.UUID("a89c7c4f-3e3d-5a6b-9f8a-3e3d5a6b9f8a")


def slack_message_uuid(team_id: str, channel_id: str, ts: str) -> str:
    return str(uuid_lib.uuid5(SLACK_UUID_NS, f"slack:msg:{team_id}:{channel_id}:{ts}"))


def slack_thread_uuid(team_id: str, channel_id: str, thread_ts: str) -> str:
    """Distinct namespace from `slack_message_uuid` so the thread row and
    its root message row do not collide on `uuid` in grid_rows. Same input
    space, different prefix."""
    return str(
        uuid_lib.uuid5(
            SLACK_UUID_NS, f"slack:thread:{team_id}:{channel_id}:{thread_ts}"
        )
    )


def slack_reaction_uuid(message_uuid: str, name: str, user_id: str) -> str:
    return str(
        uuid_lib.uuid5(SLACK_UUID_NS, f"slack:reaction:{message_uuid}:{name}:{user_id}")
    )


def ts_to_iso(ts: str) -> str:
    """Render a Slack `ts` (unix-seconds.fractional, no source offset) as
    ISO-8601 UTC with explicit `+00:00` per project convention."""
    return datetime.fromtimestamp(float(ts), tz=timezone.utc).isoformat(
        timespec="microseconds"
    )


@dataclass
class WorkspaceRow:
    team_id: str
    team_name: str | None
    team_url: str | None
    self_user_id: str | None
    raw_json: dict[str, Any]


@dataclass
class UserRow:
    team_id: str
    user_id: str
    name: str | None
    real_name: str | None
    display_name: str | None
    title: str | None
    deleted: bool | None
    raw_json: dict[str, Any]


@dataclass
class ChannelRow:
    team_id: str
    channel_id: str
    name: str | None
    is_private: bool | None
    is_archived: bool | None
    topic: str | None
    purpose: str | None
    raw_json: dict[str, Any]


@dataclass
class MessageRow:
    uuid: str
    team_id: str
    channel_id: str
    ts: str
    thread_ts: str | None
    thread_uuid: str
    user_id: str | None
    text: str
    ts_iso: str
    is_thread_root: bool
    raw_json: dict[str, Any]


@dataclass
class ReactionRow:
    uuid: str
    message_uuid: str
    name: str
    user_id: str


@dataclass
class ParsedSlackApi:
    workspaces: list[WorkspaceRow] = field(default_factory=list)
    users: list[UserRow] = field(default_factory=list)
    channels: list[ChannelRow] = field(default_factory=list)
    messages: list[MessageRow] = field(default_factory=list)
    reactions: list[ReactionRow] = field(default_factory=list)


def _read_jsonl(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    # Iterate the file directly: `slack_web.py` writes records with
    # `ensure_ascii=False`, leaving U+2028 / U+2029 unescaped. `str.splitlines()`
    # treats those as line breaks and shreds a single record into pieces that
    # no longer parse. Python's file-iterator only splits on `\n`, which is
    # the only separator the writer actually emits.
    with path.open() as f:
        return [json.loads(line) for line in f if line.strip()]


def parse_api_dir(api_dir: Path) -> ParsedSlackApi:
    """Parse the per-entity event-stream layout written by
    `src/download/slack_web.py`:

        {self_identity,user,channel,message,reply,reaction}/{created,updated}/events.jsonl

    For ingest we read only the `created` streams — they're the cumulative
    superset (the downloader appends every record it sees there). The
    `updated` streams are an audit trail for diff inspection.
    """
    api_dir = Path(api_dir)
    out = ParsedSlackApi()

    # Workspace (self_identity) — typically one row per team for the user's
    # own account. Provides the team_id all other rows scope to.
    self_events = _read_jsonl(api_dir / "self_identity" / "created" / "events.jsonl")
    team_id: str | None = None
    self_user_id: str | None = None
    for ev in self_events:
        raw = ev.get("raw") or {}
        team_id = raw.get("team_id") or team_id
        self_user_id = raw.get("user_id") or ev.get("user_id") or self_user_id
        if team_id:
            out.workspaces.append(
                WorkspaceRow(
                    team_id=team_id,
                    team_name=raw.get("team"),
                    team_url=raw.get("url"),
                    self_user_id=self_user_id,
                    raw_json=raw,
                )
            )
    # If we never saw a self_identity event, fall back to a synthetic
    # team_id so downstream rows still have a non-null FK.
    if team_id is None:
        team_id = "unknown"

    for ev in _read_jsonl(api_dir / "user" / "created" / "events.jsonl"):
        raw = ev.get("raw") or {}
        profile = raw.get("profile") or {}
        out.users.append(
            UserRow(
                team_id=raw.get("team_id") or team_id,
                user_id=raw.get("id") or ev.get("user_id") or "",
                name=raw.get("name"),
                real_name=raw.get("real_name") or profile.get("real_name"),
                display_name=profile.get("display_name"),
                title=profile.get("title"),
                deleted=raw.get("deleted"),
                raw_json=raw,
            )
        )

    for ev in _read_jsonl(api_dir / "channel" / "created" / "events.jsonl"):
        raw = ev.get("raw") or {}
        topic = (raw.get("topic") or {}).get("value")
        purpose = (raw.get("purpose") or {}).get("value")
        out.channels.append(
            ChannelRow(
                team_id=team_id,
                channel_id=raw.get("id") or ev.get("channel_id") or "",
                name=raw.get("name") or ev.get("channel_name"),
                is_private=raw.get("is_private"),
                is_archived=raw.get("is_archived"),
                topic=topic,
                purpose=purpose,
                raw_json=raw,
            )
        )

    # Top-level messages
    for ev in _read_jsonl(api_dir / "message" / "created" / "events.jsonl"):
        raw = ev.get("raw") or {}
        channel_id = ev.get("channel_id") or ""
        ts = raw.get("ts") or ev.get("message_ts") or ""
        if not ts:
            continue
        thread_ts = raw.get("thread_ts")
        # A top-level message is a thread root if it carries a thread_ts
        # equal to its own ts (Slack convention) — or simply if no
        # thread_ts is present (lone message; treat as a 1-message thread).
        is_root = thread_ts is None or thread_ts == ts
        effective_thread_ts = thread_ts or ts
        msg_uuid = slack_message_uuid(team_id, channel_id, ts)
        thread_uuid = slack_thread_uuid(team_id, channel_id, effective_thread_ts)
        out.messages.append(
            MessageRow(
                uuid=msg_uuid,
                team_id=team_id,
                channel_id=channel_id,
                ts=ts,
                thread_ts=thread_ts,
                thread_uuid=thread_uuid,
                user_id=raw.get("user"),
                text=raw.get("text") or "",
                ts_iso=ts_to_iso(ts),
                is_thread_root=is_root,
                raw_json=raw,
            )
        )

    # Threaded replies — never roots; thread_uuid points at the root via the
    # event's `thread_ts`.
    for ev in _read_jsonl(api_dir / "reply" / "created" / "events.jsonl"):
        raw = ev.get("raw") or {}
        channel_id = ev.get("channel_id") or ""
        ts = raw.get("ts") or ev.get("reply_ts") or ""
        thread_ts = raw.get("thread_ts") or ev.get("thread_ts")
        if not ts or not thread_ts:
            continue
        msg_uuid = slack_message_uuid(team_id, channel_id, ts)
        thread_uuid = slack_thread_uuid(team_id, channel_id, thread_ts)
        out.messages.append(
            MessageRow(
                uuid=msg_uuid,
                team_id=team_id,
                channel_id=channel_id,
                ts=ts,
                thread_ts=thread_ts,
                thread_uuid=thread_uuid,
                user_id=raw.get("user"),
                text=raw.get("text") or "",
                ts_iso=ts_to_iso(ts),
                is_thread_root=False,
                raw_json=raw,
            )
        )

    # Reactions — one row per (message, emoji name, user) tuple.
    for ev in _read_jsonl(api_dir / "reaction" / "created" / "events.jsonl"):
        raw = ev.get("raw") or {}
        channel_id = ev.get("channel_id") or ""
        message_ts = ev.get("message_ts") or ""
        if not message_ts:
            continue
        msg_uuid = slack_message_uuid(team_id, channel_id, message_ts)
        for r in raw.get("reactions") or []:
            name = r.get("name")
            if not name:
                continue
            for user_id in r.get("users") or []:
                out.reactions.append(
                    ReactionRow(
                        uuid=slack_reaction_uuid(msg_uuid, name, user_id),
                        message_uuid=msg_uuid,
                        name=name,
                        user_id=user_id,
                    )
                )

    return out
