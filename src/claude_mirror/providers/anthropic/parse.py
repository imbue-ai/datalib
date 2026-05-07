from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


@dataclass
class AccountRow:
    account_uuid: str
    email: str | None
    full_name: str | None
    raw_json: dict[str, Any]


@dataclass
class ProjectRow:
    account_uuid: str
    project_uuid: str
    name: str | None
    description: str | None
    is_starter: bool | None
    created_at: str | None
    updated_at: str | None
    raw_json: dict[str, Any]


@dataclass
class ConversationRow:
    account_uuid: str
    conversation_uuid: str
    project_uuid: str | None
    name: str | None
    summary: str | None
    created_at: str | None
    updated_at: str | None
    raw_json: dict[str, Any]


@dataclass
class MessageRow:
    conversation_uuid: str
    message_uuid: str
    parent_message_uuid: str | None
    sender: str | None
    text: str | None
    created_at: str | None
    updated_at: str | None
    raw_json: dict[str, Any]


@dataclass
class ContentBlockRow:
    message_uuid: str
    block_index: int
    type: str | None
    text: str | None
    start_timestamp: str | None
    stop_timestamp: str | None
    raw_json: dict[str, Any]


@dataclass
class AttachmentRow:
    message_uuid: str
    attachment_index: int
    kind: str  # "attachment" or "file"
    raw_json: dict[str, Any]


@dataclass
class ParsedExport:
    accounts: list[AccountRow] = field(default_factory=list)
    projects: list[ProjectRow] = field(default_factory=list)
    conversations: list[ConversationRow] = field(default_factory=list)
    messages: list[MessageRow] = field(default_factory=list)
    content_blocks: list[ContentBlockRow] = field(default_factory=list)
    attachments: list[AttachmentRow] = field(default_factory=list)


def parse_export(export_dir: Path) -> ParsedExport:
    export_dir = Path(export_dir)
    out = ParsedExport()

    users_path = export_dir / "users.json"
    if not users_path.exists():
        raise FileNotFoundError(f"missing users.json in {export_dir}")
    users = json.loads(users_path.read_text())
    if not isinstance(users, list):
        raise ValueError("users.json must be a list")
    for u in users:
        out.accounts.append(
            AccountRow(
                account_uuid=u["uuid"],
                email=u.get("email_address"),
                full_name=u.get("full_name"),
                raw_json=u,
            )
        )

    projects_dir = export_dir / "projects"
    if projects_dir.is_dir():
        for f in sorted(projects_dir.glob("*.json")):
            p = json.loads(f.read_text())
            creator = p.get("creator") or {}
            out.projects.append(
                ProjectRow(
                    account_uuid=creator.get("uuid", ""),
                    project_uuid=p["uuid"],
                    name=p.get("name"),
                    description=p.get("description"),
                    is_starter=p.get("is_starter_project"),
                    created_at=p.get("created_at"),
                    updated_at=p.get("updated_at"),
                    raw_json=p,
                )
            )

    convs_path = export_dir / "conversations.json"
    if not convs_path.exists():
        raise FileNotFoundError(f"missing conversations.json in {export_dir}")
    convs = json.loads(convs_path.read_text())
    if not isinstance(convs, list):
        raise ValueError("conversations.json must be a list")
    for c in convs:
        account_uuid = (c.get("account") or {}).get("uuid", "")
        out.conversations.append(
            ConversationRow(
                account_uuid=account_uuid,
                conversation_uuid=c["uuid"],
                project_uuid=(c.get("project") or {}).get("uuid") if c.get("project") else None,
                name=c.get("name"),
                summary=c.get("summary"),
                created_at=c.get("created_at"),
                updated_at=c.get("updated_at"),
                raw_json={k: v for k, v in c.items() if k != "chat_messages"},
            )
        )
        for m in c.get("chat_messages", []) or []:
            out.messages.append(
                MessageRow(
                    conversation_uuid=c["uuid"],
                    message_uuid=m["uuid"],
                    parent_message_uuid=m.get("parent_message_uuid"),
                    sender=m.get("sender"),
                    text=m.get("text"),
                    created_at=m.get("created_at"),
                    updated_at=m.get("updated_at"),
                    raw_json={k: v for k, v in m.items() if k not in ("content", "attachments", "files")},
                )
            )
            for i, blk in enumerate(m.get("content") or []):
                out.content_blocks.append(
                    ContentBlockRow(
                        message_uuid=m["uuid"],
                        block_index=i,
                        type=blk.get("type"),
                        text=blk.get("text"),
                        start_timestamp=blk.get("start_timestamp"),
                        stop_timestamp=blk.get("stop_timestamp"),
                        raw_json=blk,
                    )
                )
            atch_idx = 0
            for a in m.get("attachments") or []:
                out.attachments.append(
                    AttachmentRow(
                        message_uuid=m["uuid"],
                        attachment_index=atch_idx,
                        kind="attachment",
                        raw_json=a,
                    )
                )
                atch_idx += 1
            for f in m.get("files") or []:
                out.attachments.append(
                    AttachmentRow(
                        message_uuid=m["uuid"],
                        attachment_index=atch_idx,
                        kind="file",
                        raw_json=f,
                    )
                )
                atch_idx += 1
    return out
