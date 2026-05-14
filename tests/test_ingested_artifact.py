"""Tests against the cached output of `//tests/fixtures:ingested_tng`.

These run only under Bazel, where the genrule's outputs are wired in as
runfiles via the test target's `data` attribute. Outside Bazel the tests
are skipped (so `uv run pytest` continues to work for the inner loop).

The point of using the genrule output here \u2014 rather than re-ingesting
inside the test \u2014 is that the heavy work (Dolt subprocess, ingest,
render, dump) runs once per source change and is shared across every
downstream test that needs ingested data.
"""

from __future__ import annotations

import os
import tarfile
from pathlib import Path

import pytest

# Bazel sets RUNFILES_DIR / TEST_SRCDIR on test actions. When running under
# `uv run pytest` these are unset and the genrule outputs aren't available;
# skip the whole module in that case.
_RUNFILES = os.environ.get("RUNFILES_DIR") or os.environ.get("TEST_SRCDIR")
pytestmark = pytest.mark.skipif(
    _RUNFILES is None,
    reason="ingested_tng artifact only available under bazel test",
)


def _runfile(rel: str) -> Path:
    base = Path(_RUNFILES or "") / "_main"
    return base / rel


def test_dump_sql_has_expected_tables_and_rows() -> None:
    dump = _runfile("tests/fixtures/ingested/dump.sql").read_text()

    # Every TNG conversation should appear by UUID prefix.
    for uuid_prefix in (
        "c0000001-1701-4d00-8000",  # Tea, Earl Grey
        "c0000002-1701-4d00-8000",  # Warp plasma
        "c0000003-1701-4d00-8000",  # Klingon dispatch
        "c0000004-1701-4d00-8000",  # Tricorder
        "c0000005-1701-4d00-8000",  # Borg encryption signature
        "68fa0001-fake-7000-8000",  # Sonnet
        "68fa0002-fake-7000-8000",  # Polyfit
        "68fa0003-fake-7000-8000",  # Long-title warp research (truncation guard)
    ):
        assert uuid_prefix in dump, f"missing conversation {uuid_prefix} in dump.sql"

    # Only grid_rows is dumped now; per-provider tables don't exist.
    assert "CREATE TABLE grid_rows" in dump
    assert "CREATE TABLE anthropic_" not in dump
    assert "CREATE TABLE openai_" not in dump
    assert "CREATE TABLE slack_" not in dump


def test_qmd_tar_contains_expected_files() -> None:
    tar_path = _runfile("tests/fixtures/ingested/qmd.tar")
    with tarfile.open(tar_path) as tf:
        names = sorted(tf.getnames())

    # 8 LLM conversations + 6 Slack threads + 7 GitHub PR files (2 indices +
    # 5 thread files) + 6 GitLab MR files (2 indices + 4 thread files) +
    # 5 Notion files (3 page index.md + 2 comment thread files).
    md_files = [n for n in names if n.endswith(".md")]
    assert len(md_files) == 32, md_files

    # No dolt internals leaked into the tar.
    assert not any("dolt_repo" in n or ".dolt" in n for n in names), names

    # Spot-check one slug from each provider.
    assert any("tea-earl-grey-hot.md" in n for n in md_files)
    assert any("anyone-up-for-poker-tonight.md" in n for n in md_files)
    assert any("pr-42__recalibrate-replicator" in n for n in md_files)
    assert any("mr-17__add-earl-grey" in n for n in md_files)
    # Notion (official-API path): pages live under `notion/pages/<slug>__<id8>/`
    # with comment threads as siblings under `threads/`.
    assert any(
        "notion/pages/bridge-operations-handbook__" in n and n.endswith("/index.md")
        for n in md_files
    )
    assert any(
        "notion/pages/shift-roster__" in n and n.endswith("/index.md") for n in md_files
    )
    assert any("/threads/d0000001__" in n for n in md_files)


def test_dump_sql_loads_into_in_memory_sqlite() -> None:
    """The dump is portable: a downstream consumer can load it into
    in-memory SQLite without a Dolt subprocess and query the data."""
    from ingest.sqlite_load import load_dump_into_memory

    conn = load_dump_into_memory(_runfile("tests/fixtures/ingested/dump.sql"))

    # Sanity check expected counts on the union projection.
    # 5 anthropic chats + 3 openai chats = 8 chat rows.
    chats = conn.execute(
        "SELECT COUNT(*) FROM grid_rows WHERE kind = 'Chat'"
    ).fetchone()[0]
    assert chats == 8

    # The third openai chat has a pathologically long auto-title. Ingest
    # must truncate it to fit conversation_name VARCHAR(512) (with a
    # trailing ellipsis) rather than failing the whole INSERT batch.
    long_title_row = conn.execute(
        "SELECT conversation_name FROM grid_rows "
        "WHERE uuid = '68fa0003-fake-7000-8000-positronic0003'"
    ).fetchone()
    assert long_title_row is not None
    cname = long_title_row[0]
    assert len(cname) <= 512
    assert cname.endswith("…")
    assert cname.startswith("I have been reviewing the Daystrom Institute")

    # 6 Slack thread rows (from render fixture).
    threads = conn.execute(
        "SELECT COUNT(*) FROM grid_rows WHERE kind = 'Slack Thread'"
    ).fetchone()[0]
    assert threads == 6

    # GitHub: 2 PRs in the fixture (closed #42, open #43).
    gh_prs = conn.execute(
        "SELECT COUNT(*) FROM grid_rows WHERE kind = 'GitHub PR'"
    ).fetchone()[0]
    assert gh_prs == 2

    # GitLab: 2 MRs in the fixture (merged !17, open !18).
    gl_mrs = conn.execute(
        "SELECT COUNT(*) FROM grid_rows WHERE kind = 'GitLab MR'"
    ).fetchone()[0]
    assert gl_mrs == 2

    # Notion (official-API path): 3 pages + 2 discussion threads + 3 comments.
    # Heading and Database rows from the legacy unofficial path are not emitted.
    notion_pages = conn.execute(
        "SELECT COUNT(*) FROM grid_rows WHERE kind = 'Notion Page'"
    ).fetchone()[0]
    assert notion_pages == 3
    notion_threads = conn.execute(
        "SELECT COUNT(*) FROM grid_rows WHERE kind = 'Notion Comment Thread'"
    ).fetchone()[0]
    assert notion_threads == 2
    notion_comments = conn.execute(
        "SELECT COUNT(*) FROM grid_rows WHERE kind = 'Notion Comment'"
    ).fetchone()[0]
    assert notion_comments == 3

    # Picard's account_uuid is a stable provider key; he should appear in
    # the account column on Anthropic rows.
    row = conn.execute(
        "SELECT COUNT(*) FROM grid_rows "
        "WHERE provider = 'anthropic' "
        "AND account = '00000001-1701-4d00-8000-000000000001'"
    ).fetchone()
    assert row[0] > 0
