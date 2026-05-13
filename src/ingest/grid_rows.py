"""Build the `grid_rows` union projection from parsed provider data.

`grid_rows` is the only structured table that survives ingest — it backs
the AG Grid in frankweiler. Per-provider tables no longer exist; this
module reads the in-memory `Parsed*` dataclasses directly.

Schema (column names, types, per-provider mappings) lives in
`schemas/grid_rows.schema.json` — codegen produces matching Python /
Rust / TypeScript artifacts. See `docs/grid_rows.md` for the
architecture overview.

Re-population strategy: full delete + reinsert on every ingest. Cheap at
our scale (~5k rows), avoids row-level UPSERT complexity, and guarantees
consistency with any mapping changes.
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timedelta
from typing import Iterable, Mapping

from pymysql.connections import Connection

from ingest.generated_grid_rows import COLUMNS, DDL, MAX_LENGTHS
from ingest.providers.anthropic.parse import ParsedExport
from ingest.providers.github.parse import ParsedGithubApi
from ingest.providers.gitlab.parse import ParsedGitlabApi
from ingest.providers.notion.parse import (
    ParsedNotionWeb,
    notion_heading_uuid,
    notion_ms_to_iso,
    notion_url,
    rich_text_to_plain,
)
from ingest.providers.openai.parse import ParsedChatGPTApi
from ingest.providers.slack.mrkdwn import resolve_user_mentions
from ingest.providers.slack.parse import ParsedSlackApi
from ingest.render import (
    _github_pr_dir,
    _gitlab_mr_dir,
    _notion_page_qmd_path,
    _notion_page_titles,
    _notion_thread_qmd_path,
    _notion_thread_page_id,
    _slugify,
)

_GRID_ROWS_COLUMNS = COLUMNS["grid_rows"]
_GRID_ROWS_MAX_LENGTHS = MAX_LENGTHS["grid_rows"]


def _truncate_for_column(col: str, value):
    """Clip `value` to the column's VARCHAR(N) limit so an oversized field
    (e.g. a ChatGPT conversation auto-titled with the full first user
    message) doesn't blow up the whole insert. Non-string and unbounded
    columns pass through unchanged."""
    limit = _GRID_ROWS_MAX_LENGTHS.get(col)
    if limit is None or not isinstance(value, str) or len(value) <= limit:
        return value
    ellipsis = "…"
    return value[: max(0, limit - len(ellipsis))] + ellipsis


def ensure_schema(conn: Connection) -> None:
    """(Re)create the grid_rows table. Drops first so schema changes
    (new columns, retyped columns) take effect even when the underlying
    Dolt repo persists between ingest runs — grid_rows is fully derived
    from the parsed provider data, so dropping is always safe."""
    with conn.cursor() as cur:
        cur.execute("DROP TABLE IF EXISTS grid_rows")
        for stmt in DDL:
            cur.execute(stmt)


@dataclass(slots=True)
class _Row:
    uuid: str
    provider: str
    kind: str
    source_label: str
    when_ts: str
    author: str | None
    account: str | None
    project: str | None
    channel: str | None
    conversation_name: str | None
    conversation_uuid: str
    message_index: int | None
    entire_chat: str
    text: str
    slack_link: str | None
    qmd_path: str | None
    source_url: str | None = None
    git_sha: str | None = None
    external_id: str | None = None
    notion_page_uuid: str | None = None
    notion_block_uuid: str | None = None
    document_uuid: str | None = None


def _bump_micros(ts: str, n: int) -> str:
    """Add `n` microseconds to an ISO-8601 timestamp string, preserving
    the explicit offset suffix. Falls back to returning the input
    unchanged if the format isn't recognized — synthetic ordering is
    best-effort, matching the Rust `bump_micros` in db.rs."""
    if not ts:
        return ts
    s = ts.replace("Z", "+00:00") if ts.endswith("Z") else ts
    try:
        dt = datetime.fromisoformat(s)
    except ValueError:
        return ts
    bumped = dt + timedelta(microseconds=n)
    return bumped.isoformat(timespec="microseconds")


def _anthropic_kind_for_sender(sender: str) -> str:
    s = (sender or "").lower()
    if s in ("human", "user"):
        return "User Input"
    if s == "assistant":
        return "LLM Response"
    return "Tool Call"


def _anthropic_kind_for_block(block_type: str) -> str:
    return "LLM Thinking" if block_type == "thinking" else "Tool Call"


def _openai_kind_for_role_and_type(role: str, content_type: str) -> str:
    r = (role or "").lower()
    if r == "user":
        return "User Input"
    if r == "assistant":
        if content_type in ("thoughts", "reasoning_recap"):
            return "LLM Thinking"
        return "LLM Response"
    return "Tool Call"


# ----- qmd path computation -------------------------------------------------
#
# The grid row points the preview pane at a specific QMD on disk by carrying
# the file's path (relative to the data root) in `qmd_path`. We compute the
# path by mirroring exactly what the renderer in `ingest/render.py` writes,
# so every grid row knows the file without globbing or frontmatter scanning.


def _anthropic_qmd_path(
    account_uuid: str | None, conversation_uuid: str, name: str | None
) -> str:
    return (
        f"anthropic/{account_uuid}/llm_chats/{conversation_uuid}__{_slugify(name)}.qmd"
    )


def _openai_qmd_path(
    account_id: str | None, conversation_id: str, title: str | None
) -> str:
    return f"openai/{account_id or 'unknown'}/llm_chats/{conversation_id}__{_slugify(title)}.qmd"


def _slack_thread_title(
    root_text: str | None,
    user_labels: Mapping[str, str] | None = None,
) -> str:
    text = resolve_user_mentions(root_text or "", user_labels)
    snippet = text.strip().splitlines()
    title = snippet[0] if snippet else "(empty thread)"
    return title[:80]


def _slack_qmd_path(
    team_id: str,
    channel_name: str,
    thread_uuid: str,
    root_text: str | None,
    user_labels: Mapping[str, str] | None = None,
) -> str:
    return f"slack/{team_id}/{channel_name}/threads/{thread_uuid}__{_slugify(_slack_thread_title(root_text, user_labels))}.qmd"


def _slack_link(
    team_id: str, channel_id: str, ts: str, thread_ts: str | None = None
) -> str:
    ts_no_dot = ts.replace(".", "")
    url = f"https://slack.com/archives/{channel_id}/p{ts_no_dot}?team={team_id}"
    if thread_ts is not None and thread_ts != ts:
        url += f"&thread_ts={thread_ts}&cid={channel_id}"
    return url


# ----- Anthropic ------------------------------------------------------------


def _anthropic_rows(parsed: ParsedExport) -> Iterable[_Row]:
    convs = {c.conversation_uuid: c for c in parsed.conversations}
    msgs_by_conv: dict[str, list] = {}
    for m in parsed.messages:
        msgs_by_conv.setdefault(m.conversation_uuid, []).append(m)
    blocks_by_msg: dict[str, list] = {}
    for b in parsed.content_blocks:
        blocks_by_msg.setdefault(b.message_uuid, []).append(b)

    # Chat rows.
    for c in parsed.conversations:
        when = c.created_at or c.updated_at or ""
        text = c.summary or c.name or ""
        yield _Row(
            uuid=c.conversation_uuid,
            provider="anthropic",
            kind="Chat",
            source_label="Claude",
            when_ts=when,
            author=None,
            account=c.account_uuid,
            project=c.project_uuid,
            channel=None,
            conversation_name=c.name,
            conversation_uuid=c.conversation_uuid,
            message_index=None,
            entire_chat=f"/chat/{c.conversation_uuid}",
            text=text,
            slack_link=None,
            qmd_path=_anthropic_qmd_path(c.account_uuid, c.conversation_uuid, c.name),
            document_uuid=c.conversation_uuid,
        )

    # Message + block rows. Index messages within their conversation by
    # (created_at, message_uuid) to mirror the renderer's order.
    for cuuid, conv in convs.items():
        msgs = sorted(
            msgs_by_conv.get(cuuid, []),
            key=lambda m: (m.created_at or "", m.message_uuid),
        )
        model = (conv.raw_json or {}).get("model") or ""
        for msg_idx, m in enumerate(msgs):
            kind = _anthropic_kind_for_sender(m.sender or "")
            if kind == "User Input":
                author = conv.account_uuid
            elif kind == "LLM Response":
                author = model or m.sender
            else:
                author = m.sender
            # The claude.ai web API populates ``message.text`` with whatever
            # the first text-or-thinking-shaped block contained, which for
            # assistant turns is often the ``thinking`` content rather than
            # the user-visible response. Reconstruct from text-type blocks
            # so the grid row reflects what the renderer actually surfaces.
            msg_blocks = sorted(
                blocks_by_msg.get(m.message_uuid, []), key=lambda x: x.block_index
            )
            text_parts = [b.text for b in msg_blocks if b.type == "text" and b.text]
            row_text = "\n\n".join(text_parts) if text_parts else (m.text or "")
            yield _Row(
                uuid=m.message_uuid,
                provider="anthropic",
                kind=kind,
                source_label="Claude",
                when_ts=m.created_at or "",
                author=author,
                account=conv.account_uuid,
                project=conv.project_uuid,
                channel=None,
                conversation_name=conv.name,
                conversation_uuid=cuuid,
                message_index=msg_idx,
                entire_chat=f"/chat/{cuuid}",
                text=row_text,
                slack_link=None,
                qmd_path=_anthropic_qmd_path(conv.account_uuid, cuuid, conv.name),
                document_uuid=cuuid,
            )

            for b in sorted(
                blocks_by_msg.get(m.message_uuid, []), key=lambda x: x.block_index
            ):
                if b.type not in ("tool_use", "tool_result", "thinking"):
                    continue
                btext = b.text or ""
                if not btext and b.type == "thinking":
                    btext = (b.raw_json or {}).get("thinking") or ""
                row_when = b.start_timestamp or _bump_micros(
                    m.created_at or "", b.block_index + 1
                )
                yield _Row(
                    uuid=f"{m.message_uuid}:{b.block_index}",
                    provider="anthropic",
                    kind=_anthropic_kind_for_block(b.type or ""),
                    source_label="Claude",
                    when_ts=row_when or "",
                    author=model or b.type or "",
                    account=conv.account_uuid,
                    project=conv.project_uuid,
                    channel=None,
                    conversation_name=conv.name,
                    conversation_uuid=cuuid,
                    message_index=msg_idx,
                    entire_chat=f"/chat/{cuuid}",
                    text=btext or (b.type or ""),
                    slack_link=None,
                    qmd_path=_anthropic_qmd_path(conv.account_uuid, cuuid, conv.name),
                    document_uuid=cuuid,
                )


# ----- OpenAI ---------------------------------------------------------------


def _openai_rows(parsed: ParsedChatGPTApi) -> Iterable[_Row]:
    msgs_by_conv: dict[str, list] = {}
    for m in parsed.messages:
        msgs_by_conv.setdefault(m.conversation_id, []).append(m)

    for c in parsed.conversations:
        when = c.create_time or c.update_time or ""
        yield _Row(
            uuid=c.conversation_id,
            provider="openai",
            kind="Chat",
            source_label="ChatGPT",
            when_ts=when,
            author=None,
            account=c.account_id,
            project=None,
            channel=None,
            conversation_name=c.title,
            conversation_uuid=c.conversation_id,
            message_index=None,
            entire_chat=f"/chat/{c.conversation_id}",
            text=c.title or "",
            slack_link=None,
            qmd_path=_openai_qmd_path(c.account_id, c.conversation_id, c.title),
            document_uuid=c.conversation_id,
        )

        msgs = sorted(
            msgs_by_conv.get(c.conversation_id, []),
            key=lambda m: (m.create_time or "", m.message_id),
        )
        conv_time = c.create_time or c.update_time or ""
        for msg_idx, m in enumerate(msgs):
            kind = _openai_kind_for_role_and_type(m.role or "", m.content_type or "")
            if kind == "User Input":
                author = c.account_id
            elif kind in ("LLM Response", "LLM Thinking"):
                author = m.model_slug or m.role
            else:
                author = m.role
            row_when = m.create_time or _bump_micros(conv_time, msg_idx + 1)
            yield _Row(
                uuid=m.message_id,
                provider="openai",
                kind=kind,
                source_label="ChatGPT",
                when_ts=row_when or "",
                author=author,
                account=c.account_id,
                project=None,
                channel=None,
                conversation_name=c.title,
                conversation_uuid=c.conversation_id,
                message_index=msg_idx,
                entire_chat=f"/chat/{c.conversation_id}",
                text=m.text or "",
                slack_link=None,
                qmd_path=_openai_qmd_path(c.account_id, c.conversation_id, c.title),
                document_uuid=c.conversation_id,
            )


# ----- Slack ----------------------------------------------------------------


def _slack_rows(parsed: ParsedSlackApi) -> Iterable[_Row]:
    if not parsed.messages:
        return
    channels = {c.channel_id: c for c in parsed.channels}
    user_labels: dict[str, str] = {
        u.user_id: (u.real_name or u.name or u.user_id) for u in parsed.users
    }

    msgs_by_thread: dict[str, list] = {}
    for m in parsed.messages:
        msgs_by_thread.setdefault(m.thread_uuid, []).append(m)

    for thread_uuid, msgs in msgs_by_thread.items():
        msgs = sorted(msgs, key=lambda m: (m.ts_iso, m.ts))
        root_msg = next((m for m in msgs if m.is_thread_root), msgs[0])
        ch = channels.get(root_msg.channel_id)
        cname = (ch.name if ch and ch.name else None) or root_msg.channel_id
        author = user_labels.get(root_msg.user_id or "", root_msg.user_id)

        # Thread row.
        yield _Row(
            uuid=thread_uuid,
            provider="slack",
            kind="Slack Thread",
            source_label="Slack",
            when_ts=root_msg.ts_iso or "",
            author=author,
            account=root_msg.team_id,
            project=None,
            channel=cname,
            conversation_name=f"#{cname}",
            conversation_uuid=thread_uuid,
            message_index=None,
            entire_chat=f"/slack/{thread_uuid}",
            text=resolve_user_mentions(root_msg.text or "", user_labels),
            slack_link=_slack_link(root_msg.team_id, root_msg.channel_id, root_msg.ts),
            qmd_path=_slack_qmd_path(
                root_msg.team_id, cname, thread_uuid, root_msg.text, user_labels
            ),
            document_uuid=thread_uuid,
        )

        # Message rows.
        for msg_idx, m in enumerate(msgs):
            mauthor = user_labels.get(m.user_id or "", m.user_id)
            yield _Row(
                uuid=m.uuid,
                provider="slack",
                kind="Slack Message",
                source_label="Slack",
                when_ts=m.ts_iso or "",
                author=mauthor,
                account=m.team_id,
                project=None,
                channel=cname,
                conversation_name=f"#{cname}",
                conversation_uuid=thread_uuid,
                message_index=msg_idx,
                entire_chat=f"/slack/{thread_uuid}",
                text=resolve_user_mentions(m.text or "", user_labels),
                slack_link=_slack_link(
                    m.team_id, m.channel_id, m.ts, thread_ts=root_msg.ts
                ),
                qmd_path=_slack_qmd_path(
                    root_msg.team_id, cname, thread_uuid, root_msg.text, user_labels
                ),
                document_uuid=thread_uuid,
            )


# ----- GitHub ---------------------------------------------------------------


def _github_pr_index_path(repo_full_name: str, pr_number: int, title: str) -> str:
    rel_dir, _ = _github_pr_dir(repo_full_name, pr_number, title)
    return f"{rel_dir}/index.qmd"


def _github_thread_path(
    repo_full_name: str, pr_number: int, title: str, thread_key: str
) -> str:
    rel_dir, _ = _github_pr_dir(repo_full_name, pr_number, title)
    if thread_key == "general":
        fname = "general"
    else:
        path, _, line = thread_key.rpartition(":")
        fname = f"{_slugify(path)}-L{line or '0'}"
    return f"{rel_dir}/threads/{fname}.qmd"


def _github_rows(parsed: ParsedGithubApi, self_account: str | None) -> Iterable[_Row]:
    prs = {(p.repo_full_name, p.pr_number): p for p in parsed.pull_requests}

    for pr in parsed.pull_requests:
        yield _Row(
            uuid=pr.uuid,
            provider="github",
            kind="GitHub PR",
            source_label="GitHub",
            when_ts=pr.updated_at or pr.created_at or "",
            author=pr.user_login,
            account=self_account,
            project=pr.repo_full_name,
            channel=None,
            conversation_name=pr.title,
            conversation_uuid=pr.uuid,
            message_index=None,
            entire_chat=f"/chat/{pr.uuid}",
            text=(pr.title + "\n\n" + pr.body).strip() if pr.body else pr.title,
            slack_link=None,
            qmd_path=_github_pr_index_path(pr.repo_full_name, pr.pr_number, pr.title),
            source_url=pr.html_url,
            git_sha=pr.head_sha,
            external_id=str(pr.pr_number),
            document_uuid=pr.uuid,
        )

    # Index comments per-PR so message_index is per-thread.
    comments_by_thread: dict[tuple[str, int, str], list] = {}
    for c in parsed.comments:
        comments_by_thread.setdefault(
            (c.repo_full_name, c.pr_number, c.thread_key), []
        ).append(c)

    for (repo, pr_number, thread_key), items in comments_by_thread.items():
        pr = prs.get((repo, pr_number))
        if pr is None:
            continue
        items_sorted = sorted(items, key=lambda c: (c.created_at, c.external_id))
        qmd = _github_thread_path(repo, pr_number, pr.title, thread_key)
        for idx, c in enumerate(items_sorted):
            yield _Row(
                uuid=c.uuid,
                provider="github",
                kind=c.kind,
                source_label="GitHub",
                when_ts=c.created_at or "",
                author=c.user_login,
                account=self_account,
                project=repo,
                channel=None,
                conversation_name=pr.title,
                conversation_uuid=pr.uuid,
                message_index=idx,
                entire_chat=f"/chat/{pr.uuid}",
                text=c.body or "",
                slack_link=None,
                qmd_path=qmd,
                source_url=c.html_url,
                git_sha=c.commit_id,
                external_id=c.external_id,
                document_uuid=pr.uuid,
            )


# ----- GitLab ---------------------------------------------------------------


def _gitlab_mr_index_path(project_path: str, mr_iid: int, title: str) -> str:
    rel_dir, _ = _gitlab_mr_dir(project_path, mr_iid, title)
    return f"{rel_dir}/index.qmd"


def _gitlab_thread_path(
    project_path: str, mr_iid: int, title: str, thread_key: str
) -> str:
    rel_dir, _ = _gitlab_mr_dir(project_path, mr_iid, title)
    if thread_key == "general":
        fname = "general"
    else:
        path, _, line = thread_key.rpartition(":")
        fname = f"{_slugify(path)}-L{line or '0'}"
    return f"{rel_dir}/threads/{fname}.qmd"


def _gitlab_rows(parsed: ParsedGitlabApi, self_account: str | None) -> Iterable[_Row]:
    mrs = {(m.project_path, m.mr_iid): m for m in parsed.merge_requests}

    for mr in parsed.merge_requests:
        yield _Row(
            uuid=mr.uuid,
            provider="gitlab",
            kind="GitLab MR",
            source_label="GitLab",
            when_ts=mr.updated_at or mr.created_at or "",
            author=mr.author_username,
            account=self_account,
            project=mr.project_path,
            channel=None,
            conversation_name=mr.title,
            conversation_uuid=mr.uuid,
            message_index=None,
            entire_chat=f"/chat/{mr.uuid}",
            text=(mr.title + "\n\n" + mr.description).strip()
            if mr.description
            else mr.title,
            slack_link=None,
            qmd_path=_gitlab_mr_index_path(mr.project_path, mr.mr_iid, mr.title),
            source_url=mr.web_url,
            git_sha=mr.head_sha,
            external_id=str(mr.mr_iid),
            document_uuid=mr.uuid,
        )

    notes_by_thread: dict[tuple[str, int, str], list] = {}
    for n in parsed.notes:
        notes_by_thread.setdefault((n.project_path, n.mr_iid, n.thread_key), []).append(
            n
        )

    for (project, mr_iid, thread_key), items in notes_by_thread.items():
        mr = mrs.get((project, mr_iid))
        if mr is None:
            continue
        items_sorted = sorted(items, key=lambda n: (n.created_at, n.external_id))
        qmd = _gitlab_thread_path(project, mr_iid, mr.title, thread_key)
        for idx, n in enumerate(items_sorted):
            yield _Row(
                uuid=n.uuid,
                provider="gitlab",
                kind="GitLab Discussion Note",
                source_label="GitLab",
                when_ts=n.created_at or "",
                author=n.user_login,
                account=self_account,
                project=project,
                channel=None,
                conversation_name=mr.title,
                conversation_uuid=mr.uuid,
                message_index=idx,
                entire_chat=f"/chat/{mr.uuid}",
                text=n.body or "",
                slack_link=None,
                qmd_path=qmd,
                source_url=n.web_url,
                git_sha=n.commit_sha,
                external_id=n.external_id,
                document_uuid=mr.uuid,
            )


# ----- Notion ---------------------------------------------------------------


_NOTION_HEADING_KIND = {
    "header": "Notion Heading 1",
    "sub_header": "Notion Heading 2",
    "sub_sub_header": "Notion Heading 3",
}


def _notion_rows(parsed: ParsedNotionWeb) -> Iterable[_Row]:
    if not parsed.blocks and not parsed.comments:
        return
    blocks_by_id = {b.block_id: b for b in parsed.blocks}
    page_titles = _notion_page_titles(parsed.blocks, parsed.collections)
    user_names = {u.user_id: (u.name or u.user_id) for u in parsed.users}
    space = parsed.space
    account = space.name if space else None

    # Page rows + heading rows.
    for b in parsed.blocks:
        if not b.alive:
            continue
        if b.type in ("page", "collection_view_page"):
            title = page_titles.get(b.block_id, "(untitled)")
            kind = (
                "Notion Database" if b.type == "collection_view_page" else "Notion Page"
            )
            qmd = _notion_page_qmd_path(space, b.block_id, blocks_by_id, page_titles)
            # Aggregate body: page title + every descendant text-bearing block.
            body_parts: list[str] = [title]
            for d in _notion_descendant_blocks(b, blocks_by_id):
                t = rich_text_to_plain(
                    (d.properties or {}).get("title"),
                    user_names=user_names,
                    page_titles=page_titles,
                )
                if t:
                    body_parts.append(t)
            yield _Row(
                uuid=b.block_id,
                provider="notion",
                kind=kind,
                source_label="Notion",
                when_ts=notion_ms_to_iso(b.last_edited_time_ms),
                author=user_names.get(b.last_edited_by_id or "")
                or user_names.get(b.created_by_id or ""),
                account=account,
                project=None,
                channel=None,
                conversation_name=title,
                conversation_uuid=b.block_id,
                message_index=None,
                entire_chat=f"/notion/{b.block_id}",
                text="\n".join(body_parts),
                slack_link=None,
                qmd_path=qmd,
                source_url=notion_url(b.block_id),
                notion_page_uuid=b.block_id,
                notion_block_uuid=None,
                document_uuid=b.block_id,
            )
            continue

        kind = _NOTION_HEADING_KIND.get(b.type or "")
        if kind is None:
            continue
        page_id = _notion_block_owning_page(b, blocks_by_id)
        if not page_id:
            continue
        page = blocks_by_id.get(page_id)
        heading_text = rich_text_to_plain(
            (b.properties or {}).get("title"),
            user_names=user_names,
            page_titles=page_titles,
        )
        yield _Row(
            uuid=notion_heading_uuid(page_id, b.block_id),
            provider="notion",
            kind=kind,
            source_label="Notion",
            when_ts=notion_ms_to_iso(
                (page.last_edited_time_ms if page else None) or b.last_edited_time_ms
            ),
            author=user_names.get(
                (page.last_edited_by_id if page else None) or b.last_edited_by_id or ""
            ),
            account=account,
            project=None,
            channel=None,
            conversation_name=page_titles.get(page_id, "(untitled)"),
            conversation_uuid=page_id,
            message_index=None,
            entire_chat=f"/notion/{page_id}",
            text=heading_text,
            slack_link=None,
            qmd_path=_notion_page_qmd_path(space, page_id, blocks_by_id, page_titles),
            source_url=notion_url(page_id, block_anchor=b.block_id),
            notion_page_uuid=page_id,
            notion_block_uuid=b.block_id,
            document_uuid=page_id,
        )

    # Comment thread + comment rows.
    comments_by_disc: dict[str, list] = {}
    for c in parsed.comments:
        if c.discussion_id:
            comments_by_disc.setdefault(c.discussion_id, []).append(c)
    for disc in parsed.discussions:
        items = sorted(
            comments_by_disc.get(disc.discussion_id, []),
            key=lambda c: (c.created_time_ms or 0, c.comment_id),
        )
        if not items:
            continue
        page_id = _notion_thread_page_id(disc, blocks_by_id)
        if not page_id:
            continue
        block_anchor = disc.parent_id if disc.parent_table == "block" else None
        snippet = (
            items[0].text_plain.splitlines()[0][:60] if items[0].text_plain else ""
        )
        thread_qmd = _notion_thread_qmd_path(
            space, page_id, disc.discussion_id, blocks_by_id, page_titles, snippet
        )
        page_title = page_titles.get(page_id, "(untitled)")
        first = items[0]
        thread_url = notion_url(
            page_id, discussion_id=disc.discussion_id, block_anchor=block_anchor
        )

        # Resolve @-mentions against the user map; `c.text_plain` was built
        # at parse time without it, so unresolved users render as `@<short
        # uuid>`. Re-run rich_text_to_plain here with names available.
        def _comment_text(c):
            return (
                rich_text_to_plain(
                    (c.raw_json or {}).get("text"),
                    user_names=user_names,
                    page_titles=page_titles,
                )
                or c.text_plain
            )

        # Thread row.
        yield _Row(
            uuid=disc.discussion_id,
            provider="notion",
            kind="Notion Comment Thread",
            source_label="Notion",
            when_ts=notion_ms_to_iso(first.created_time_ms),
            author=user_names.get(first.created_by_id or ""),
            account=account,
            project=None,
            channel=None,
            conversation_name=page_title,
            conversation_uuid=disc.discussion_id,
            message_index=None,
            entire_chat=f"/notion/thread/{disc.discussion_id}",
            text="\n".join(t for t in (_comment_text(c) for c in items) if t),
            slack_link=None,
            qmd_path=thread_qmd,
            source_url=thread_url,
            notion_page_uuid=page_id,
            notion_block_uuid=block_anchor,
            document_uuid=disc.discussion_id,
        )

        for idx, c in enumerate(items):
            yield _Row(
                uuid=c.comment_id,
                provider="notion",
                kind="Notion Comment",
                source_label="Notion",
                when_ts=notion_ms_to_iso(c.created_time_ms),
                author=user_names.get(c.created_by_id or ""),
                account=account,
                project=None,
                channel=None,
                conversation_name=page_title,
                conversation_uuid=disc.discussion_id,
                message_index=idx,
                entire_chat=f"/notion/thread/{disc.discussion_id}",
                text=_comment_text(c),
                slack_link=None,
                qmd_path=thread_qmd,
                source_url=thread_url,
                notion_page_uuid=page_id,
                notion_block_uuid=block_anchor,
                document_uuid=disc.discussion_id,
            )


def _notion_descendant_blocks(page, blocks_by_id: dict) -> Iterable:
    """Recursively yield every block under `page.content`, stopping at
    nested pages so a parent page's `text` doesn't engulf its children."""
    stack = list(page.content)
    seen: set[str] = set()
    while stack:
        bid = stack.pop()
        if bid in seen:
            continue
        seen.add(bid)
        b = blocks_by_id.get(bid)
        if b is None:
            continue
        if b.type in ("page", "collection_view_page"):
            continue
        yield b
        stack.extend(b.content)


def _notion_block_owning_page(block, blocks_by_id: dict) -> str | None:
    """Walk parents until we find a page-type block. Returns block_id of
    that page, or None if the block is orphaned."""
    cur_id = block.parent_id
    seen: set[str] = set()
    while cur_id and cur_id not in seen:
        seen.add(cur_id)
        parent = blocks_by_id.get(cur_id)
        if parent is None:
            return None
        if parent.type in ("page", "collection_view_page"):
            return parent.block_id
        cur_id = parent.parent_id
    return None


# ----- entry point ----------------------------------------------------------


def gather_rows(
    anthropic: ParsedExport | None,
    openai: ParsedChatGPTApi | None,
    slack: ParsedSlackApi | None,
    github: ParsedGithubApi | None = None,
    gitlab: ParsedGitlabApi | None = None,
    notion: ParsedNotionWeb | None = None,
) -> list[_Row]:
    """Assemble the unified `_Row` list from every provider's parsed data
    without touching Dolt. Shared by `populate_grid_rows` and the
    documents-table populator so they hash exactly the same row set."""
    rows: list[_Row] = []
    if anthropic is not None:
        rows.extend(_anthropic_rows(anthropic))
    if openai is not None:
        rows.extend(_openai_rows(openai))
    if slack is not None:
        rows.extend(_slack_rows(slack))
    if github is not None:
        gh_acct = github.self_identity.login if github.self_identity else None
        rows.extend(_github_rows(github, gh_acct))
    if gitlab is not None:
        gl_acct = gitlab.self_identity.login if gitlab.self_identity else None
        rows.extend(_gitlab_rows(gitlab, gl_acct))
    if notion is not None:
        rows.extend(_notion_rows(notion))
    return rows


def populate_grid_rows(
    conn: Connection,
    anthropic: ParsedExport | None,
    openai: ParsedChatGPTApi | None,
    slack: ParsedSlackApi | None,
    github: ParsedGithubApi | None = None,
    gitlab: ParsedGitlabApi | None = None,
    notion: ParsedNotionWeb | None = None,
    rows: list[_Row] | None = None,
) -> int:
    """Truncate `grid_rows` and re-emit every row from the parsed provider
    data. Returns the number of rows inserted. Callers can pass a
    pre-computed `rows` list (from `gather_rows`) to avoid re-running the
    per-provider row generators."""
    ensure_schema(conn)
    if rows is None:
        rows = gather_rows(anthropic, openai, slack, github, gitlab, notion)

    placeholders = ",".join(["%s"] * len(_GRID_ROWS_COLUMNS))
    columns_sql = ", ".join(_GRID_ROWS_COLUMNS)
    with conn.cursor() as cur:
        cur.execute("DELETE FROM grid_rows")
        if rows:
            raw_tuples = [
                (
                    r.uuid,
                    r.provider,
                    r.kind,
                    r.source_label,
                    r.when_ts,
                    r.author,
                    r.account,
                    r.project,
                    r.channel,
                    r.conversation_name,
                    r.conversation_uuid,
                    r.message_index,
                    r.entire_chat,
                    r.text,
                    r.slack_link,
                    r.qmd_path,
                    r.source_url,
                    r.git_sha,
                    r.external_id,
                    r.notion_page_uuid,
                    r.notion_block_uuid,
                    r.document_uuid,
                )
                for r in rows
            ]
            cur.executemany(
                f"INSERT INTO grid_rows ({columns_sql}) VALUES ({placeholders})",
                [
                    tuple(
                        _truncate_for_column(col, v)
                        for col, v in zip(_GRID_ROWS_COLUMNS, t)
                    )
                    for t in raw_tuples
                ],
            )
    return len(rows)
