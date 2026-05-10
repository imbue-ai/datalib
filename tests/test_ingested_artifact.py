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

    # 7 LLM conversations + 5 Slack threads (Picard's tea thread, Worf
    # standalone, Data standalone in #engineering, Riker's #ten-forward
    # poker thread, and Picard's combadge thread in #bridge) across three
    # providers.
    qmd_files = [n for n in names if n.endswith(".qmd")]
    assert len(qmd_files) == 12, qmd_files

    # No dolt internals leaked into the tar.
    assert not any("dolt_repo" in n or ".dolt" in n for n in names), names

    # Spot-check one slug from each provider.
    assert any("tea-earl-grey-hot.qmd" in n for n in qmd_files)
    assert any("anyone-up-for-poker-tonight.qmd" in n for n in qmd_files)


def test_dump_sql_loads_into_in_memory_sqlite() -> None:
    """The dump is portable: a downstream consumer can load it into
    in-memory SQLite without a Dolt subprocess and query the data."""
    from ingest.sqlite_load import load_dump_into_memory

    conn = load_dump_into_memory(_runfile("tests/fixtures/ingested/dump.sql"))

    # Sanity check expected counts on the union projection.
    # 5 anthropic chats + 2 openai chats = 7 chat rows.
    chats = conn.execute(
        "SELECT COUNT(*) FROM grid_rows WHERE kind = 'Chat'"
    ).fetchone()[0]
    assert chats == 7

    # 5 Slack thread rows (from render fixture).
    threads = conn.execute(
        "SELECT COUNT(*) FROM grid_rows WHERE kind = 'Slack Thread'"
    ).fetchone()[0]
    assert threads == 5

    # Picard's account_uuid is a stable provider key; he should appear in
    # the account column on Anthropic rows.
    row = conn.execute(
        "SELECT COUNT(*) FROM grid_rows "
        "WHERE provider = 'anthropic' "
        "AND account = '00000001-1701-4d00-8000-000000000001'"
    ).fetchone()
    assert row[0] > 0
