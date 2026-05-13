#!/usr/bin/env python3
"""Mirror Notion pages via the official `api.notion.com` API, with
inbox discovery via the unofficial web API.

The official API doesn't expose notifications, so inbox mode is hybrid:
the unofficial `getNotificationLog` endpoint discovers page IDs, then the
official API fetches each page's content + comments.

Two modes:
  - `--subtree <page_id>`: BFS-mirror one hierarchy, no unofficial calls.
  - `--inbox`: walk getNotificationLog per space (unofficial), then
    fetch each referenced page via the official API.

Auth: two latchkey services.
  - `notion`: Bearer token for `api.notion.com` (PAT or integration token).
  - `notion_unofficial`: cookie session for `www.notion.so/api/v3`,
    needed only when `--inbox` is used.

Storage: three entities under the event store.
  - `notion_official_page` keyed by `id` (page UUID).
  - `notion_official_block` keyed by `id` (block UUID).
  - `notion_official_comment` keyed by `id` (comment UUID).

Usage:
    uv run python -m download.notion_official --subtree <page_id>
    uv run python -m download.notion_official --inbox
    uv run python -m download.notion_official --inbox --space <space_id>
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
BASE = "https://api.notion.com/v1"
UNOFFICIAL_BASE = "https://www.notion.so/api/v3"
LATCHKEY_TIMEOUT = 180
RETRY_MAX = 6
RETRY_INITIAL_BACKOFF = 2.0
RETRY_MAX_BACKOFF = 60.0
PAGE_SIZE = 100  # max allowed by official API
DEFAULT_NOTIFICATION_PAGE_SIZE = 40
DEFAULT_MAX_NOTIFICATION_PAGES = 50

ENTITY_PAGE = "notion_official_page"
ENTITY_BLOCK = "notion_official_block"
ENTITY_COMMENT = "notion_official_comment"

logger = logging.getLogger(__name__)


class NotionOfficialError(RuntimeError):
    pass


class NotionOfficialClient:
    """GET/POST against api.notion.com via `latchkey curl`. Latchkey injects
    the Bearer token registered on the `notion` service."""

    def __init__(self) -> None:
        self.requests = 0
        self.network_seconds = 0.0

    def _request(
        self, method: str, path: str, body: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        url = f"{BASE}{path}"
        # Note: latchkey's notion service injects Notion-Version itself, so
        # don't pass one here or Notion sees two comma-joined values and 400s.
        cmd = [
            "latchkey",
            "curl",
            "-sS",
            "-X",
            method,
            "-H",
            "Accept: application/json",
        ]
        if body is not None:
            cmd += ["-H", "Content-Type: application/json", "--data", json.dumps(body)]
        cmd += ["-w", "\n%{http_code}", url]

        backoff = RETRY_INITIAL_BACKOFF
        for attempt in range(RETRY_MAX + 1):
            t0 = time.perf_counter()
            try:
                proc = subprocess.run(
                    cmd,
                    capture_output=True,
                    text=True,
                    timeout=LATCHKEY_TIMEOUT,
                    check=False,
                )
            except subprocess.TimeoutExpired as e:
                # Wrap so the per-page error handler can catch and skip.
                self.network_seconds += time.perf_counter() - t0
                self.requests += 1
                raise NotionOfficialError(
                    f"{method} {path}: timeout after {LATCHKEY_TIMEOUT}s"
                ) from e
            self.network_seconds += time.perf_counter() - t0
            self.requests += 1
            if proc.returncode != 0:
                raise NotionOfficialError(
                    f"{method} {path}: latchkey exit {proc.returncode}; "
                    f"stderr={proc.stderr[:300]}"
                )
            # last line is the HTTP status; rest is the body.
            out = proc.stdout
            nl = out.rfind("\n")
            body_text = out[:nl] if nl >= 0 else out
            status_txt = (out[nl + 1 :] if nl >= 0 else "").strip()
            try:
                status = int(status_txt)
            except ValueError:
                status = 0

            if status == 200:
                try:
                    return json.loads(body_text)
                except json.JSONDecodeError as e:
                    raise NotionOfficialError(
                        f"{method} {path}: HTTP 200 but non-JSON: {e}; "
                        f"body[:200]={body_text[:200]}"
                    )
            if status in (429, 502, 503, 504):
                if attempt == RETRY_MAX:
                    raise NotionOfficialError(
                        f"{method} {path}: HTTP {status} after {attempt} retries"
                    )
                logger.warning(
                    "%s %s -> %d; sleeping %.0fs (attempt %d/%d)",
                    method,
                    path,
                    status,
                    backoff,
                    attempt + 1,
                    RETRY_MAX,
                )
                time.sleep(backoff)
                backoff = min(backoff * 2, RETRY_MAX_BACKOFF)
                continue
            raise NotionOfficialError(
                f"{method} {path}: HTTP {status} body={body_text[:300]}"
            )
        raise AssertionError("unreachable")

    def get_page(self, page_id: str) -> dict[str, Any]:
        return self._request("GET", f"/pages/{page_id}")

    def get_block_children(
        self, block_id: str, start_cursor: str | None = None
    ) -> dict[str, Any]:
        q = f"?page_size={PAGE_SIZE}"
        if start_cursor:
            q += f"&start_cursor={start_cursor}"
        return self._request("GET", f"/blocks/{block_id}/children{q}")

    def get_database(self, database_id: str) -> dict[str, Any]:
        return self._request("GET", f"/databases/{database_id}")

    def query_database(
        self, database_id: str, start_cursor: str | None = None
    ) -> dict[str, Any]:
        body: dict[str, Any] = {"page_size": PAGE_SIZE}
        if start_cursor:
            body["start_cursor"] = start_cursor
        return self._request("POST", f"/databases/{database_id}/query", body=body)

    def get_comments(
        self, block_id: str, start_cursor: str | None = None
    ) -> dict[str, Any]:
        """List comments anchored to `block_id`. When `block_id` is a page,
        Notion returns every comment on that page across discussions."""
        q = f"?block_id={block_id}&page_size={PAGE_SIZE}"
        if start_cursor:
            q += f"&start_cursor={start_cursor}"
        return self._request("GET", f"/comments{q}")


class NotionUnofficialError(RuntimeError):
    pass


class NotionUnofficialClient:
    """Tiny client for the handful of unofficial endpoints we still need:
    `loadUserContent`, `getSpaces`, `getNotificationLog`. Used for inbox
    discovery only — the public API has no notification equivalent.

    Uses the `notion_unofficial` latchkey service (cookie session) and
    routes through `latchkey_curl_shim` so Cloudflare-protected endpoints
    clear the challenge."""

    def __init__(self) -> None:
        self.requests = 0
        self.network_seconds = 0.0
        self._env = _latchkey_env()

    def _post(self, method: str, body: dict[str, Any]) -> dict[str, Any]:
        url = f"{UNOFFICIAL_BASE}/{method}"
        payload = json.dumps(body)
        backoff = RETRY_INITIAL_BACKOFF
        for attempt in range(RETRY_MAX + 1):
            with tempfile.NamedTemporaryFile(
                prefix="notion-uo-", suffix=".json", delete=False
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
                try:
                    proc = subprocess.run(
                        cmd,
                        capture_output=True,
                        text=True,
                        timeout=LATCHKEY_TIMEOUT,
                        check=False,
                        env=self._env,
                    )
                except subprocess.TimeoutExpired as e:
                    self.network_seconds += time.perf_counter() - t0
                    self.requests += 1
                    raise NotionUnofficialError(
                        f"{method}: timeout after {LATCHKEY_TIMEOUT}s"
                    ) from e
                self.network_seconds += time.perf_counter() - t0
                self.requests += 1
                if proc.returncode != 0:
                    raise NotionUnofficialError(
                        f"{method}: latchkey exit {proc.returncode}; "
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
                    raise NotionUnofficialError(
                        f"{method}: HTTP 200 but non-JSON: {e}; "
                        f"body[:200]={resp_text[:200]}"
                    )
            if status in (429, 502, 503, 504):
                if attempt == RETRY_MAX:
                    raise NotionUnofficialError(
                        f"{method}: HTTP {status} after {attempt} retries"
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
            raise NotionUnofficialError(
                f"{method}: HTTP {status} body={resp_text[:300]}"
            )
        raise AssertionError("unreachable")

    def load_user_content(self) -> dict[str, Any]:
        return self._post("loadUserContent", {})

    def get_spaces(self) -> dict[str, Any]:
        return self._post("getSpaces", {})

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
# Sink: page / block / comment dicts → JSONL event store
# ---------------------------------------------------------------------------


def _page_key(rec: dict) -> str:
    return rec["id"]


def _block_key(rec: dict) -> str:
    return rec["id"]


def _comment_key(rec: dict) -> str:
    return rec["id"]


def _page_record(page: dict) -> dict[str, Any]:
    return _make_record(
        {
            "id": page["id"],
            "last_edited_time": page.get("last_edited_time"),
            "parent": page.get("parent"),
        },
        page,
    )


def _block_record(block: dict, page_id: str) -> dict[str, Any]:
    return _make_record(
        {
            "id": block["id"],
            "page_id": page_id,
            "type": block.get("type"),
            "last_edited_time": block.get("last_edited_time"),
        },
        block,
    )


def _comment_record(comment: dict, page_id: str) -> dict[str, Any]:
    # The comment's `parent` carries page_id OR block_id; we record `page_id`
    # as the page we discovered the comment under, separate from the anchor.
    parent = comment.get("parent") or {}
    return _make_record(
        {
            "id": comment["id"],
            "page_id": page_id,
            "discussion_id": comment.get("discussion_id"),
            "parent_block_id": parent.get("block_id"),
            "parent_page_id": parent.get("page_id"),
            "created_time": comment.get("created_time"),
            "last_edited_time": comment.get("last_edited_time"),
        },
        comment,
    )


# ---------------------------------------------------------------------------
# Page-content traversal
# ---------------------------------------------------------------------------


def _fetch_all_children(
    client: NotionOfficialClient, parent_id: str
) -> list[dict[str, Any]]:
    """Paginate `/blocks/{id}/children` for a single parent. Returns the flat
    list of direct children in order."""
    out: list[dict[str, Any]] = []
    cursor: str | None = None
    while True:
        resp = client.get_block_children(parent_id, start_cursor=cursor)
        out.extend(resp.get("results") or [])
        if not resp.get("has_more"):
            return out
        cursor = resp.get("next_cursor")
        if not cursor:
            return out


def _walk_page_blocks(
    client: NotionOfficialClient, page_id: str
) -> list[dict[str, Any]]:
    """Depth-first walk of all blocks under `page_id`, *not* descending into
    nested child_page / child_database blocks (those are separate pages and
    will be fetched in their own pass)."""
    collected: list[dict[str, Any]] = []
    queue: deque[str] = deque([page_id])
    seen: set[str] = set()
    while queue:
        pid = queue.popleft()
        if pid in seen:
            continue
        seen.add(pid)
        children = _fetch_all_children(client, pid)
        for ch in children:
            collected.append(ch)
            t = ch.get("type")
            # child_page / child_database are page-level boundaries — don't
            # recurse into their bodies here; they get their own _walk_page
            # call from the outer driver.
            if t in ("child_page", "child_database"):
                continue
            if ch.get("has_children"):
                queue.append(ch["id"])
    return collected


def _child_page_ids(blocks: list[dict[str, Any]]) -> list[str]:
    return [b["id"] for b in blocks if b.get("type") == "child_page"]


def _fetch_all_comments(
    client: NotionOfficialClient, page_id: str
) -> list[dict[str, Any]]:
    """Paginate `/comments?block_id=<page_id>` and return every comment
    on the page (Notion returns thread comments via the same endpoint
    when the block is a page)."""
    out: list[dict[str, Any]] = []
    cursor: str | None = None
    while True:
        resp = client.get_comments(page_id, start_cursor=cursor)
        out.extend(resp.get("results") or [])
        if not resp.get("has_more"):
            return out
        cursor = resp.get("next_cursor")
        if not cursor:
            return out


# ---------------------------------------------------------------------------
# Unofficial recordMap helpers — only used to extract `navigable_block_id`
# from notification activity entries during inbox discovery.
# ---------------------------------------------------------------------------


def _extract_value(record_payload: Any) -> dict | None:
    if not isinstance(record_payload, dict):
        return None
    v = record_payload.get("value")
    if isinstance(v, dict) and "value" in v and isinstance(v["value"], dict):
        return v["value"]
    if isinstance(v, dict):
        return v
    return None


def _walk_inbox(
    client: NotionUnofficialClient,
    space_id: str,
    page_size: int,
    max_pages: int,
    types: Iterable[str],
) -> list[str]:
    """Return the deduped list of page IDs referenced by notifications in
    `space_id` across all requested feed types."""
    seen: dict[str, None] = {}
    for type_ in types:
        cursor: dict | None = None
        for _ in range(max_pages):
            resp = client.get_notification_log(space_id, page_size, cursor, type_=type_)
            rm = resp.get("recordMap") or {}
            for payload in (rm.get("activity") or {}).values():
                value = _extract_value(payload) or {}
                nav = value.get("navigable_block_id")
                if isinstance(nav, str):
                    seen.setdefault(nav, None)
            ids = resp.get("notificationIds") or []
            next_cursor = resp.get("cursor")
            if not next_cursor or not ids:
                break
            cursor = next_cursor
    return list(seen.keys())


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def _format_uuid(s: str) -> str:
    raw = s.replace("-", "")
    if len(raw) != 32:
        return s
    return f"{raw[0:8]}-{raw[8:12]}-{raw[12:16]}-{raw[16:20]}-{raw[20:32]}"


def _mirror_page(
    client: NotionOfficialClient,
    out_dir: Path,
    pid: str,
    existing_pages: dict[str, dict],
    existing_blocks: dict[str, dict],
    existing_comments: dict[str, dict],
    counts: dict[str, int],
) -> list[dict[str, Any]]:
    """Fetch + sink page record, blocks, and comments for one page id.
    Returns the block list so the caller can enqueue child_page descendants.
    Mutates `counts` (`new_pages`, `upd_pages`, `new_blocks`, `upd_blocks`,
    `new_comments`, `upd_comments`)."""
    try:
        page = client.get_page(pid)
    except NotionOfficialError as e:
        tqdm.write(f"  ! page {pid}: {e}")
        return []
    n, u = _diff_and_save(
        out_dir, ENTITY_PAGE, [_page_record(page)], existing_pages, _page_key
    )
    counts["new_pages"] += n
    counts["upd_pages"] += u
    existing_pages[pid] = _page_record(page)

    try:
        blocks = _walk_page_blocks(client, pid)
    except NotionOfficialError as e:
        tqdm.write(f"  ! blocks {pid}: {e}")
        return []
    block_records = [_block_record(b, pid) for b in blocks]
    n, u = _diff_and_save(
        out_dir, ENTITY_BLOCK, block_records, existing_blocks, _block_key
    )
    counts["new_blocks"] += n
    counts["upd_blocks"] += u
    for br in block_records:
        existing_blocks[br["id"]] = br

    try:
        comments = _fetch_all_comments(client, pid)
    except NotionOfficialError as e:
        tqdm.write(f"  ! comments {pid}: {e}")
        comments = []
    if comments:
        comment_records = [_comment_record(c, pid) for c in comments]
        n, u = _diff_and_save(
            out_dir,
            ENTITY_COMMENT,
            comment_records,
            existing_comments,
            _comment_key,
        )
        counts["new_comments"] += n
        counts["upd_comments"] += u
        for cr in comment_records:
            existing_comments[cr["id"]] = cr

    return blocks


def fetch(
    subtree: str = typer.Option(
        None,
        "--subtree",
        help="Root page id (UUID, dashed or undashed) to mirror.",
    ),
    inbox: bool = typer.Option(
        False,
        "--inbox",
        help=(
            "Discover pages via the unofficial getNotificationLog endpoint "
            "(requires `notion_unofficial` latchkey service)."
        ),
    ),
    out_dir: Path = typer.Option(
        DEFAULT_OUT_DIR,
        "--out-dir",
        help=f"Where to write JSONL streams (default {DEFAULT_OUT_DIR}).",
    ),
    space: str = typer.Option(
        None,
        "--space",
        help="Inbox mode: restrict to one space id (default: all visible spaces).",
    ),
    notification_page_size: int = typer.Option(
        DEFAULT_NOTIFICATION_PAGE_SIZE,
        "--notification-page-size",
        help=f"getNotificationLog page size (default {DEFAULT_NOTIFICATION_PAGE_SIZE}).",
    ),
    max_notification_pages: int = typer.Option(
        DEFAULT_MAX_NOTIFICATION_PAGES,
        "--max-notification-pages",
        help=(
            "Safety bound on inbox pagination per space per type. "
            f"Default: {DEFAULT_MAX_NOTIFICATION_PAGES}."
        ),
    ),
    inbox_types: list[str] = typer.Option(
        ["unread_and_read"],
        "--inbox-types",
        help=(
            "Notification feed types to walk (repeatable). Valid values "
            "include `unread_and_read` and `archived`."
        ),
    ),
    max_pages: int = typer.Option(
        5000,
        "--max-pages",
        help="Safety bound on BFS page count. Default 5000.",
    ),
    verbose: bool = typer.Option(False, "--verbose", "-v"),
) -> None:
    """Mirror Notion pages via the official API.

    Two modes (exactly one required):
      --subtree <id>: BFS-mirror that page's hierarchy.
      --inbox: discover pages via the unofficial getNotificationLog,
               then mirror each one.
    """
    logging.basicConfig(
        level=logging.DEBUG if verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    if subtree is None and not inbox:
        typer.echo("must specify either --subtree <id> or --inbox", err=True)
        raise typer.Exit(2)
    if subtree is not None and inbox:
        typer.echo("--subtree and --inbox are mutually exclusive", err=True)
        raise typer.Exit(2)

    out_dir = out_dir.expanduser()
    out_dir.mkdir(parents=True, exist_ok=True)

    client = NotionOfficialClient()
    existing_pages = _load_latest_by_key(out_dir, ENTITY_PAGE, _page_key)
    existing_blocks = _load_latest_by_key(out_dir, ENTITY_BLOCK, _block_key)
    existing_comments = _load_latest_by_key(out_dir, ENTITY_COMMENT, _comment_key)
    typer.echo(
        f"out: {out_dir}  existing: pages={len(existing_pages)} "
        f"blocks={len(existing_blocks)} comments={len(existing_comments)}"
    )

    # Seed the BFS queue.
    queue: deque[str] = deque()
    queued: set[str] = set()
    uo_client: NotionUnofficialClient | None = None

    if subtree is not None:
        root = _format_uuid(subtree)
        queue.append(root)
        queued.add(root)
    else:
        uo_client = NotionUnofficialClient()
        typer.echo("inbox bootstrap: loadUserContent + getSpaces")
        uo_client.load_user_content()
        spaces_resp = uo_client.get_spaces()
        # getSpaces shape: {<user_id>: {space: {<space_id>: ...}}}
        space_ids: list[str] = []
        if space is not None:
            space_ids = [space]
        else:
            for v in spaces_resp.values():
                if isinstance(v, dict):
                    sp = v.get("space") or {}
                    for sid in sp.keys():
                        if sid not in space_ids:
                            space_ids.append(sid)
        if not space_ids:
            typer.echo("no spaces discovered — check notion_unofficial auth", err=True)
            raise typer.Exit(1)
        typer.echo(f"spaces: {space_ids}")
        for sid in space_ids:
            refs = _walk_inbox(
                uo_client,
                sid,
                notification_page_size,
                max_notification_pages,
                inbox_types,
            )
            typer.echo(f"  inbox[{sid[:8]}]: {len(refs)} referenced pages")
            for rid in refs:
                pid = _format_uuid(rid)
                if pid in queued:
                    continue
                queue.append(pid)
                queued.add(pid)

    counts = {
        "new_pages": 0, "upd_pages": 0,
        "new_blocks": 0, "upd_blocks": 0,
        "new_comments": 0, "upd_comments": 0,
    }
    visited: set[str] = set()
    pbar = tqdm(total=len(queue), unit="pg", desc="fetch")
    while queue and len(visited) < max_pages:
        pid = queue.popleft()
        if pid in visited:
            continue
        visited.add(pid)
        pbar.set_postfix_str(pid[:8])
        blocks = _mirror_page(
            client, out_dir, pid,
            existing_pages, existing_blocks, existing_comments,
            counts,
        )
        # Enqueue child pages (only useful in subtree mode, but harmless in
        # inbox mode — referenced pages whose children we didn't see in the
        # inbox are usually wanted too).
        for cid in _child_page_ids(blocks):
            if cid in queued or cid in visited:
                continue
            queue.append(cid)
            queued.add(cid)
            pbar.total += 1
            pbar.refresh()
        pbar.update(1)
    pbar.close()

    typer.echo(
        f"requests: official={client.requests} "
        f"unofficial={uo_client.requests if uo_client else 0}  "
        f"network: {client.network_seconds + (uo_client.network_seconds if uo_client else 0.0):.1f}s"
    )
    typer.echo(
        f"pages:    +{counts['new_pages']}  ~{counts['upd_pages']}\n"
        f"blocks:   +{counts['new_blocks']}  ~{counts['upd_blocks']}\n"
        f"comments: +{counts['new_comments']}  ~{counts['upd_comments']}"
    )


def main() -> None:
    typer.run(fetch)


if __name__ == "__main__":
    sys.exit(main() or 0)
