#!/usr/bin/env python3
"""Incrementally mirror Notion (unofficial web API) record-maps to JSONL.

The Notion counterpart to download/slack_web.py. Where Slack decomposes into
hand-curated entities (channel/message/reply/...), Notion's web API already
ships its responses as a `recordMap` keyed by {table, id} — we mirror that
shape verbatim, one entity per Notion table (`notion_block`, `notion_space`,
`notion_user`, `notion_collection`, `notion_discussion`, `notion_comment`,
`notion_activity`, ...). Each record's full `{role, value}` payload is stored
under `raw`; `id`, `space_id`, `last_edited_time`, and `version` are pulled
out as top-level columns for greppability.

Default mode: `inbox` — walks `getNotificationLog` per space, then for each
referenced page calls `loadCachedPageChunkV2` and sinks the entire recordMap.
Opt-in `--subtree <page_id>` mode BFS's down a single hierarchy.

Incremental: every `loadCachedPageChunkV2` call passes the versions we already
have under `omitExistingRecordVersions`, so the server only returns records
that have changed. This is the big incremental win — re-running on a quiet
inbox costs ~one request per notification page and almost no payload.

Auth: assumes `latchkey curl` is configured for the `notion_unofficial`
service (see NOTION_AUTH.md). We harvest the injected `Cookie` /
`User-Agent` / `Notion-Client-Version` headers from `latchkey curl -v`, then
issue the real requests via `curl_cffi` with a Chrome TLS fingerprint —
Notion sits behind Cloudflare and bounces off-fingerprint clients (other
endpoints work without it, but `loadCachedPageChunkV2` / `getNotificationLog`
do not).

Usage:
    uv run python -m download.notion_web                      # inbox, all spaces
    uv run python -m download.notion_web --subtree <page_id>  # full subtree BFS
    uv run python -m download.notion_web --space <space_id>   # restrict to one space
"""

from __future__ import annotations

import logging
import re
import subprocess
import sys
import time
from collections import deque
from pathlib import Path
from typing import Any, Iterable

import typer
from curl_cffi import requests as curl_requests
from tqdm import tqdm

from event_store import (
    diff_and_save as _diff_and_save,
    load_latest_by_key as _load_latest_by_key,
    make_record as _make_record,
)

DEFAULT_OUT_DIR = Path.home() / "backups" / "notion"
BASE = "https://www.notion.so/api/v3"
IMPERSONATE = "chrome"
LATCHKEY_TIMEOUT = 60
RETRY_MAX = 6
RETRY_INITIAL_BACKOFF = 2.0
RETRY_MAX_BACKOFF = 60.0
DEFAULT_NOTIFICATION_PAGE_SIZE = 40
DEFAULT_SUBTREE_MAX_PAGES = 5000

# Notion's recordMap tables we sink. Anything else encountered is logged and
# skipped; add to this set when a new table appears in the wild.
KNOWN_TABLES: tuple[str, ...] = (
    "block",
    "space",
    "space_view",
    "space_user",
    "notion_user",
    "user_root",
    "user_settings",
    "sidebar_section",
    "collection",
    "collection_view",
    "discussion",
    "comment",
    "team",
    "activity",
)

logger = logging.getLogger("notion_web")


# ---------------------------------------------------------------------------
# Auth + transport: latchkey for cookie storage, curl_cffi for the actual
# requests (Chrome TLS fingerprint clears Cloudflare).
# ---------------------------------------------------------------------------


class NotionError(RuntimeError):
    pass


def _harvest_auth_headers() -> dict[str, str]:
    """Run `latchkey curl -v` against a known-cheap endpoint and pull out the
    Cookie / User-Agent / Notion-Client-Version that latchkey injects."""
    proc = subprocess.run(
        [
            "latchkey", "curl", "-v", "-o", "/dev/null", "-s",
            f"{BASE}/loadUserContent",
            "-X", "POST",
            "-H", "Content-Type: application/json",
            "--data", "{}",
        ],
        capture_output=True,
        text=True,
        timeout=LATCHKEY_TIMEOUT,
        check=False,
    )
    headers: dict[str, str] = {}
    for name in ("Cookie", "User-Agent", "Notion-Client-Version"):
        m = re.search(rf"^> {name}: (.+)$", proc.stderr, flags=re.MULTILINE)
        if m:
            headers[name] = m.group(1).strip()
    missing = [n for n in ("Cookie", "User-Agent") if n not in headers]
    if missing:
        raise NotionError(
            f"latchkey did not inject {missing}. "
            "Is `notion_unofficial` registered and authed? See NOTION_AUTH.md."
        )
    headers["Content-Type"] = "application/json"
    headers["Accept"] = "application/json"
    return headers


class NotionWebClient:
    def __init__(self, headers: dict[str, str]) -> None:
        self._session = curl_requests.Session(impersonate=IMPERSONATE, headers=headers)
        self.requests = 0
        self.network_seconds = 0.0

    def _post(self, method: str, body: dict[str, Any]) -> dict[str, Any]:
        url = f"{BASE}/{method}"
        backoff = RETRY_INITIAL_BACKOFF
        for attempt in range(RETRY_MAX + 1):
            t0 = time.perf_counter()
            r = self._session.post(url, json=body)
            self.network_seconds += time.perf_counter() - t0
            self.requests += 1
            if r.status_code == 200:
                return r.json()
            if r.status_code in (429, 502, 503, 504):
                if attempt == RETRY_MAX:
                    raise NotionError(
                        f"{method}: HTTP {r.status_code} after {attempt} retries; "
                        f"body={r.text[:200]}"
                    )
                logger.warning(
                    "%s -> %d; sleeping %.0fs (attempt %d/%d)",
                    method, r.status_code, backoff, attempt + 1, RETRY_MAX,
                )
                time.sleep(backoff)
                backoff = min(backoff * 2, RETRY_MAX_BACKOFF)
                continue
            raise NotionError(
                f"{method}: HTTP {r.status_code} body={r.text[:300]}"
            )
        raise AssertionError("unreachable")

    def load_user_content(self) -> dict[str, Any]:
        return self._post("loadUserContent", {})

    def get_spaces(self) -> dict[str, Any]:
        return self._post("getSpaces", {})

    def load_page_chunk(
        self,
        page_id: str,
        cursor: dict | None,
        omit_versions: list[dict],
    ) -> dict[str, Any]:
        body = {
            "page": {"id": page_id},
            "cursor": cursor or {"stack": []},
            "verticalColumns": False,
            "omitExistingRecordVersions": omit_versions,
        }
        return self._post("loadCachedPageChunkV2", body)

    def get_notification_log(
        self,
        space_id: str,
        size: int,
        cursor: dict | None = None,
        type_: str = "unread_and_read",
    ) -> dict[str, Any]:
        body: dict[str, Any] = {"spaceId": space_id, "size": size, "type": type_}
        if cursor is not None:
            body["cursor"] = cursor
        return self._post("getNotificationLog", body)


# ---------------------------------------------------------------------------
# Record sink: any Notion response → JSONL events keyed by id, one entity per
# Notion table.
# ---------------------------------------------------------------------------


def _entity_name(table: str) -> str:
    # Notion's own table names already start with "notion_" for some tables
    # (e.g. notion_user); avoid the awkward "notion_notion_user" by collapsing.
    return table if table.startswith("notion_") else f"notion_{table}"


def _key_id(rec: dict) -> str:
    return rec["id"]


def _extract_value(record_payload: Any) -> dict | None:
    """Pull the innermost value dict out of a recordMap entry.

    Notion returns two shapes interchangeably:
        {"role": "...", "value": {<value>}}
        {"value": {"role": "...", "value": {<value>}}}
    The latter shows up in syncRecordValues responses. We keep the outer
    record under `raw` regardless; this helper is just to lift top-level
    columns like id / version / last_edited_time / space_id for greppability.
    """
    if not isinstance(record_payload, dict):
        return None
    v = record_payload.get("value")
    if isinstance(v, dict) and "value" in v and isinstance(v["value"], dict):
        return v["value"]
    if isinstance(v, dict):
        return v
    return None


def _iter_record_maps(response: dict) -> Iterable[dict]:
    """Yield every recordMap-shaped dict found in a Notion response.

    Standard `{recordMap: {...}}` envelope covers most endpoints; `getSpaces`
    returns `{<user_id>: {<table>: {...}}}` instead, so we also scan one level
    deeper for dicts whose keys overlap KNOWN_TABLES.
    """
    if not isinstance(response, dict):
        return
    rm = response.get("recordMap")
    if isinstance(rm, dict):
        yield rm
    for v in response.values():
        if isinstance(v, dict) and any(t in v for t in KNOWN_TABLES):
            # avoid double-yielding the same recordMap we already saw
            if v is not rm:
                yield v


def _collect_records_by_entity(
    response: dict,
) -> dict[str, list[dict[str, Any]]]:
    """Walk every recordMap in `response` and bucket records by entity.

    Top-level columns: id, space_id (when present), version, last_edited_time.
    Everything else lives under `raw` (the original recordMap entry, role + all).
    """
    buckets: dict[str, list[dict[str, Any]]] = {}
    seen_keys: set[tuple[str, str]] = set()
    for rm in _iter_record_maps(response):
        for table, by_id in rm.items():
            if table == "__version__" or not isinstance(by_id, dict):
                continue
            if table not in KNOWN_TABLES:
                logger.debug("unknown table %r (%d entries) — skipping", table, len(by_id))
                continue
            for rec_id, payload in by_id.items():
                if (table, rec_id) in seen_keys:
                    continue
                seen_keys.add((table, rec_id))
                value = _extract_value(payload) or {}
                key_fields: dict[str, Any] = {"id": rec_id}
                if isinstance(value.get("space_id"), str):
                    key_fields["space_id"] = value["space_id"]
                if isinstance(value.get("version"), int):
                    key_fields["version"] = value["version"]
                if isinstance(value.get("last_edited_time"), int):
                    key_fields["last_edited_time"] = value["last_edited_time"]
                rec = _make_record(key_fields, payload)
                buckets.setdefault(_entity_name(table), []).append(rec)
    return buckets


def _sink_response(
    out_dir: Path,
    response: dict,
    existing: dict[str, dict[str, dict]],
) -> dict[str, tuple[int, int]]:
    """Bucket → diff_and_save per entity. Mutates `existing` so subsequent
    sinks in the same run see freshly-written records as already-known."""
    stats: dict[str, tuple[int, int]] = {}
    for entity, records in _collect_records_by_entity(response).items():
        ex = existing.setdefault(entity, {})
        new, upd = _diff_and_save(out_dir, entity, records, ex, _key_id)
        # roll fresh records into `existing` so later sinks skip duplicates
        for r in records:
            ex[_key_id(r)] = r
        stats[entity] = (new, upd)
    return stats


# ---------------------------------------------------------------------------
# Fetch logic.
# ---------------------------------------------------------------------------


# Notion's loadCachedPageChunkV2 caps the request body around ~250 KB (returns
# HTTP 413 PayloadTooLargeError beyond that). Each pointer encodes to ~150 B,
# so ~100 pointers leaves ample headroom. Prioritize blocks plausibly *on the
# page being requested*: the page itself, then its known direct children
# (parent_id == page_id), then the rest as filler if we have room. The cap
# only affects bandwidth (server returns full payload for omitted blocks);
# diff_and_save still dedupes them on the client.
_MAX_OMIT_VERSIONS = 100


def _block_versions_for_page(
    page_id: str,
    space_id: str,
    existing_blocks: dict[str, dict],
) -> list[dict]:
    def _ptr(bid: str, version: int) -> dict:
        return {
            "pointer": {"id": bid, "table": "block", "spaceId": space_id},
            "version": version,
        }

    out: list[dict] = []
    page_rec = existing_blocks.get(page_id)
    if page_rec is not None and isinstance(page_rec.get("version"), int):
        out.append(_ptr(page_id, page_rec["version"]))

    # Walk in two passes so children of the requested page get priority.
    children: list[dict] = []
    others: list[dict] = []
    for bid, rec in existing_blocks.items():
        if bid == page_id:
            continue
        if rec.get("space_id") != space_id:
            continue
        v = rec.get("version")
        if not isinstance(v, int):
            continue
        value = _extract_value(rec.get("raw")) or {}
        bucket = children if value.get("parent_id") == page_id else others
        bucket.append(_ptr(bid, v))

    for ptr in children:
        if len(out) >= _MAX_OMIT_VERSIONS:
            break
        out.append(ptr)
    for ptr in others:
        if len(out) >= _MAX_OMIT_VERSIONS:
            break
        out.append(ptr)
    return out


def _fetch_page(
    client: NotionWebClient,
    out_dir: Path,
    page_id: str,
    space_id: str,
    existing: dict[str, dict[str, dict]],
) -> dict[str, tuple[int, int]]:
    """Walk all cursors for a page, sinking each chunk's recordMap."""
    cursors: deque[dict] = deque([{"stack": []}])
    visited_cursor_keys: set[str] = set()
    totals: dict[str, tuple[int, int]] = {}
    while cursors:
        cur = cursors.popleft()
        # cheap dedupe on cursor identity so a malformed response can't loop
        key = repr(cur)
        if key in visited_cursor_keys:
            continue
        visited_cursor_keys.add(key)

        omit = _block_versions_for_page(page_id, space_id, existing.get("notion_block", {}))
        resp = client.load_page_chunk(page_id, cur, omit)
        stats = _sink_response(out_dir, resp, existing)
        for ent, (n, u) in stats.items():
            tn, tu = totals.get(ent, (0, 0))
            totals[ent] = (tn + n, tu + u)
        for next_cur in resp.get("cursors") or []:
            cursors.append(next_cur)
    return totals


def _extract_space_id_for_page(
    page_id: str, existing_blocks: dict[str, dict]
) -> str | None:
    rec = existing_blocks.get(page_id)
    if rec is None:
        return None
    return rec.get("space_id")


def _walk_inbox(
    client: NotionWebClient,
    out_dir: Path,
    space_id: str,
    existing: dict[str, dict[str, dict]],
    page_size: int,
    max_pages: int,
    type_: str = "unread_and_read",
) -> tuple[list[str], dict[str, tuple[int, int]]]:
    """Page through getNotificationLog, sinking every activity/recordMap we
    see. Returns the set of referenced page (block) IDs to deep-fetch next.

    Stops when the server runs out of items or we hit `max_pages` API calls
    (safety bound — a backlogged inbox can be huge)."""
    cursor: dict | None = None
    referenced: list[str] = []
    totals: dict[str, tuple[int, int]] = {}
    for _ in range(max_pages):
        resp = client.get_notification_log(space_id, page_size, cursor, type_=type_)
        stats = _sink_response(out_dir, resp, existing)
        for ent, (n, u) in stats.items():
            tn, tu = totals.get(ent, (0, 0))
            totals[ent] = (tn + n, tu + u)
        # Each activity has a `navigable_block_id` pointing at the page.
        # notificationIds in the response are opaque/separate; iterate the
        # activity recordMap directly.
        rm = resp.get("recordMap") or {}
        activities = rm.get("activity") or {}
        for payload in activities.values():
            value = _extract_value(payload) or {}
            nav = value.get("navigable_block_id")
            if isinstance(nav, str):
                referenced.append(nav)
        ids = resp.get("notificationIds") or []
        next_cursor = resp.get("cursor")
        if not next_cursor or not ids:
            break
        cursor = next_cursor
    return referenced, totals


def _child_page_ids(value: dict) -> list[str]:
    """Block IDs referenced by a page-like block's `content` array that are
    themselves pages — these get queued in subtree BFS."""
    return [c for c in (value.get("content") or []) if isinstance(c, str)]


def _is_page_or_database(value: dict) -> bool:
    t = value.get("type")
    return t in ("page", "collection_view_page", "collection_view")


def _walk_subtree(
    client: NotionWebClient,
    out_dir: Path,
    root_page_id: str,
    space_id: str,
    existing: dict[str, dict[str, dict]],
    max_pages: int,
) -> dict[str, tuple[int, int]]:
    queue: deque[str] = deque([root_page_id])
    queued: set[str] = {root_page_id}
    visited: set[str] = set()
    totals: dict[str, tuple[int, int]] = {}
    pbar = tqdm(total=1, unit="pg", desc="subtree")
    while queue and len(visited) < max_pages:
        pid = queue.popleft()
        if pid in visited:
            continue
        visited.add(pid)
        pbar.set_postfix_str(pid[:8])
        try:
            stats = _fetch_page(client, out_dir, pid, space_id, existing)
        except NotionError as e:
            tqdm.write(f"  ! {pid}: {e}")
            continue
        for ent, (n, u) in stats.items():
            tn, tu = totals.get(ent, (0, 0))
            totals[ent] = (tn + n, tu + u)
        # Enqueue direct child pages of `pid` by reading its block's content.
        # parent-link traversal would also work but doubles back on the whole
        # cached block table per page; this is O(children) not O(all blocks).
        pid_rec = existing.get("notion_block", {}).get(pid)
        pid_value = _extract_value(pid_rec.get("raw")) if pid_rec else None
        for child_id in (pid_value or {}).get("content") or []:
            if child_id in queued or child_id in visited:
                continue
            child_rec = existing.get("notion_block", {}).get(child_id)
            child_value = _extract_value(child_rec.get("raw")) if child_rec else None
            if not child_value or not _is_page_or_database(child_value):
                continue
            queue.append(child_id)
            queued.add(child_id)
            pbar.total += 1
            pbar.refresh()
        pbar.update(1)
    pbar.close()
    return totals


# ---------------------------------------------------------------------------
# Entry point: typer CLI.
# ---------------------------------------------------------------------------


def fetch(
    out_dir: Path = typer.Option(
        DEFAULT_OUT_DIR,
        "--out-dir",
        help=f"Where to write the JSONL streams (default {DEFAULT_OUT_DIR}).",
    ),
    subtree: str = typer.Option(
        None,
        "--subtree",
        help=(
            "Root page id (UUID with or without dashes) to BFS-mirror. If "
            "omitted, inbox mode is used instead."
        ),
    ),
    space: str = typer.Option(
        None,
        "--space",
        help=(
            "Restrict to one space id (UUID). Default: every space your user "
            "belongs to. Required when --subtree's space is ambiguous."
        ),
    ),
    notification_page_size: int = typer.Option(
        DEFAULT_NOTIFICATION_PAGE_SIZE,
        "--notification-page-size",
        help=f"getNotificationLog page size (default {DEFAULT_NOTIFICATION_PAGE_SIZE}).",
    ),
    max_notification_pages: int = typer.Option(
        50,
        "--max-notification-pages",
        help="Safety bound on inbox pagination per space per type. Default: 50.",
    ),
    inbox_types: str = typer.Option(
        "unread_and_read",
        "--inbox-types",
        help=(
            "Comma-separated notification feed types to walk. Valid values "
            "include `unread_and_read` (active inbox) and `archived`. "
            "Default: unread_and_read."
        ),
    ),
    subtree_max_pages: int = typer.Option(
        DEFAULT_SUBTREE_MAX_PAGES,
        "--subtree-max-pages",
        help=f"Safety bound on subtree BFS. Default: {DEFAULT_SUBTREE_MAX_PAGES}.",
    ),
    verbose: bool = typer.Option(False, "--verbose", "-v", help="Debug logging."),
) -> None:
    """Incrementally mirror Notion (unofficial web API) to JSONL event streams."""
    logging.basicConfig(
        level=logging.DEBUG if verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    out_dir = out_dir.expanduser()
    out_dir.mkdir(parents=True, exist_ok=True)

    headers = _harvest_auth_headers()
    client = NotionWebClient(headers)

    # Load all known entities up front. The set of entities can grow as we
    # encounter new tables, so we load lazily inside _sink_response too.
    existing: dict[str, dict[str, dict]] = {}
    for t in KNOWN_TABLES:
        ent = _entity_name(t)
        existing[ent] = _load_latest_by_key(out_dir, ent, _key_id)

    typer.echo(f"out: {out_dir}")
    typer.echo(
        "existing: "
        + " ".join(
            f"{ent.split('_',1)[1]}={len(existing[ent])}"
            for ent in sorted(existing)
            if existing[ent]
        )
        or "existing: (empty)"
    )

    # Bootstrap: loadUserContent + getSpaces always run — cheap and gives us
    # spaces + the self user before any deeper work.
    typer.echo("bootstrap: loadUserContent + getSpaces")
    user_stats = _sink_response(out_dir, client.load_user_content(), existing)
    space_stats = _sink_response(out_dir, client.get_spaces(), existing)
    for ent, (n, u) in {**user_stats, **space_stats}.items():
        if n or u:
            typer.echo(f"  {ent}: +{n} ~{u}")

    space_ids: list[str]
    if space is not None:
        space_ids = [space]
    else:
        space_ids = sorted(existing.get("notion_space", {}).keys())
    if not space_ids:
        typer.echo("no spaces discovered — check auth.")
        raise typer.Exit(1)
    typer.echo(f"spaces: {space_ids}")

    grand: dict[str, tuple[int, int]] = {}

    def merge(stats: dict[str, tuple[int, int]]) -> None:
        for ent, (n, u) in stats.items():
            tn, tu = grand.get(ent, (0, 0))
            grand[ent] = (tn + n, tu + u)

    if subtree:
        root = subtree.replace("-", "")
        root = f"{root[0:8]}-{root[8:12]}-{root[12:16]}-{root[16:20]}-{root[20:32]}"
        # Use the space we already know (single-space case) or require --space.
        if len(space_ids) > 1 and space is None:
            typer.echo(
                "multiple spaces visible; pass --space <space_id> alongside --subtree.",
                err=True,
            )
            raise typer.Exit(2)
        sid = space_ids[0]
        typer.echo(f"subtree: {root} in {sid}")
        merge(_walk_subtree(client, out_dir, root, sid, existing, subtree_max_pages))
    else:
        types = [t.strip() for t in inbox_types.split(",") if t.strip()]
        for sid in space_ids:
            all_refs: list[str] = []
            for t in types:
                typer.echo(f"inbox[{t}]: {sid}")
                refs, inbox_stats = _walk_inbox(
                    client, out_dir, sid, existing,
                    notification_page_size, max_notification_pages,
                    type_=t,
                )
                merge(inbox_stats)
                all_refs.extend(refs)
            unique_refs = list(dict.fromkeys(all_refs))
            typer.echo(f"  {len(unique_refs)} unique referenced pages")
            pbar = tqdm(unique_refs, unit="pg", desc=f"pages in {sid[:8]}")
            for pid in pbar:
                pbar.set_postfix_str(pid[:8])
                try:
                    merge(_fetch_page(client, out_dir, pid, sid, existing))
                except NotionError as e:
                    tqdm.write(f"  ! {pid}: {e}")

    typer.echo(
        f"\nrequests: {client.requests}  network: {client.network_seconds:.1f}s"
    )
    if grand:
        typer.echo("new/updated:")
        for ent in sorted(grand):
            n, u = grand[ent]
            if n or u:
                typer.echo(f"  {ent}: +{n} ~{u}")
    else:
        typer.echo("no changes")


def main() -> None:
    typer.run(fetch)


if __name__ == "__main__":
    sys.exit(main() or 0)
