"""Parse a GitLab-API events directory into in-memory `ParsedGitlabApi`.

Layout:
    {self_identity,merge_request,discussion}/{created,updated}/events.jsonl

GitLab discussions are natively threaded: each `discussion` event already
carries a list of notes in `raw.notes[]`. We treat one discussion as one
*thread* — diff-anchored when `individual_note: false` and a `path`/`line`
is present, otherwise general (general thread is shared across all
individual_note discussions on a given MR).
"""

from __future__ import annotations

import json
import uuid as uuid_lib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

GITLAB_UUID_NS = uuid_lib.UUID("c2b91d4b-2080-5e5c-ab34-8f4f3c9e0002")


def gitlab_mr_uuid(project: str, iid: int | str) -> str:
    return str(uuid_lib.uuid5(GITLAB_UUID_NS, f"gitlab:{project}:mr:{iid}"))


def gitlab_note_uuid(project: str, note_id: int | str) -> str:
    return str(uuid_lib.uuid5(GITLAB_UUID_NS, f"gitlab:{project}:note:{note_id}"))


def gitlab_thread_uuid(project: str, mr_iid: int | str, key: str) -> str:
    return str(
        uuid_lib.uuid5(GITLAB_UUID_NS, f"gitlab:{project}:thread:{mr_iid}:{key}")
    )


@dataclass
class GitlabSelfIdentity:
    user_id: int | str | None
    login: str | None
    name: str | None
    html_url: str | None
    email: str | None
    raw_json: dict[str, Any]


@dataclass
class MergeRequestRow:
    uuid: str
    project_path: str
    mr_iid: int
    title: str
    description: str
    state: str | None
    web_url: str | None
    head_sha: str | None
    base_sha: str | None
    source_branch: str | None
    target_branch: str | None
    author_username: str | None
    created_at: str | None
    updated_at: str | None
    merged_at: str | None
    raw_json: dict[str, Any]


@dataclass
class NoteRow:
    uuid: str
    project_path: str
    mr_iid: int
    thread_uuid: str
    thread_key: str  # "general" or "{path}:{line}"
    kind: str  # GitLab Discussion Note
    external_id: str
    discussion_id: str
    user_login: str | None
    body: str
    web_url: str | None
    path: str | None
    line: int | None
    commit_sha: str | None
    created_at: str
    updated_at: str | None
    raw_json: dict[str, Any]


@dataclass
class ParsedGitlabApi:
    self_identity: GitlabSelfIdentity | None = None
    merge_requests: list[MergeRequestRow] = field(default_factory=list)
    notes: list[NoteRow] = field(default_factory=list)


def _read_jsonl(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def parse_api_dir(api_dir: Path) -> ParsedGitlabApi:
    api_dir = Path(api_dir)
    out = ParsedGitlabApi()

    for ev in _read_jsonl(api_dir / "self_identity" / "created" / "events.jsonl"):
        raw = ev.get("raw") or {}
        out.self_identity = GitlabSelfIdentity(
            user_id=ev.get("user_id") or raw.get("id"),
            login=ev.get("login") or raw.get("username"),
            name=raw.get("name"),
            html_url=ev.get("html_url") or raw.get("web_url"),
            email=raw.get("email"),
            raw_json=raw,
        )

    for ev in _read_jsonl(api_dir / "merge_request" / "created" / "events.jsonl"):
        raw = ev.get("raw") or {}
        project = ev.get("project_path") or ""
        iid = ev.get("mr_iid") or raw.get("iid")
        if not project or iid is None:
            continue
        out.merge_requests.append(
            MergeRequestRow(
                uuid=gitlab_mr_uuid(project, iid),
                project_path=project,
                mr_iid=int(iid),
                title=raw.get("title") or "",
                description=raw.get("description") or "",
                state=ev.get("state") or raw.get("state"),
                web_url=ev.get("web_url") or raw.get("web_url"),
                head_sha=ev.get("head_sha") or raw.get("sha"),
                base_sha=ev.get("base_sha"),
                source_branch=ev.get("source_branch") or raw.get("source_branch"),
                target_branch=ev.get("target_branch") or raw.get("target_branch"),
                author_username=(raw.get("author") or {}).get("username"),
                created_at=raw.get("created_at"),
                updated_at=raw.get("updated_at"),
                merged_at=ev.get("merged_at") or raw.get("merged_at"),
                raw_json=raw,
            )
        )

    for ev in _read_jsonl(api_dir / "discussion" / "created" / "events.jsonl"):
        raw = ev.get("raw") or {}
        project = ev.get("project_path") or ""
        iid = ev.get("mr_iid")
        discussion_id = ev.get("discussion_id") or raw.get("id") or ""
        if not project or iid is None or not discussion_id:
            continue
        individual_note = bool(
            ev.get("individual_note")
            if ev.get("individual_note") is not None
            else raw.get("individual_note")
        )
        # Thread key: general (all individual_notes share a single thread)
        # or "path:line" for diff-anchored discussions.
        path = ev.get("path")
        line = ev.get("line")
        if individual_note or not path:
            key = "general"
        else:
            key = f"{path}:{line or 0}"
        thread_uuid = gitlab_thread_uuid(project, iid, key)
        for note in raw.get("notes") or []:
            nid = note.get("id")
            if nid is None:
                continue
            position = note.get("position") or {}
            out.notes.append(
                NoteRow(
                    uuid=gitlab_note_uuid(project, nid),
                    project_path=project,
                    mr_iid=int(iid),
                    thread_uuid=thread_uuid,
                    thread_key=key,
                    kind="GitLab Discussion Note",
                    external_id=str(nid),
                    discussion_id=str(discussion_id),
                    user_login=(note.get("author") or {}).get("username"),
                    body=note.get("body") or "",
                    web_url=ev.get("first_note_url"),
                    path=position.get("new_path") or position.get("old_path") or path,
                    line=position.get("new_line") or position.get("old_line") or line,
                    commit_sha=position.get("head_sha") or ev.get("head_sha"),
                    created_at=note.get("created_at") or ev.get("created_at") or "",
                    updated_at=note.get("updated_at"),
                    raw_json=note,
                )
            )

    return out
