"""Parse a GitHub-API events directory into in-memory `ParsedGithubApi`.

Layout mirrors the other providers:
    {self_identity,pull_request,issue_comment,pr_review,pr_review_comment}/
        {created,updated}/events.jsonl

We read only the `created` streams — they are the cumulative superset.

Thread grouping (per user spec):
  * One *general* thread per PR — collects issue_comments (PR-wide chat)
    and pr_review summary bodies (the review-level body, distinct from
    inline diff comments).
  * One *diff* thread per (file path, line) within a PR — collects every
    pr_review_comment anchored at that location, including reply chains.
"""

from __future__ import annotations

import json
import uuid as uuid_lib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

GITHUB_UUID_NS = uuid_lib.UUID("b1a90c3a-1f7f-5d4b-9a23-7e3f2b8d0001")


def github_pr_uuid(repo: str, number: int | str) -> str:
    return str(uuid_lib.uuid5(GITHUB_UUID_NS, f"github:{repo}:pr:{number}"))


def github_issue_comment_uuid(repo: str, comment_id: int | str) -> str:
    return str(
        uuid_lib.uuid5(GITHUB_UUID_NS, f"github:{repo}:issue_comment:{comment_id}")
    )


def github_review_uuid(repo: str, review_id: int | str) -> str:
    return str(uuid_lib.uuid5(GITHUB_UUID_NS, f"github:{repo}:pr_review:{review_id}"))


def github_review_comment_uuid(repo: str, comment_id: int | str) -> str:
    return str(
        uuid_lib.uuid5(GITHUB_UUID_NS, f"github:{repo}:pr_review_comment:{comment_id}")
    )


def github_thread_uuid(repo: str, pr_number: int | str, key: str) -> str:
    return str(
        uuid_lib.uuid5(GITHUB_UUID_NS, f"github:{repo}:thread:{pr_number}:{key}")
    )


@dataclass
class GithubSelfIdentity:
    user_id: int | str | None
    login: str | None
    name: str | None
    html_url: str | None
    email: str | None
    raw_json: dict[str, Any]


@dataclass
class PullRequestRow:
    uuid: str
    repo_full_name: str
    pr_number: int
    title: str
    body: str
    state: str | None
    html_url: str | None
    head_sha: str | None
    base_sha: str | None
    head_ref: str | None
    base_ref: str | None
    user_login: str | None
    created_at: str | None
    updated_at: str | None
    merged_at: str | None
    raw_json: dict[str, Any]


@dataclass
class CommentRow:
    """Unified row for issue/review-summary/review-comment bodies.

    `thread_uuid` groups comments into PR-scoped threads:
      * general   → issue_comments + pr_review summary bodies
      * file:line → pr_review_comments at that diff anchor

    `kind` matches the grid `kind` column.
    """

    uuid: str
    repo_full_name: str
    pr_number: int
    thread_uuid: str
    thread_key: str  # "general" or "{path}:{line}"
    kind: str  # GitHub PR Comment | GitHub Review | GitHub Review Comment
    external_id: str
    in_reply_to_id: int | str | None
    user_login: str | None
    body: str
    html_url: str | None
    path: str | None
    line: int | None
    commit_id: str | None
    created_at: str
    updated_at: str | None
    raw_json: dict[str, Any]


@dataclass
class ParsedGithubApi:
    self_identity: GithubSelfIdentity | None = None
    pull_requests: list[PullRequestRow] = field(default_factory=list)
    comments: list[CommentRow] = field(default_factory=list)


def _read_jsonl(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def parse_api_dir(api_dir: Path) -> ParsedGithubApi:
    api_dir = Path(api_dir)
    out = ParsedGithubApi()

    # self_identity — typically one row; last wins.
    for ev in _read_jsonl(api_dir / "self_identity" / "created" / "events.jsonl"):
        raw = ev.get("raw") or {}
        out.self_identity = GithubSelfIdentity(
            user_id=ev.get("user_id") or raw.get("id"),
            login=ev.get("login") or raw.get("login"),
            name=raw.get("name"),
            html_url=ev.get("html_url") or raw.get("html_url"),
            email=raw.get("email"),
            raw_json=raw,
        )

    # Pull requests
    for ev in _read_jsonl(api_dir / "pull_request" / "created" / "events.jsonl"):
        raw = ev.get("raw") or {}
        repo = ev.get("repo_full_name") or ""
        number = ev.get("pr_number") or raw.get("number")
        if not repo or number is None:
            continue
        out.pull_requests.append(
            PullRequestRow(
                uuid=github_pr_uuid(repo, number),
                repo_full_name=repo,
                pr_number=int(number),
                title=raw.get("title") or "",
                body=raw.get("body") or "",
                state=ev.get("state") or raw.get("state"),
                html_url=ev.get("html_url") or raw.get("html_url"),
                head_sha=ev.get("head_sha") or (raw.get("head") or {}).get("sha"),
                base_sha=ev.get("base_sha") or (raw.get("base") or {}).get("sha"),
                head_ref=ev.get("head_ref") or (raw.get("head") or {}).get("ref"),
                base_ref=ev.get("base_ref") or (raw.get("base") or {}).get("ref"),
                user_login=(raw.get("user") or {}).get("login"),
                created_at=raw.get("created_at"),
                updated_at=raw.get("updated_at"),
                merged_at=ev.get("merged_at") or raw.get("merged_at"),
                raw_json=raw,
            )
        )

    # Issue comments → general thread
    for ev in _read_jsonl(api_dir / "issue_comment" / "created" / "events.jsonl"):
        raw = ev.get("raw") or {}
        repo = ev.get("repo_full_name") or ""
        number = ev.get("pr_number")
        comment_id = ev.get("comment_id") or raw.get("id")
        if not repo or number is None or comment_id is None:
            continue
        out.comments.append(
            CommentRow(
                uuid=github_issue_comment_uuid(repo, comment_id),
                repo_full_name=repo,
                pr_number=int(number),
                thread_uuid=github_thread_uuid(repo, number, "general"),
                thread_key="general",
                kind="GitHub PR Comment",
                external_id=str(comment_id),
                in_reply_to_id=None,
                user_login=ev.get("user_login") or (raw.get("user") or {}).get("login"),
                body=raw.get("body") or "",
                html_url=ev.get("html_url") or raw.get("html_url"),
                path=None,
                line=None,
                commit_id=None,
                created_at=ev.get("created_at") or raw.get("created_at") or "",
                updated_at=ev.get("updated_at") or raw.get("updated_at"),
                raw_json=raw,
            )
        )

    # PR review summaries → general thread (only if there's a body)
    for ev in _read_jsonl(api_dir / "pr_review" / "created" / "events.jsonl"):
        raw = ev.get("raw") or {}
        repo = ev.get("repo_full_name") or ""
        number = ev.get("pr_number")
        review_id = ev.get("review_id") or raw.get("id")
        if not repo or number is None or review_id is None:
            continue
        body = raw.get("body") or ""
        state = ev.get("state") or raw.get("state") or ""
        # Always emit a row so the review state (APPROVED, etc.) is visible.
        out.comments.append(
            CommentRow(
                uuid=github_review_uuid(repo, review_id),
                repo_full_name=repo,
                pr_number=int(number),
                thread_uuid=github_thread_uuid(repo, number, "general"),
                thread_key="general",
                kind="GitHub Review",
                external_id=str(review_id),
                in_reply_to_id=None,
                user_login=ev.get("user_login") or (raw.get("user") or {}).get("login"),
                body=body or f"({state.lower()})",
                html_url=ev.get("html_url") or raw.get("html_url"),
                path=None,
                line=None,
                commit_id=ev.get("commit_id") or raw.get("commit_id"),
                created_at=ev.get("submitted_at") or raw.get("submitted_at") or "",
                updated_at=None,
                raw_json=raw,
            )
        )

    # PR review comments → per (path, line) thread. Replies inherit the
    # thread of their in_reply_to root.
    review_comments_raw: list[dict[str, Any]] = list(
        _read_jsonl(api_dir / "pr_review_comment" / "created" / "events.jsonl")
    )
    # First pass: index roots (in_reply_to_id is null) by comment_id.
    root_thread_keys: dict[str, str] = {}
    for ev in review_comments_raw:
        cid = ev.get("comment_id") or (ev.get("raw") or {}).get("id")
        if ev.get("in_reply_to_id") is None and cid is not None:
            path = ev.get("path") or "unknown"
            line = ev.get("line") or ev.get("original_line") or 0
            root_thread_keys[str(cid)] = f"{path}:{line}"
    # Second pass: emit, resolving thread via in_reply_to_id when present.
    for ev in review_comments_raw:
        raw = ev.get("raw") or {}
        repo = ev.get("repo_full_name") or ""
        number = ev.get("pr_number")
        cid = ev.get("comment_id") or raw.get("id")
        if not repo or number is None or cid is None:
            continue
        in_reply = ev.get("in_reply_to_id")
        if in_reply is not None and str(in_reply) in root_thread_keys:
            key = root_thread_keys[str(in_reply)]
        else:
            path = ev.get("path") or "unknown"
            line = ev.get("line") or ev.get("original_line") or 0
            key = f"{path}:{line}"
        out.comments.append(
            CommentRow(
                uuid=github_review_comment_uuid(repo, cid),
                repo_full_name=repo,
                pr_number=int(number),
                thread_uuid=github_thread_uuid(repo, number, key),
                thread_key=key,
                kind="GitHub Review Comment",
                external_id=str(cid),
                in_reply_to_id=in_reply,
                user_login=ev.get("user_login") or (raw.get("user") or {}).get("login"),
                body=raw.get("body") or "",
                html_url=ev.get("html_url") or raw.get("html_url"),
                path=ev.get("path"),
                line=ev.get("line") or ev.get("original_line"),
                commit_id=ev.get("commit_id") or ev.get("original_commit_id"),
                created_at=ev.get("created_at") or raw.get("created_at") or "",
                updated_at=ev.get("updated_at") or raw.get("updated_at"),
                raw_json=raw,
            )
        )

    return out
