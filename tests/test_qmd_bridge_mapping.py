"""Unit tests for the hit↔row mapping abstraction.

These tests don't run qmd — they exercise `GridIndex` and `parse_query`
against synthetic `QmdHit`s and a tiny in-memory grid. The integration
test (`test_qmd_bridge_integration.py`) is what actually drives the qmd
CLI against the cached fixture index.
"""

from __future__ import annotations

import pytest

from qmd_bridge.mapping import (
    GridIndex,
    GridRowRef,
    QmdHit,
    parse_query,
)


# ---------------------------------------------------------------------------
# Synthetic grid (small, hand-built — mirrors the real fixture's *shape*
# without depending on it).
# ---------------------------------------------------------------------------


# An LLM chat: one container row, one qmd file. The inner m-* divs are
# message ids, NOT grid rows.
LLM_PATH = "rendered_md/anthropic/00000001-1701-4d00-8000-000000000001/llm_chats/c0000001-1701-4d00-8000-00000000c001__tea-earl-grey-hot.md"
LLM_ROW = GridRowRef(
    uuid="c0000001-1701-4d00-8000-00000000c001",
    kind="Chat",
    qmd_path=LLM_PATH,
    provider="anthropic",
)

# A Slack thread: one container row, one qmd file.
SLACK_PATH = "rendered_md/slack/T_NCC1701D/ten-forward/threads/a93e15fa-68d5-5d53-aa9a-590e01b83275__anyone-up-for-poker-tonight.md"
SLACK_ROW = GridRowRef(
    uuid="a93e15fa-68d5-5d53-aa9a-590e01b83275",
    kind="Slack Thread",
    qmd_path=SLACK_PATH,
    provider="slack",
)

# A GitHub PR-42 container row (qmd_path = index.md).
GH_PR_PATH = "rendered_md/github/enterprise-d/replicator-firmware/pr-42__recalibrate-replicator-templates-for-earl-grey-hot/index.md"
GH_PR_ROW = GridRowRef(
    uuid="ed42aaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
    kind="GitHub PR",
    qmd_path=GH_PR_PATH,
    provider="github",
)

# Two GitHub PR-42 comment rows whose qmd_path is a shared thread file.
GH_THREAD_PATH = "rendered_md/github/enterprise-d/replicator-firmware/pr-42__recalibrate-replicator-templates-for-earl-grey-hot/threads/general.md"
GH_COMMENT_1 = GridRowRef(
    uuid="0a6abb8f-71df-553c-80e0-940c8f0c1213",
    kind="GitHub PR Comment",
    qmd_path=GH_THREAD_PATH,
    provider="github",
)
GH_COMMENT_2 = GridRowRef(
    uuid="c75eacdd-fa14-58cb-88db-e1a6dc12a9e3",
    kind="GitHub Review",
    qmd_path=GH_THREAD_PATH,
    provider="github",
)


@pytest.fixture
def index() -> GridIndex:
    return GridIndex([LLM_ROW, SLACK_ROW, GH_PR_ROW, GH_COMMENT_1, GH_COMMENT_2])


# ---------------------------------------------------------------------------
# parse_query
# ---------------------------------------------------------------------------


def test_parse_query_bare_text_is_query_mode():
    assert parse_query("earl grey") == ("query", "earl grey")


def test_parse_query_qmd_predicate():
    assert parse_query('qmd:"earl grey"') == ("query", "earl grey")


def test_parse_query_vsearch_predicate():
    assert parse_query('qmd_vsearch:"earl grey"') == ("vsearch", "earl grey")


def test_parse_query_whitespace_around_predicate():
    assert parse_query('  qmd : "warp core"  ') == ("query", "warp core")


def test_parse_query_preserves_inner_whitespace():
    assert parse_query('qmd:"  earl grey, hot  "') == ("query", "  earl grey, hot  ")


# ---------------------------------------------------------------------------
# rows_for_hit — uuid-match path
# ---------------------------------------------------------------------------


def test_hit_with_known_uuid_resolves_to_that_row(index):
    # Snippet from a GitHub thread chunk: two known comment uuids embedded
    # in <div id="m-...">.
    hit = QmdHit(
        path=GH_THREAD_PATH,
        score=0.5,
        snippet=(
            '<div id="m-0a6abb8f-71df-553c-80e0-940c8f0c1213" data-msg-index="0" '
            'class="msg msg--github">comment</div>\n'
            '<div id="m-c75eacdd-fa14-58cb-88db-e1a6dc12a9e3" data-msg-index="1" '
            'class="msg msg--github">review</div>'
        ),
    )
    rows = index.rows_for_hit(hit)
    assert [r.uuid for r in rows] == [GH_COMMENT_1.uuid, GH_COMMENT_2.uuid]


def test_hit_uuids_preserve_snippet_order(index):
    # Reverse the order in the snippet — result should reverse too.
    hit = QmdHit(
        path=GH_THREAD_PATH,
        score=0.5,
        snippet=(
            "m-c75eacdd-fa14-58cb-88db-e1a6dc12a9e3 "
            "then m-0a6abb8f-71df-553c-80e0-940c8f0c1213"
        ),
    )
    rows = index.rows_for_hit(hit)
    assert [r.uuid for r in rows] == [GH_COMMENT_2.uuid, GH_COMMENT_1.uuid]


def test_hit_dedupes_repeated_uuid(index):
    hit = QmdHit(
        path=GH_THREAD_PATH,
        score=0.5,
        snippet=(
            "m-0a6abb8f-71df-553c-80e0-940c8f0c1213 and again "
            "m-0a6abb8f-71df-553c-80e0-940c8f0c1213"
        ),
    )
    rows = index.rows_for_hit(hit)
    assert [r.uuid for r in rows] == [GH_COMMENT_1.uuid]


# ---------------------------------------------------------------------------
# rows_for_hit — path fallback (uuids present but unknown to grid)
# ---------------------------------------------------------------------------


def test_llm_chat_hit_falls_back_to_conversation_row(index):
    # Inner m-{uuid} ids exist in the snippet but aren't real grid rows
    # (LLM chats are conversation-level). Fallback to qmd_path lookup.
    hit = QmdHit(
        path=LLM_PATH,
        score=0.5,
        snippet=(
            '<div id="m-deadbeef-dead-beef-dead-beefdeadbeef" data-msg-index="0">\n'
            "Tea. Earl Grey. Hot.\n"
        ),
    )
    rows = index.rows_for_hit(hit)
    assert [r.uuid for r in rows] == [LLM_ROW.uuid]


def test_slack_thread_hit_resolves_to_thread_row(index):
    hit = QmdHit(
        path=SLACK_PATH,
        score=0.3,
        snippet="Anyone up for poker tonight?",  # no m-{uuid} divs
    )
    rows = index.rows_for_hit(hit)
    assert [r.uuid for r in rows] == [SLACK_ROW.uuid]


def test_long_message_body_chunk_with_no_m_uuid_falls_back_to_path(index):
    # Simulates a qmd chunk that landed entirely inside a long message
    # body, past the opening `<div id="m-...">` wrapper. There are NO
    # m-{uuid} ids visible in this slice of text. We expect the path
    # fallback to kick in and return every grid row on that file —
    # imprecise but lands the user in the right neighborhood (strict v1).
    hit = QmdHit(
        path=GH_THREAD_PATH,
        score=0.5,
        snippet=(
            "...the water temperature drift on long replicator runs is "
            "consistent with a tannin extraction preset mismatch. "
            "Reproducer: program 'tea, earl grey, hot' on holodeck-3 "
            "after a 2-hour idle and observe the bitter aftertaste..."
        ),
    )
    rows = index.rows_for_hit(hit)
    assert {r.uuid for r in rows} == {GH_COMMENT_1.uuid, GH_COMMENT_2.uuid}


def test_pr_index_hit_resolves_to_pr_container_row(index):
    hit = QmdHit(
        path=GH_PR_PATH,
        score=0.4,
        snippet="pr_number: 42\ntitle: Recalibrate replicator templates",
    )
    rows = index.rows_for_hit(hit)
    assert [r.uuid for r in rows] == [GH_PR_ROW.uuid]


# ---------------------------------------------------------------------------
# rows_for_hit — path normalization
# ---------------------------------------------------------------------------


def test_path_match_is_case_and_underscore_insensitive(index):
    # qmd lowercases + collapses `[_-]+` -> `-`. The hit path arrives
    # already-normalized; rows_for_hit normalizes the grid side for the
    # lookup, so a hit with the qmd-style path should still find the row.
    hit = QmdHit(
        path=(
            "rendered_md/github/enterprise-d/replicator-firmware/"
            "pr-42-recalibrate-replicator-templates-for-earl-grey-hot/index.md"
        ),
        score=0.4,
        snippet="pr_number: 42",
    )
    rows = index.rows_for_hit(hit)
    assert [r.uuid for r in rows] == [GH_PR_ROW.uuid]


# ---------------------------------------------------------------------------
# rows_for_hit — graceful "no match" cases
# ---------------------------------------------------------------------------


def test_unknown_path_with_unknown_uuids_returns_empty(index):
    hit = QmdHit(
        path="nowhere/never.md",
        score=0.1,
        snippet="m-deadbeef-dead-beef-dead-beefdeadbeef",
    )
    assert index.rows_for_hit(hit) == []


# ---------------------------------------------------------------------------
# rows_for_hits — aggregation
# ---------------------------------------------------------------------------


def test_rows_for_hits_preserves_rank_and_dedupes(index):
    # First hit resolves to two rows; second hit resolves to a row that
    # also appeared in the first; third resolves to a new row.
    hits = [
        QmdHit(
            path=GH_THREAD_PATH,
            score=0.5,
            snippet=(
                "m-0a6abb8f-71df-553c-80e0-940c8f0c1213 "
                "m-c75eacdd-fa14-58cb-88db-e1a6dc12a9e3"
            ),
        ),
        QmdHit(
            path=GH_THREAD_PATH,
            score=0.45,
            snippet="m-0a6abb8f-71df-553c-80e0-940c8f0c1213",
        ),
        QmdHit(path=LLM_PATH, score=0.3, snippet=""),
    ]
    rows = index.rows_for_hits(hits)
    assert [r.uuid for r in rows] == [
        GH_COMMENT_1.uuid,
        GH_COMMENT_2.uuid,
        LLM_ROW.uuid,
    ]


# ---------------------------------------------------------------------------
# hits_for_row — reverse direction
# ---------------------------------------------------------------------------


def test_hits_for_message_level_row_filters_by_uuid(index):
    # Three hits on the same thread file. Only the two that mention
    # COMMENT_1's uuid in their snippet should map back.
    h1 = QmdHit(
        path=GH_THREAD_PATH,
        score=0.6,
        snippet="m-0a6abb8f-71df-553c-80e0-940c8f0c1213 first",
    )
    h2 = QmdHit(
        path=GH_THREAD_PATH,
        score=0.5,
        snippet=(
            "m-0a6abb8f-71df-553c-80e0-940c8f0c1213 and "
            "m-c75eacdd-fa14-58cb-88db-e1a6dc12a9e3"
        ),
    )
    h3 = QmdHit(
        path=GH_THREAD_PATH,
        score=0.4,
        snippet="m-c75eacdd-fa14-58cb-88db-e1a6dc12a9e3 only",
    )
    out = index.hits_for_row(GH_COMMENT_1, [h1, h2, h3])
    assert [h.score for h in out] == [0.6, 0.5]


def test_hits_for_row_with_no_m_uuids_falls_back_to_path(index):
    # A snippet with no m-{uuid} ids still counts as mentioning the
    # container row (LLM chat, Slack thread, PR/MR index).
    h = QmdHit(path=LLM_PATH, score=0.7, snippet="Tea. Earl Grey. Hot.")
    assert index.hits_for_row(LLM_ROW, [h]) == [h]


def test_hits_for_row_ignores_other_paths(index):
    h = QmdHit(
        path="some/other/path.md",
        score=0.9,
        snippet=f"m-{LLM_ROW.uuid} mention",
    )
    assert index.hits_for_row(LLM_ROW, [h]) == []
