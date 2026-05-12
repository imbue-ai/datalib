"""Regression test for Notion comment-thread rendering.

A discussion record carries a `comments` array listing every comment id
in the thread. In real backups we've seen cases where a newer reply
shows up in that list but its `notion_comment` record hasn't been
fetched (or wasn't included in the backup yet). The renderer currently
walks `parsed.comments` only, so any referenced-but-missing comment is
silently dropped from the rendered thread.

Concrete example from `~/backups/notion`, discussion
`358a550f-af95-80ae-810b-001c1f13e742`: its `comments` list names 3 ids
but only 2 of them have a `notion_comment` row, so Thad's final reply
is missing from the rendered markdown.

This test reproduces that scenario with synthetic data and asserts the
final comment's body appears in the output. It currently FAILS — left
as a regression test until the renderer learns to surface
referenced-but-missing comments (e.g. as a placeholder line).
"""

from __future__ import annotations

from pathlib import Path

from ingest.providers.notion.parse import (
    BlockRow,
    CommentRow,
    DiscussionRow,
    ParsedNotionWeb,
    SpaceRow,
    UserRow,
)
from ingest.render import render_notion


def _make_parsed() -> ParsedNotionWeb:
    space_id = "5face1d0-1701-4d00-8000-0000000000aa"
    page_id = "b10cb10c-1701-4d00-8000-0000000000a1"
    anchor_id = "b10cb10c-1701-4d00-8000-0000000000a2"
    disc_id = "d15c0001-1701-4d00-8000-0000000000a3"
    c1 = "c00ffee1-1701-4d00-8000-0000000000a4"
    c2 = "c00ffee2-1701-4d00-8000-0000000000a5"
    c3_missing = "c00ffee3-1701-4d00-8000-0000000000a6"  # referenced, not stored
    zack = "00000001-1701-4d00-8000-00000000000a"
    thad = "00000002-1701-4d00-8000-00000000000b"

    parsed = ParsedNotionWeb()
    parsed.space = SpaceRow(
        space_id=space_id, name="Test Space", domain=None, raw_json={}
    )
    parsed.users = [
        UserRow(user_id=zack, name="Zack Polizzi", email=None, raw_json={}),
        UserRow(user_id=thad, name="Thad Hughes", email=None, raw_json={}),
    ]
    parsed.blocks = [
        BlockRow(
            block_id=page_id,
            space_id=space_id,
            type="page",
            properties={"title": [["Personal Data Liberation"]]},
            format={},
            content=[anchor_id],
            parent_id=space_id,
            parent_table="space",
            created_time_ms=0,
            last_edited_time_ms=0,
            created_by_id=zack,
            last_edited_by_id=zack,
            alive=True,
            collection_id=None,
            view_ids=[],
            raw_json={},
        ),
        BlockRow(
            block_id=anchor_id,
            space_id=space_id,
            type="text",
            properties={"title": [["Backup/Mirror/Cache"]]},
            format={},
            content=[],
            parent_id=page_id,
            parent_table="block",
            created_time_ms=0,
            last_edited_time_ms=0,
            created_by_id=zack,
            last_edited_by_id=zack,
            alive=True,
            collection_id=None,
            view_ids=[],
            raw_json={},
        ),
    ]
    parsed.discussions = [
        DiscussionRow(
            discussion_id=disc_id,
            parent_id=anchor_id,
            parent_table="block",
            space_id=space_id,
            resolved=False,
            comment_ids=[c1, c2, c3_missing],
            context_plain="Backup/Mirror/Cache",
            raw_json={},
        )
    ]
    parsed.comments = [
        CommentRow(
            comment_id=c1,
            discussion_id=disc_id,
            space_id=space_id,
            created_by_id=zack,
            created_time_ms=1_700_000_000_000,
            last_edited_time_ms=None,
            text_plain=(
                "are you primarily thinking of getting data via one-time "
                "manual “takeouts”, or trying also to keep these data "
                "views fresh as new data comes into the silo?"
            ),
            raw_json={
                "id": c1,
                "text": [
                    [
                        "are you primarily thinking of getting data via one-time "
                        "manual “takeouts”, or trying also to keep these data "
                        "views fresh as new data comes into the silo?"
                    ]
                ],
            },
        ),
        CommentRow(
            comment_id=c2,
            discussion_id=disc_id,
            space_id=space_id,
            created_by_id=zack,
            created_time_ms=1_700_000_001_000,
            last_edited_time_ms=None,
            text_plain=(
                "(i think this decision affects both the difficulty and "
                "the usefulness significantly)"
            ),
            raw_json={
                "id": c2,
                "text": [
                    [
                        "(i think this decision affects both the difficulty and "
                        "the usefulness significantly)"
                    ]
                ],
            },
        ),
        # c3_missing intentionally absent — present in discussion.comment_ids
        # but no CommentRow available, mirroring real backup data.
    ]
    return parsed


def test_thread_renders_comment_referenced_but_missing_from_comment_table(
    tmp_path: Path,
) -> None:
    parsed = _make_parsed()
    render_notion(parsed, tmp_path)

    thread_files = list(tmp_path.rglob("comments/*.qmd"))
    assert len(thread_files) == 1, thread_files
    body = thread_files[0].read_text()

    # The two comments with backing CommentRow records render fine.
    assert "Zack Polizzi" in body
    assert "takeouts" in body

    # The third comment is named in discussion.comment_ids but no
    # CommentRow exists for it. The renderer should still surface its
    # presence (placeholder author, "missing comment" marker, or similar)
    # rather than silently dropping it. Today it drops it — this assertion
    # fails as a result.
    missing_id = "c00ffee3-1701-4d00-8000-0000000000a6"
    assert missing_id in body or "missing" in body.lower(), (
        "expected referenced-but-missing comment to be surfaced in the "
        "rendered thread, but the rendered body contains no trace of it:\n" + body
    )
