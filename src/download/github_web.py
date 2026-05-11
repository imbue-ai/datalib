#!/usr/bin/env python3
"""Incrementally fetch GitHub PRs and code-review comments to JSONL.

The GitHub counterpart to download/slack_web.py. Same per-entity event-store
shape: under `<out_dir>/<entity>/{created,updated}/events.jsonl`. `created`
is append-only first-sightings; `updated` captures every change. Tail
`updated` to get the latest snapshot, scan `created` for first-seen times.

Entities written:
  self_identity      — /user (one row, the authenticated user)
  pull_request       — PR meta keyed by (repo_full_name, pr_number). Carries
                       html_url, head/base sha+ref, state, merged_at, etc.
  issue_comment      — comments on the PR's "Conversation" tab. Threading is
                       linear (no parent_id at the API level).
  pr_review          — reviews keyed by (repo_full_name, pr_number, review_id).
                       Carries body + state (APPROVED / CHANGES_REQUESTED / …).
  pr_review_comment  — line-anchored diff comments. Threaded via
                       `in_reply_to_id`; top-level comments start a thread.
                       Carries `path`, `line`, `original_line`, `commit_id`,
                       `original_commit_id`, `html_url`, `pull_request_url`.

Every record has the original `html_url` so a "Open in GitHub" feature can
deep-link directly at the comment.

PR discovery: union of `is:pr+author:@me` and `is:pr+commenter:@me` via the
search-issues endpoint. After the first run, the refresh window re-scopes
the search to PRs `updated:>=<since>` so subsequent runs stay cheap.

Auth: assumes `latchkey curl` is configured for the `github` service.
GitHub's REST API accepts the latchkey-injected `Authorization: Bearer <token>`
directly — no cookie/Cloudflare workaround needed.

Usage:
    uv run python -m download.github_web                       # everything
    uv run python -m download.github_web --refresh-window-days 7
    uv run python -m download.github_web --max-prs 50          # smoke test
"""

from __future__ import annotations

import json
import logging
import re
import subprocess
import sys
import time
from datetime import datetime, timezone
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

DEFAULT_OUT_DIR = Path.home() / "backups" / "github"
DEFAULT_REFRESH_WINDOW_DAYS = 30
LATCHKEY_TIMEOUT = 60
RATE_LIMIT_MAX_RETRIES = 7
RATE_LIMIT_INITIAL_BACKOFF = 2.0
RATE_LIMIT_MAX_BACKOFF = 120.0
GITHUB_API = "https://api.github.com"
PER_PAGE = 100  # GitHub's max for most list endpoints

logger = logging.getLogger("github_web")


# ---------------------------------------------------------------------------
# Auth + transport: latchkey curl with rate-limit + transient-error retries.
# ---------------------------------------------------------------------------


class GitHubError(RuntimeError):
    pass


class _RateLimited(Exception):
    pass


_LINK_NEXT_RE = re.compile(r'<([^>]+)>;\s*rel="next"')


def _call_github_once(url: str) -> tuple[Any, dict[str, str]]:
    """Single GET. Returns (parsed_json, response_headers).

    Headers are needed for Link-header pagination and for X-RateLimit-Reset
    so we can sleep until the quota resets instead of guessing.
    """
    proc = subprocess.run(
        ["latchkey", "curl", "-sS", "-D", "-", url],
        capture_output=True,
        text=True,
        timeout=LATCHKEY_TIMEOUT,
        check=False,
    )
    if proc.returncode != 0:
        raise GitHubError(
            f"latchkey curl exit={proc.returncode} stderr={proc.stderr[-200:]!r}"
        )
    # `-D -` prepends headers to stdout. Split on the first blank line.
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
    if "403" in status or "429" in status:
        # Primary rate limit: x-ratelimit-remaining=0, retry after reset.
        # Secondary rate limit: retry-after header (seconds).
        raise _RateLimited(
            headers.get("retry-after") or headers.get("x-ratelimit-reset") or ""
        )
    if not status.split(" ", 2)[1].startswith("2"):
        raise GitHubError(f"{status.strip()} body={body[:200]!r} url={url}")
    try:
        data = json.loads(body) if body.strip() else None
    except json.JSONDecodeError as e:
        raise GitHubError(f"invalid JSON: {body[:200]!r}") from e
    return data, headers


def call_github(url: str) -> tuple[Any, dict[str, str]]:
    """GET with exponential backoff on rate-limit / transient failures."""
    backoff = RATE_LIMIT_INITIAL_BACKOFF
    for attempt in range(RATE_LIMIT_MAX_RETRIES + 1):
        try:
            return _call_github_once(url)
        except _RateLimited as rl:
            sleep_for = backoff
            hint = str(rl)
            if hint.isdigit():
                # Retry-After in seconds.
                sleep_for = max(backoff, float(hint))
            elif hint:
                # x-ratelimit-reset is a unix timestamp.
                try:
                    delta = float(hint) - time.time()
                    if delta > 0:
                        sleep_for = max(backoff, delta + 1)
                except ValueError:
                    pass
            sleep_for = min(sleep_for, RATE_LIMIT_MAX_BACKOFF)
            if attempt == RATE_LIMIT_MAX_RETRIES:
                raise GitHubError(f"rate-limited after {attempt} retries")
            logger.warning(
                "rate-limited; sleeping %.0fs (attempt %d/%d)",
                sleep_for,
                attempt + 1,
                RATE_LIMIT_MAX_RETRIES,
            )
            time.sleep(sleep_for)
            backoff = min(backoff * 2, RATE_LIMIT_MAX_BACKOFF)
        except GitHubError as e:
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
    """Walk Link-header pagination until rel=next is gone."""
    items: list[Any] = []
    while url:
        data, headers = call_github(url)
        if isinstance(data, dict) and "items" in data:
            # Search endpoints wrap results under `items`.
            items.extend(data["items"])
        elif isinstance(data, list):
            items.extend(data)
        else:
            return [data]
        link = headers.get("link", "")
        m = _LINK_NEXT_RE.search(link)
        url = m.group(1) if m else ""
    return items


# ---------------------------------------------------------------------------
# Event store: per-entity created/ + updated/ JSONL streams. Same shape as
# slack_web.py — intentionally so a future ingest pass can use one reader.
# ---------------------------------------------------------------------------

ENTITY_SELF = "self_identity"
ENTITY_PR = "pull_request"
ENTITY_ISSUE_COMMENT = "issue_comment"
ENTITY_PR_REVIEW = "pr_review"
ENTITY_PR_REVIEW_COMMENT = "pr_review_comment"


# ---------------------------------------------------------------------------
# Per-entity key extractors. Top-level fields are denormalized off `raw` so
# the JSONL is greppable without needing jq.
# ---------------------------------------------------------------------------


def _key_self(rec: dict) -> int:
    return int(rec["user_id"])


def _key_pr(rec: dict) -> tuple[str, int]:
    return (rec["repo_full_name"], int(rec["pr_number"]))


def _key_issue_comment(rec: dict) -> tuple[str, int]:
    return (rec["repo_full_name"], int(rec["comment_id"]))


def _key_pr_review(rec: dict) -> tuple[str, int]:
    return (rec["repo_full_name"], int(rec["review_id"]))


def _key_pr_review_comment(rec: dict) -> tuple[str, int]:
    return (rec["repo_full_name"], int(rec["comment_id"]))


def _repo_full_name_from_pr_url(pr_url: str) -> str:
    """Extract `owner/repo` from an api.github.com PR url."""
    # https://api.github.com/repos/owner/repo/pulls/123
    m = re.search(r"/repos/([^/]+/[^/]+)/", pr_url)
    if not m:
        raise GitHubError(f"unrecognized PR url: {pr_url!r}")
    return m.group(1)


# ---------------------------------------------------------------------------
# Discovery: search PRs the user authored or commented on.
# ---------------------------------------------------------------------------


def _search_prs(query: str, since: str | None) -> list[dict[str, Any]]:
    """Run a search-issues query, scoped to PRs only.

    `since` (ISO date) narrows the search to `updated:>=since` so refresh
    runs stay small. The search-issues API is a separate, lower rate limit
    (~30/min), so paginate carefully and use per_page=100.
    """
    full_q = f"is:pr {query}"
    if since:
        full_q += f" updated:>={since}"
    params = {
        "q": full_q,
        "per_page": str(PER_PAGE),
        "sort": "updated",
        "order": "desc",
    }
    url = f"{GITHUB_API}/search/issues?{urlencode(params)}"
    return paginate(url)


def discover_prs(out_dir: Path, since: str | None) -> list[tuple[str, int]]:
    """Union of (author:@me, commenter:@me). Returns (repo_full_name, number)."""
    seen: set[tuple[str, int]] = set()
    for who in ("author:@me", "commenter:@me"):
        logger.info("searching PRs %s%s", who, f" since {since}" if since else "")
        try:
            results = _search_prs(who, since)
        except GitHubError as e:
            logger.error("search %s failed: %s", who, e)
            continue
        for item in results:
            # search-issues returns the issue shape; pull_request.url is the
            # PR API url, repository_url + number give us repo+num.
            repo_url = item.get("repository_url", "")
            repo_full = repo_url.split("/repos/", 1)[-1] if repo_url else ""
            num = item.get("number")
            if repo_full and isinstance(num, int):
                seen.add((repo_full, num))
        logger.info("  → %d PRs", len(results))
    out = sorted(seen)
    logger.info("discovered %d unique PRs", len(out))
    return out


# ---------------------------------------------------------------------------
# Per-PR fetch passes.
# ---------------------------------------------------------------------------


def fetch_self_identity(out_dir: Path) -> None:
    data, _ = call_github(f"{GITHUB_API}/user")
    if not isinstance(data, dict):
        raise GitHubError(f"/user returned non-object: {type(data).__name__}")
    rec = _make_record(
        {
            "user_id": data["id"],
            "login": data.get("login"),
            "html_url": data.get("html_url"),
        },
        data,
    )
    existing = _load_latest_by_key(out_dir, ENTITY_SELF, _key_self)
    _diff_and_save(out_dir, ENTITY_SELF, [rec], existing, _key_self)


def fetch_pr_meta(out_dir: Path, repo_full: str, num: int) -> dict[str, Any]:
    data, _ = call_github(f"{GITHUB_API}/repos/{repo_full}/pulls/{num}")
    if not isinstance(data, dict):
        raise GitHubError(f"PR {repo_full}#{num} returned non-object")
    return _make_record(
        {
            "repo_full_name": repo_full,
            "pr_number": data["number"],
            "html_url": data.get("html_url"),
            "state": data.get("state"),
            "merged_at": data.get("merged_at"),
            "head_sha": (data.get("head") or {}).get("sha"),
            "head_ref": (data.get("head") or {}).get("ref"),
            "base_sha": (data.get("base") or {}).get("sha"),
            "base_ref": (data.get("base") or {}).get("ref"),
        },
        data,
    )


def fetch_issue_comments(
    out_dir: Path, repo_full: str, num: int
) -> list[dict[str, Any]]:
    """`/repos/{r}/issues/{n}/comments` covers PR-level (Conversation tab) comments."""
    url = f"{GITHUB_API}/repos/{repo_full}/issues/{num}/comments?per_page={PER_PAGE}"
    out: list[dict[str, Any]] = []
    for c in paginate(url):
        out.append(
            _make_record(
                {
                    "repo_full_name": repo_full,
                    "pr_number": num,
                    "comment_id": c["id"],
                    "html_url": c.get("html_url"),
                    "user_login": (c.get("user") or {}).get("login"),
                    "created_at": c.get("created_at"),
                    "updated_at": c.get("updated_at"),
                },
                c,
            )
        )
    return out


def fetch_pr_reviews(out_dir: Path, repo_full: str, num: int) -> list[dict[str, Any]]:
    url = f"{GITHUB_API}/repos/{repo_full}/pulls/{num}/reviews?per_page={PER_PAGE}"
    out: list[dict[str, Any]] = []
    for r in paginate(url):
        out.append(
            _make_record(
                {
                    "repo_full_name": repo_full,
                    "pr_number": num,
                    "review_id": r["id"],
                    "html_url": r.get("html_url"),
                    "user_login": (r.get("user") or {}).get("login"),
                    "state": r.get("state"),
                    "commit_id": r.get("commit_id"),
                    "submitted_at": r.get("submitted_at"),
                },
                r,
            )
        )
    return out


def fetch_pr_review_comments(
    out_dir: Path, repo_full: str, num: int
) -> list[dict[str, Any]]:
    """`/repos/{r}/pulls/{n}/comments` returns line-anchored diff comments.

    Threading: top-level comment has `in_reply_to_id == None`; replies set
    `in_reply_to_id` to the parent's id. We surface that field at the top
    level so the ingest pass can reconstruct trees without re-parsing raw.
    """
    url = f"{GITHUB_API}/repos/{repo_full}/pulls/{num}/comments?per_page={PER_PAGE}"
    out: list[dict[str, Any]] = []
    for c in paginate(url):
        out.append(
            _make_record(
                {
                    "repo_full_name": repo_full,
                    "pr_number": num,
                    "comment_id": c["id"],
                    "in_reply_to_id": c.get("in_reply_to_id"),
                    "pull_request_review_id": c.get("pull_request_review_id"),
                    "html_url": c.get("html_url"),
                    "user_login": (c.get("user") or {}).get("login"),
                    "path": c.get("path"),
                    "line": c.get("line"),
                    "original_line": c.get("original_line"),
                    "commit_id": c.get("commit_id"),
                    "original_commit_id": c.get("original_commit_id"),
                    "created_at": c.get("created_at"),
                    "updated_at": c.get("updated_at"),
                },
                c,
            )
        )
    return out


# ---------------------------------------------------------------------------
# Top-level orchestration.
# ---------------------------------------------------------------------------


def fetch(
    out_dir: Path = DEFAULT_OUT_DIR,
    refresh_window_days: int = DEFAULT_REFRESH_WINDOW_DAYS,
    max_prs: int | None = None,
) -> None:
    """Download identity + every authored/commented PR + its comments + reviews."""
    out_dir = out_dir.expanduser()
    out_dir.mkdir(parents=True, exist_ok=True)

    fetch_self_identity(out_dir)

    # If we've fetched before, narrow the search to PRs updated since
    # (now - refresh_window). On first run, no `since` → full backfill.
    existing_prs = _load_latest_by_key(out_dir, ENTITY_PR, _key_pr)
    since: str | None = None
    if existing_prs and refresh_window_days > 0:
        since_dt = datetime.now(timezone.utc) - _td_days(refresh_window_days)
        since = since_dt.date().isoformat()

    pr_keys = discover_prs(out_dir, since)
    if max_prs is not None:
        pr_keys = pr_keys[:max_prs]

    pr_records: list[dict[str, Any]] = []
    issue_comment_records: list[dict[str, Any]] = []
    review_records: list[dict[str, Any]] = []
    review_comment_records: list[dict[str, Any]] = []

    for repo_full, num in tqdm(pr_keys, desc="prs", unit="pr"):
        try:
            pr_records.append(fetch_pr_meta(out_dir, repo_full, num))
            issue_comment_records.extend(fetch_issue_comments(out_dir, repo_full, num))
            review_records.extend(fetch_pr_reviews(out_dir, repo_full, num))
            review_comment_records.extend(
                fetch_pr_review_comments(out_dir, repo_full, num)
            )
        except GitHubError as e:
            # One bad PR (e.g. lost access to a private repo) shouldn't
            # poison the whole run. Log and move on.
            logger.error("failed PR %s#%s: %s", repo_full, num, e)

    _diff_and_save(
        out_dir,
        ENTITY_PR,
        pr_records,
        _load_latest_by_key(out_dir, ENTITY_PR, _key_pr),
        _key_pr,
    )
    _diff_and_save(
        out_dir,
        ENTITY_ISSUE_COMMENT,
        issue_comment_records,
        _load_latest_by_key(out_dir, ENTITY_ISSUE_COMMENT, _key_issue_comment),
        _key_issue_comment,
    )
    _diff_and_save(
        out_dir,
        ENTITY_PR_REVIEW,
        review_records,
        _load_latest_by_key(out_dir, ENTITY_PR_REVIEW, _key_pr_review),
        _key_pr_review,
    )
    _diff_and_save(
        out_dir,
        ENTITY_PR_REVIEW_COMMENT,
        review_comment_records,
        _load_latest_by_key(out_dir, ENTITY_PR_REVIEW_COMMENT, _key_pr_review_comment),
        _key_pr_review_comment,
    )

    logger.info("done. out_dir=%s", out_dir)


def _td_days(n: int):
    from datetime import timedelta

    return timedelta(days=n)


# ---------------------------------------------------------------------------
# CLI.
# ---------------------------------------------------------------------------


app = typer.Typer(add_completion=False, help=__doc__)


@app.command()
def main(
    out_dir: Path = typer.Option(DEFAULT_OUT_DIR, help="Where to write JSONL streams."),
    refresh_window_days: int = typer.Option(
        DEFAULT_REFRESH_WINDOW_DAYS,
        help="On a non-empty out_dir, only refetch PRs updated in the last N days.",
    ),
    max_prs: int | None = typer.Option(
        None, help="Cap PR count (smoke-test convenience)."
    ),
    verbose: bool = typer.Option(False, "-v", "--verbose"),
) -> None:
    logging.basicConfig(
        level=logging.DEBUG if verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        stream=sys.stderr,
    )
    fetch(out_dir=out_dir, refresh_window_days=refresh_window_days, max_prs=max_prs)


if __name__ == "__main__":
    app()
