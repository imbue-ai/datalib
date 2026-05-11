#!/usr/bin/env python3
"""Incrementally fetch GitLab MRs and code-review comments to JSONL.

GitLab counterpart to download/github_web.py. Same per-entity event-store
shape: under `<out_dir>/<entity>/{created,updated}/events.jsonl`.

Entities written:
  self_identity — /user (one row, the authenticated user)
  merge_request — MR meta keyed by (project_path, mr_iid). Carries web_url,
                  source/target sha+ref, state, merged_at, etc.
  discussion    — MR discussions keyed by (project_path, mr_iid, discussion_id).
                  Each discussion is a thread; the threading is intrinsic to
                  the API shape (discussion.notes is the list of replies).
                  Position-anchored discussions carry `position.new_path`,
                  `position.new_line`, `position.head_sha`, etc. for line-level
                  diff comments. Free-form MR comments come back as singleton
                  discussions (one note per discussion).

Each discussion carries the MR's web_url so deep-link URLs follow GitLab's
convention: {mr.web_url}#note_{note.id}.

MR discovery: union of `scope=created_by_me`, `scope=assigned_to_me`, and
`reviewer_id={me}`. After the first run, the refresh window narrows the
list to MRs `updated_after={since}` so subsequent runs stay cheap.

Auth: assumes `latchkey curl` is configured for the `gitlab` service.
GitLab's API accepts the latchkey-injected `PRIVATE-TOKEN: <token>` header
directly.

Usage:
    uv run python -m download.gitlab_web                        # everything
    uv run python -m download.gitlab_web --refresh-window-days 7
    uv run python -m download.gitlab_web --max-mrs 50           # smoke test
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

DEFAULT_OUT_DIR = Path.home() / "backups" / "gitlab"
DEFAULT_REFRESH_WINDOW_DAYS = 30
LATCHKEY_TIMEOUT = 60
RATE_LIMIT_MAX_RETRIES = 7
RATE_LIMIT_INITIAL_BACKOFF = 2.0
RATE_LIMIT_MAX_BACKOFF = 60.0
GITLAB_API = "https://gitlab.com/api/v4"
PER_PAGE = 100  # GitLab's max for most list endpoints

logger = logging.getLogger("gitlab_web")


# ---------------------------------------------------------------------------
# Auth + transport: latchkey curl with retries + Link-header pagination.
# ---------------------------------------------------------------------------


class GitLabError(RuntimeError):
    pass


class _RateLimited(Exception):
    pass


_LINK_NEXT_RE = re.compile(r'<([^>]+)>;\s*rel="next"')


def _call_gitlab_once(url: str) -> tuple[Any, dict[str, str]]:
    proc = subprocess.run(
        ["latchkey", "curl", "-sS", "-D", "-", url],
        capture_output=True,
        text=True,
        timeout=LATCHKEY_TIMEOUT,
        check=False,
    )
    if proc.returncode != 0:
        raise GitLabError(
            f"latchkey curl exit={proc.returncode} stderr={proc.stderr[-200:]!r}"
        )
    head, _, body = proc.stdout.partition("\r\n\r\n")
    if not body:
        head, _, body = proc.stdout.partition("\n\n")
    headers: dict[str, str] = {}
    status = ""
    for i, line in enumerate(head.splitlines()):
        if i == 0:
            status = line
            continue
        if ":" in line:
            k, _, v = line.partition(":")
            headers[k.strip().lower()] = v.strip()
    parts = status.split(" ", 2)
    code = parts[1] if len(parts) > 1 else ""
    if code == "429":
        raise _RateLimited(headers.get("retry-after", ""))
    if not code.startswith("2"):
        raise GitLabError(f"{status.strip()} body={body[:200]!r} url={url}")
    try:
        data = json.loads(body) if body.strip() else None
    except json.JSONDecodeError as e:
        raise GitLabError(f"invalid JSON: {body[:200]!r}") from e
    return data, headers


def call_gitlab(url: str) -> tuple[Any, dict[str, str]]:
    backoff = RATE_LIMIT_INITIAL_BACKOFF
    for attempt in range(RATE_LIMIT_MAX_RETRIES + 1):
        try:
            return _call_gitlab_once(url)
        except _RateLimited as rl:
            sleep_for = backoff
            hint = str(rl)
            if hint.isdigit():
                sleep_for = max(backoff, float(hint))
            sleep_for = min(sleep_for, RATE_LIMIT_MAX_BACKOFF)
            if attempt == RATE_LIMIT_MAX_RETRIES:
                raise GitLabError(f"rate-limited after {attempt} retries")
            logger.warning(
                "rate-limited; sleeping %.0fs (attempt %d/%d)",
                sleep_for,
                attempt + 1,
                RATE_LIMIT_MAX_RETRIES,
            )
            time.sleep(sleep_for)
            backoff = min(backoff * 2, RATE_LIMIT_MAX_BACKOFF)
        except GitLabError as e:
            msg = str(e)
            if (
                "exit=7" in msg
                or "exit=28" in msg
                or "exit=35" in msg
                or "exit=56" in msg
            ):
                if attempt == RATE_LIMIT_MAX_RETRIES:
                    raise
                logger.warning("transient (%s); sleeping %.0fs", e, backoff)
                time.sleep(backoff)
                backoff = min(backoff * 2, RATE_LIMIT_MAX_BACKOFF)
            else:
                raise
    raise AssertionError("unreachable")


def paginate(url: str) -> list[Any]:
    """GitLab uses Link headers like GitHub. Walk rel=next until exhausted."""
    items: list[Any] = []
    while url:
        data, headers = call_gitlab(url)
        if isinstance(data, list):
            items.extend(data)
        else:
            return [data]
        link = headers.get("link", "")
        m = _LINK_NEXT_RE.search(link)
        url = m.group(1) if m else ""
    return items


# ---------------------------------------------------------------------------
# Event store: same shape as github_web.py / slack_web.py.
# ---------------------------------------------------------------------------

ENTITY_SELF = "self_identity"
ENTITY_MR = "merge_request"
ENTITY_DISCUSSION = "discussion"


# ---------------------------------------------------------------------------
# Per-entity key extractors.
# ---------------------------------------------------------------------------


def _key_self(rec: dict) -> int:
    return int(rec["user_id"])


def _key_mr(rec: dict) -> tuple[str, int]:
    return (rec["project_path"], int(rec["mr_iid"]))


def _key_discussion(rec: dict) -> tuple[str, int, str]:
    return (rec["project_path"], int(rec["mr_iid"]), str(rec["discussion_id"]))


# ---------------------------------------------------------------------------
# Fetch passes.
# ---------------------------------------------------------------------------


def fetch_self_identity(out_dir: Path) -> int:
    """Returns the user id so MR discovery can use it as a reviewer filter."""
    data, _ = call_gitlab(f"{GITLAB_API}/user")
    if not isinstance(data, dict):
        raise GitLabError(f"/user returned non-object: {type(data).__name__}")
    rec = _make_record(
        {
            "user_id": data["id"],
            "username": data.get("username"),
            "web_url": data.get("web_url"),
        },
        data,
    )
    existing = _load_latest_by_key(out_dir, ENTITY_SELF, _key_self)
    _diff_and_save(out_dir, ENTITY_SELF, [rec], existing, _key_self)
    return int(data["id"])


def discover_mrs(
    out_dir: Path, user_id: int, since: str | None
) -> list[tuple[str, int, int]]:
    """Union across created_by_me, assigned_to_me, reviewer_id=me.

    Returns (project_path_with_namespace, project_id, mr_iid). project_id is
    needed for the per-MR endpoints; project_path is the human-readable key.
    `since` is an ISO datetime — narrows results to MRs updated after.
    """
    seen: dict[tuple[str, int], tuple[str, int, int]] = {}
    queries: list[dict[str, str]] = [
        {"scope": "created_by_me", "state": "all", "per_page": str(PER_PAGE)},
        {"scope": "assigned_to_me", "state": "all", "per_page": str(PER_PAGE)},
        {
            "reviewer_id": str(user_id),
            "state": "all",
            "scope": "all",
            "per_page": str(PER_PAGE),
        },
    ]
    for params in queries:
        if since:
            params = dict(params, updated_after=since)
        url = f"{GITLAB_API}/merge_requests?{urlencode(params)}"
        logger.info("searching MRs %s", params)
        try:
            results = paginate(url)
        except GitLabError as e:
            logger.error("MR search %s failed: %s", params, e)
            continue
        for mr in results:
            iid = mr.get("iid")
            project_id = mr.get("project_id")
            refs = mr.get("references") or {}
            full_ref = refs.get("full") or ""  # e.g. "imbue-ai/foo!123"
            project_path = full_ref.split("!", 1)[0] if "!" in full_ref else ""
            if not project_path:
                # Fallback: derive from web_url if `references.full` is empty.
                wu = mr.get("web_url", "")
                m = re.match(r"https?://[^/]+/(.+)/-/merge_requests/\d+", wu)
                project_path = m.group(1) if m else f"id_{project_id}"
            if isinstance(iid, int) and isinstance(project_id, int):
                seen[(project_path, iid)] = (project_path, project_id, iid)
        logger.info("  → %d MRs", len(results))
    out = sorted(seen.values())
    logger.info("discovered %d unique MRs", len(out))
    return out


def fetch_mr_meta(project_id: int, iid: int, project_path: str) -> dict[str, Any]:
    data, _ = call_gitlab(f"{GITLAB_API}/projects/{project_id}/merge_requests/{iid}")
    if not isinstance(data, dict):
        raise GitLabError(f"MR {project_path}!{iid} returned non-object")
    diff_refs = data.get("diff_refs") or {}
    return _make_record(
        {
            "project_path": project_path,
            "project_id": project_id,
            "mr_iid": data["iid"],
            "web_url": data.get("web_url"),
            "state": data.get("state"),
            "merged_at": data.get("merged_at"),
            "source_branch": data.get("source_branch"),
            "target_branch": data.get("target_branch"),
            "sha": data.get("sha"),
            "head_sha": diff_refs.get("head_sha"),
            "base_sha": diff_refs.get("base_sha"),
            "start_sha": diff_refs.get("start_sha"),
        },
        data,
    )


def fetch_mr_discussions(
    project_id: int, iid: int, project_path: str, mr_web_url: str | None
) -> list[dict[str, Any]]:
    """Return one record per discussion (thread) on the MR.

    GitLab's `/discussions` endpoint groups notes by thread out of the box,
    so each row is already in threaded form. Position-anchored line comments
    have `notes[0].position` populated; free-form MR comments come back as
    singleton discussions.

    Each note's permalink is `{mr.web_url}#note_{note.id}` — we surface the
    first note's id at the top level so deep-linking is one string-format
    away in the UI.
    """
    url = (
        f"{GITLAB_API}/projects/{project_id}/merge_requests/{iid}/discussions"
        f"?per_page={PER_PAGE}"
    )
    out: list[dict[str, Any]] = []
    for d in paginate(url):
        notes = d.get("notes") or []
        first_note = notes[0] if notes else {}
        position = first_note.get("position") or {}
        out.append(
            _make_record(
                {
                    "project_path": project_path,
                    "project_id": project_id,
                    "mr_iid": iid,
                    "discussion_id": d.get("id"),
                    "individual_note": d.get("individual_note", False),
                    "first_note_id": first_note.get("id"),
                    "first_note_url": (
                        f"{mr_web_url}#note_{first_note['id']}"
                        if mr_web_url and first_note.get("id")
                        else None
                    ),
                    "note_count": len(notes),
                    "author_username": (first_note.get("author") or {}).get("username"),
                    "created_at": first_note.get("created_at"),
                    "updated_at": (notes[-1] if notes else {}).get("updated_at"),
                    "path": position.get("new_path") or position.get("old_path"),
                    "line": position.get("new_line") or position.get("old_line"),
                    "head_sha": position.get("head_sha"),
                    "base_sha": position.get("base_sha"),
                    "start_sha": position.get("start_sha"),
                },
                d,
            )
        )
    return out


# ---------------------------------------------------------------------------
# Top-level orchestration.
# ---------------------------------------------------------------------------


def fetch(
    out_dir: Path = DEFAULT_OUT_DIR,
    refresh_window_days: int = DEFAULT_REFRESH_WINDOW_DAYS,
    max_mrs: int | None = None,
) -> None:
    out_dir = out_dir.expanduser()
    out_dir.mkdir(parents=True, exist_ok=True)

    user_id = fetch_self_identity(out_dir)

    existing_mrs = _load_latest_by_key(out_dir, ENTITY_MR, _key_mr)
    since: str | None = None
    if existing_mrs and refresh_window_days > 0:
        since_dt = datetime.now(timezone.utc) - timedelta(days=refresh_window_days)
        # GitLab's `updated_after` accepts ISO 8601 datetime.
        since = since_dt.isoformat(timespec="seconds")

    mr_keys = discover_mrs(out_dir, user_id, since)
    if max_mrs is not None:
        mr_keys = mr_keys[:max_mrs]

    mr_records: list[dict[str, Any]] = []
    discussion_records: list[dict[str, Any]] = []
    for project_path, project_id, iid in tqdm(mr_keys, desc="mrs", unit="mr"):
        try:
            mr_rec = fetch_mr_meta(project_id, iid, project_path)
            mr_records.append(mr_rec)
            mr_web_url = mr_rec.get("web_url") or (mr_rec["raw"].get("web_url"))
            discussion_records.extend(
                fetch_mr_discussions(project_id, iid, project_path, mr_web_url)
            )
        except GitLabError as e:
            # Don't poison the run on a single MR (lost access, transient 5xx).
            logger.error("failed MR %s!%s: %s", project_path, iid, e)

    _diff_and_save(
        out_dir,
        ENTITY_MR,
        mr_records,
        _load_latest_by_key(out_dir, ENTITY_MR, _key_mr),
        _key_mr,
    )
    _diff_and_save(
        out_dir,
        ENTITY_DISCUSSION,
        discussion_records,
        _load_latest_by_key(out_dir, ENTITY_DISCUSSION, _key_discussion),
        _key_discussion,
    )

    logger.info("done. out_dir=%s", out_dir)


# ---------------------------------------------------------------------------
# CLI.
# ---------------------------------------------------------------------------


app = typer.Typer(add_completion=False, help=__doc__)


@app.command()
def main(
    out_dir: Path = typer.Option(DEFAULT_OUT_DIR, help="Where to write JSONL streams."),
    refresh_window_days: int = typer.Option(
        DEFAULT_REFRESH_WINDOW_DAYS,
        help="On a non-empty out_dir, only refetch MRs updated in the last N days.",
    ),
    max_mrs: int | None = typer.Option(
        None, help="Cap MR count (smoke-test convenience)."
    ),
    verbose: bool = typer.Option(False, "-v", "--verbose"),
) -> None:
    logging.basicConfig(
        level=logging.DEBUG if verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        stream=sys.stderr,
    )
    fetch(out_dir=out_dir, refresh_window_days=refresh_window_days, max_mrs=max_mrs)


if __name__ == "__main__":
    app()
