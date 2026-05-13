#!/usr/bin/env python3
"""Mirror Notion pages via the official `api.notion.com` API.

Sibling to `notion_web.py`, which talks to Notion's internal v3 web API and
is the only path with inbox access. This module talks to the public API —
it can't see the inbox, but it returns blocks in a stable, documented shape
(`rich_text` already structured, `child_page` first-class, `synced_block`
present, etc.), which is much friendlier to render to markdown.

The intended hybrid flow:
  - notion_web.py: walk getNotificationLog → discover page IDs (inbox only).
  - notion_official.py: take page IDs, fetch full content via official API.
  - ingest/render_notion_official.py: convert official-API blocks → markdown.

Auth: a `notion` latchkey service with a Bearer token (Notion internal
integration *or* Notion personal-access-token both work). See NOTION_AUTH.md
for setup — the personal-access-token route is simpler since it inherits
the user's permissions and doesn't require per-page integration grants.

Storage: two entities under the event store.
  - `notion_official_page` keyed by `id` (page UUID): one record per page.
  - `notion_official_block` keyed by `id` (block UUID): one record per block.
Both are kept separate from `notion_block` / `notion_page` (unofficial) so
the two paths can coexist during the migration.

Usage:
    uv run python -m download.notion_official --subtree <page_id>
"""

from __future__ import annotations

import json
import logging
import subprocess
import sys
import time
from collections import deque
from pathlib import Path
from typing import Any

import typer
from tqdm import tqdm

from event_store import (
    diff_and_save as _diff_and_save,
    load_latest_by_key as _load_latest_by_key,
    make_record as _make_record,
)

DEFAULT_OUT_DIR = Path.home() / "backups" / "notion"
BASE = "https://api.notion.com/v1"
LATCHKEY_TIMEOUT = 180
RETRY_MAX = 6
RETRY_INITIAL_BACKOFF = 2.0
RETRY_MAX_BACKOFF = 60.0
PAGE_SIZE = 100  # max allowed by official API

ENTITY_PAGE = "notion_official_page"
ENTITY_BLOCK = "notion_official_block"

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


# ---------------------------------------------------------------------------
# Sink: page / block dicts → JSONL event store
# ---------------------------------------------------------------------------


def _page_key(rec: dict) -> str:
    return rec["id"]


def _block_key(rec: dict) -> str:
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


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def _format_uuid(s: str) -> str:
    raw = s.replace("-", "")
    if len(raw) != 32:
        return s
    return f"{raw[0:8]}-{raw[8:12]}-{raw[12:16]}-{raw[16:20]}-{raw[20:32]}"


def fetch(
    subtree: str = typer.Option(
        ...,
        "--subtree",
        help="Root page id (UUID, dashed or undashed) to mirror.",
    ),
    out_dir: Path = typer.Option(
        DEFAULT_OUT_DIR,
        "--out-dir",
        help=f"Where to write JSONL streams (default {DEFAULT_OUT_DIR}).",
    ),
    max_pages: int = typer.Option(
        5000,
        "--max-pages",
        help="Safety bound on BFS page count. Default 5000.",
    ),
    verbose: bool = typer.Option(False, "--verbose", "-v"),
) -> None:
    """BFS-mirror a Notion subtree via the official API."""
    logging.basicConfig(
        level=logging.DEBUG if verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    out_dir = out_dir.expanduser()
    out_dir.mkdir(parents=True, exist_ok=True)

    root = _format_uuid(subtree)
    client = NotionOfficialClient()

    existing_pages = _load_latest_by_key(out_dir, ENTITY_PAGE, _page_key)
    existing_blocks = _load_latest_by_key(out_dir, ENTITY_BLOCK, _block_key)
    typer.echo(
        f"out: {out_dir}  existing: pages={len(existing_pages)} blocks={len(existing_blocks)}"
    )

    queue: deque[str] = deque([root])
    queued: set[str] = {root}
    visited: set[str] = set()

    pbar = tqdm(total=1, unit="pg", desc="fetch")
    new_pages = upd_pages = new_blocks = upd_blocks = 0

    while queue and len(visited) < max_pages:
        pid = queue.popleft()
        if pid in visited:
            continue
        visited.add(pid)
        pbar.set_postfix_str(pid[:8])

        # 1) Fetch the page record itself.
        try:
            page = client.get_page(pid)
        except NotionOfficialError as e:
            tqdm.write(f"  ! {pid}: {e}")
            pbar.update(1)
            continue
        n, u = _diff_and_save(
            out_dir,
            ENTITY_PAGE,
            [_page_record(page)],
            existing_pages,
            _page_key,
        )
        new_pages += n
        upd_pages += u
        existing_pages[pid] = _page_record(page)

        # 2) Walk blocks.
        try:
            blocks = _walk_page_blocks(client, pid)
        except NotionOfficialError as e:
            tqdm.write(f"  ! blocks {pid}: {e}")
            pbar.update(1)
            continue
        block_records = [_block_record(b, pid) for b in blocks]
        n, u = _diff_and_save(
            out_dir,
            ENTITY_BLOCK,
            block_records,
            existing_blocks,
            _block_key,
        )
        new_blocks += n
        upd_blocks += u
        for br in block_records:
            existing_blocks[br["id"]] = br

        # 3) Enqueue child_page descendants.
        for cid in _child_page_ids(blocks):
            if cid in queued or cid in visited:
                continue
            queue.append(cid)
            queued.add(cid)
            pbar.total += 1
            pbar.refresh()
        pbar.update(1)

    pbar.close()
    typer.echo(f"requests: {client.requests}  network: {client.network_seconds:.1f}s")
    typer.echo(
        f"pages:  +{new_pages}  ~{upd_pages}\nblocks: +{new_blocks}  ~{upd_blocks}"
    )


def main() -> None:
    typer.run(fetch)


if __name__ == "__main__":
    sys.exit(main() or 0)
