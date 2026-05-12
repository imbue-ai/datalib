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

Auth + transport: every request shells out to `latchkey curl`, which injects
the registered `notion_unofficial` cookies/headers (see NOTION_AUTH.md). For
the Cloudflare-protected endpoints (`loadCachedPageChunkV2`,
`getNotificationLog`), point `LATCHKEY_CURL` at a curl-impersonate-style
binary before invoking this script — latchkey will use it as its curl
backend so requests carry a Chrome TLS fingerprint.

Usage:
    uv run python -m download.notion_web                      # inbox, all spaces
    uv run python -m download.notion_web --subtree <page_id>  # full subtree BFS
    uv run python -m download.notion_web --space <space_id>   # restrict to one space
"""

from __future__ import annotations

import json
import logging
import subprocess
import sys
import tempfile
import time
from collections import deque
from pathlib import Path
from typing import Any, Iterable

import typer
from tqdm import tqdm

from download.latchkey_curl_shim import latchkey_env as _latchkey_env
from event_store import (
    diff_and_save as _diff_and_save,
    load_latest_by_key as _load_latest_by_key,
    make_record as _make_record,
)

DEFAULT_OUT_DIR = Path.home() / "backups" / "notion"
BASE = "https://www.notion.so/api/v3"
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
# Auth + transport: latchkey for cookie injection; LATCHKEY_CURL pointed at
# our curl_cffi shim so Cloudflare-protected endpoints clear the challenge.
# ---------------------------------------------------------------------------


class NotionError(RuntimeError):
    pass


class NotionWebClient:
    """Posts JSON to Notion's v3 endpoints via `latchkey curl`.

    Every request shells out to `latchkey curl` so latchkey injects the
    registered cookies/headers. We set `LATCHKEY_CURL` to point latchkey at
    `latchkey_curl_shim.py`, a thin wrapper around `curl_cffi` that gives us
    a Chrome TLS fingerprint for Cloudflare-protected endpoints.
    """

    def __init__(self) -> None:
        self.requests = 0
        self.network_seconds = 0.0
        self._env = _latchkey_env()

    def _post(self, method: str, body: dict[str, Any]) -> dict[str, Any]:
        url = f"{BASE}/{method}"
        payload = json.dumps(body)
        backoff = RETRY_INITIAL_BACKOFF
        for attempt in range(RETRY_MAX + 1):
            with tempfile.NamedTemporaryFile(
                prefix="notion-", suffix=".json", delete=False
            ) as bodyf:
                body_path = Path(bodyf.name)
            try:
                cmd = [
                    "latchkey",
                    "curl",
                    "-sS",
                    "-X",
                    "POST",
                    "-H",
                    "Content-Type: application/json",
                    "-H",
                    "Accept: application/json",
                    "--data",
                    payload,
                    "-o",
                    str(body_path),
                    "-w",
                    "%{http_code}",
                    url,
                ]
                t0 = time.perf_counter()
                proc = subprocess.run(
                    cmd,
                    capture_output=True,
                    text=True,
                    timeout=LATCHKEY_TIMEOUT,
                    check=False,
                    env=self._env,
                )
                self.network_seconds += time.perf_counter() - t0
                self.requests += 1
                if proc.returncode != 0:
                    raise NotionError(
                        f"{method}: latchkey curl exit {proc.returncode}; "
                        f"stderr={proc.stderr[:300]}"
                    )
                status_txt = proc.stdout.strip()
                try:
                    status = int(status_txt) if status_txt else 0
                except ValueError:
                    status = 0
                resp_text = body_path.read_text(errors="replace")
            finally:
                body_path.unlink(missing_ok=True)

            if status == 200:
                try:
                    return json.loads(resp_text)
                except json.JSONDecodeError as e:
                    raise NotionError(
                        f"{method}: HTTP 200 but non-JSON body: {e}; "
                        f"body[:200]={resp_text[:200]}"
                    )
            if status in (429, 502, 503, 504):
                if attempt == RETRY_MAX:
                    raise NotionError(
                        f"{method}: HTTP {status} after {attempt} retries; "
                        f"body={resp_text[:200]}"
                    )
                logger.warning(
                    "%s -> %d; sleeping %.0fs (attempt %d/%d)",
                    method,
                    status,
                    backoff,
                    attempt + 1,
                    RETRY_MAX,
                )
                time.sleep(backoff)
                backoff = min(backoff * 2, RETRY_MAX_BACKOFF)
                continue
            raise NotionError(f"{method}: HTTP {status} body={resp_text[:300]}")
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

    def sync_record_values(self, pointers: list[dict]) -> dict[str, Any]:
        """Fetch specific records by pointer. Used to plug holes left by
        loadCachedPageChunkV2 — e.g. comments referenced by a discussion
        whose own record never showed up in any page chunk recordMap.

        Each pointer is {"table": ..., "id": ..., "spaceId": ...}; version=-1
        asks Notion for the latest. The response is a normal recordMap, so
        `_sink_response` handles it without special-casing."""
        body = {
            "requests": [{"pointer": p, "version": -1} for p in pointers],
        }
        return self._post("syncRecordValues", body)

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
                logger.debug(
                    "unknown table %r (%d entries) — skipping", table, len(by_id)
                )
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

        omit = _block_versions_for_page(
            page_id, space_id, existing.get("notion_block", {})
        )
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
# Dangling-reference sweep: chase ids referenced by records we already have
# but whose own row never showed up in any loadCachedPageChunkV2 recordMap.
# Concrete trigger: discussions whose `comments[]` listed a comment id whose
# row was missing — Thad's reply on the Personal Data Liberation page.
# Generalized to other cross-table refs so other holes (orphan child blocks,
# DB rows, etc.) heal the same way without bespoke logic per type.
# ---------------------------------------------------------------------------


# Per source-table, the (field, target_table) refs to follow. Values are
# field accessors against `_extract_value(record.raw)`; list-typed fields
# yield each element, scalar fields yield the single id. parent_id is
# handled separately because it's gated on parent_table.
_LIST_REFS: dict[str, list[tuple[str, str]]] = {
    "notion_block": [
        ("content", "block"),
        ("discussions", "discussion"),
        ("view_ids", "collection_view"),
    ],
    "notion_discussion": [
        ("comments", "comment"),
    ],
    "notion_collection": [
        ("template_pages", "block"),
    ],
}

_SCALAR_REFS: dict[str, list[tuple[str, str]]] = {
    "notion_block": [
        ("collection_id", "collection"),
    ],
}

# (source_entity, expected parent_table, target_table)
_PARENT_REFS: list[tuple[str, str, str]] = [
    ("notion_block", "block", "block"),
    ("notion_discussion", "block", "block"),
    ("notion_comment", "discussion", "discussion"),
    ("notion_collection", "block", "block"),
]


def _subtree_block_ids(
    root_id: str,
    existing_blocks: dict[str, dict],
) -> set[str]:
    """BFS the `content[]` graph within `existing_blocks` starting at
    `root_id`. Result is the set of currently-known descendant block IDs.
    Missing children aren't here yet — they get added in the next sweep
    pass after they're fetched."""
    result: set[str] = set()
    queue: deque[str] = deque([root_id])
    while queue:
        bid = queue.popleft()
        if bid in result:
            continue
        rec = existing_blocks.get(bid)
        if rec is None:
            continue
        result.add(bid)
        value = _extract_value(rec.get("raw")) or {}
        for cid in value.get("content") or []:
            if isinstance(cid, str) and cid not in result:
                queue.append(cid)
    return result


def _subtree_scope(
    subtree_root: str,
    existing: dict[str, dict[str, dict]],
) -> dict[str, set[str]]:
    """In-scope record ids per entity for a subtree-restricted sweep.

    Blocks: descendants of `subtree_root` via `content[]`.
    Discussions: anchored at an in-scope block (`parent_table=='block'`).
    Comments: parented to an in-scope discussion.
    Collections: parented to an in-scope block.

    A ref originating from a record NOT in this scope is ignored — that's
    what prevents the sweep from chasing the entire workspace graph when
    a single page links sideways."""
    in_blocks = _subtree_block_ids(subtree_root, existing.get("notion_block", {}))

    in_discussions: set[str] = set()
    for did, drec in (existing.get("notion_discussion") or {}).items():
        dv = _extract_value(drec.get("raw")) or {}
        if (
            dv.get("parent_table") == "block"
            and isinstance(dv.get("parent_id"), str)
            and dv["parent_id"] in in_blocks
        ):
            in_discussions.add(did)

    in_comments: set[str] = set()
    for cid, crec in (existing.get("notion_comment") or {}).items():
        cv = _extract_value(crec.get("raw")) or {}
        if (
            cv.get("parent_table") == "discussion"
            and isinstance(cv.get("parent_id"), str)
            and cv["parent_id"] in in_discussions
        ):
            in_comments.add(cid)

    in_collections: set[str] = set()
    for coll_id, coll_rec in (existing.get("notion_collection") or {}).items():
        cv = _extract_value(coll_rec.get("raw")) or {}
        if (
            cv.get("parent_table") == "block"
            and isinstance(cv.get("parent_id"), str)
            and cv["parent_id"] in in_blocks
        ):
            in_collections.add(coll_id)

    return {
        "notion_block": in_blocks,
        "notion_discussion": in_discussions,
        "notion_comment": in_comments,
        "notion_collection": in_collections,
    }


def _dangling_refs(
    existing: dict[str, dict[str, dict]],
    scope: dict[str, set[str]] | None = None,
) -> dict[str, dict[str, str]]:
    """Return {target_entity: {missing_id: space_id_hint}} for every record
    referenced by something we have but absent from our own store.

    When `scope` is supplied, only refs whose source `(entity, id)` lives
    in the scope set are followed. Unscoped (`scope=None`) means walk all
    source records — only safe when the working set is naturally bounded.

    space_id_hint is the source record's space_id; refs are intra-space in
    practice (and Notion requires spaceId on the pointer). Falls back to ""
    when neither the source value nor the top-level column carries one."""
    out: dict[str, dict[str, str]] = {}

    def _consider(target_table: str, ref_id: Any, space_id: str) -> None:
        if not isinstance(ref_id, str) or not ref_id:
            return
        ent = _entity_name(target_table)
        if ref_id in existing.get(ent, {}):
            return
        bucket = out.setdefault(ent, {})
        # First hit wins for the space hint; pointers are cheap.
        bucket.setdefault(ref_id, space_id)

    for source_ent, recs in existing.items():
        list_refs = _LIST_REFS.get(source_ent, [])
        scalar_refs = _SCALAR_REFS.get(source_ent, [])
        parent_refs = [(pt, tt) for (se, pt, tt) in _PARENT_REFS if se == source_ent]
        if not (list_refs or scalar_refs or parent_refs):
            continue
        scope_ids = None if scope is None else scope.get(source_ent, set())
        for rec_id, rec in recs.items():
            if scope_ids is not None and rec_id not in scope_ids:
                continue
            value = _extract_value(rec.get("raw")) or {}
            space_id = (
                value.get("space_id")
                if isinstance(value.get("space_id"), str)
                else rec.get("space_id")
            ) or ""
            for field, target in list_refs:
                for ref_id in value.get(field) or []:
                    _consider(target, ref_id, space_id)
            for field, target in scalar_refs:
                _consider(target, value.get(field), space_id)
            for expected_pt, target in parent_refs:
                if value.get("parent_table") == expected_pt:
                    _consider(target, value.get("parent_id"), space_id)

    return out


def _resolve_dangling(
    client: NotionWebClient,
    out_dir: Path,
    existing: dict[str, dict[str, dict]],
    *,
    subtree_root: str | None = None,
    max_passes: int = 10,
    batch_size: int = 50,
) -> dict[str, tuple[int, int]]:
    """Iteratively fetch every referenced-but-missing id via syncRecordValues.
    Repeats because freshly-fetched blocks can themselves reference other
    missing records (e.g. a newly-pulled page chunk's child blocks). Capped
    at `max_passes` so a permanently-unfetchable id (deleted, no permission)
    can't loop forever.

    When `subtree_root` is given, the sweep stays inside that subtree:
    only refs originating from records anchored under `subtree_root` are
    followed. The in-scope set is recomputed each pass so newly-fetched
    descendant blocks broaden the search frontier without escaping it."""
    totals: dict[str, tuple[int, int]] = {}
    unresolved: set[tuple[str, str]] = set()
    for pass_n in range(max_passes):
        scope = _subtree_scope(subtree_root, existing) if subtree_root else None
        missing = _dangling_refs(existing, scope=scope)
        # Drop ids syncRecordValues refused to return on a prior pass —
        # otherwise a deleted/inaccessible record traps the loop.
        pending: list[tuple[str, str, str]] = []
        for ent, by_id in missing.items():
            table = ent.removeprefix("notion_")
            for ref_id, space_id in by_id.items():
                key = (table, ref_id)
                if key in unresolved:
                    continue
                pending.append((table, ref_id, space_id))
        if not pending:
            break
        logger.info(
            "dangling pass %d: %d ids across %d tables",
            pass_n + 1,
            len(pending),
            len({t for t, _, _ in pending}),
        )
        attempted_this_pass: set[tuple[str, str]] = set()
        for i in range(0, len(pending), batch_size):
            chunk = pending[i : i + batch_size]
            pointers = [
                {"table": t, "id": rid, "spaceId": sid} for (t, rid, sid) in chunk
            ]
            try:
                resp = client.sync_record_values(pointers)
            except NotionError as e:
                tqdm.write(f"  ! syncRecordValues batch: {e}")
                continue
            stats = _sink_response(out_dir, resp, existing)
            for ent, (n, u) in stats.items():
                tn, tu = totals.get(ent, (0, 0))
                totals[ent] = (tn + n, tu + u)
            for t, rid, _ in chunk:
                attempted_this_pass.add((t, rid))
        # Anything still missing after we attempted it is "unresolved" — mark
        # so we don't refetch on the next pass. New refs surfaced by records
        # added this pass remain eligible.
        for table, ref_id in attempted_this_pass:
            ent = _entity_name(table)
            if ref_id not in existing.get(ent, {}):
                unresolved.add((table, ref_id))
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

    client = NotionWebClient()

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
            f"{ent.split('_', 1)[1]}={len(existing[ent])}"
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

    root: str | None = None
    if subtree:
        raw = subtree.replace("-", "")
        root = f"{raw[0:8]}-{raw[8:12]}-{raw[12:16]}-{raw[16:20]}-{raw[20:32]}"
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
                    client,
                    out_dir,
                    sid,
                    existing,
                    notification_page_size,
                    max_notification_pages,
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

    # Plug holes: any record referenced by something we have but not yet in
    # our store (most commonly comments named in discussion.comments[] whose
    # rows never appeared in any page chunk). Runs after the main walks so
    # the working set is as complete as it'll get this run.
    #
    # Subtree mode: scope the sweep to the requested subtree so it doesn't
    # chase content[] refs out across the whole workspace. Inbox mode has
    # no single root, so we skip the sweep entirely for now — its job
    # there would be to chase notification-referenced pages, which the
    # main inbox walk already does with proper cursoring.
    if root is not None:
        typer.echo("dangling-reference sweep (scoped to subtree)")
        merge(_resolve_dangling(client, out_dir, existing, subtree_root=root))
    else:
        typer.echo("dangling-reference sweep: skipped (inbox mode)")

    typer.echo(f"\nrequests: {client.requests}  network: {client.network_seconds:.1f}s")
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
