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

    # Cross-table sanity: every Anthropic table represented.
    for table in (
        "anthropic_accounts",
        "anthropic_conversations",
        "anthropic_messages",
        "anthropic_content_blocks",
        "anthropic_attachments",
        "anthropic_projects",
        "openai_accounts",
        "openai_conversations",
        "openai_messages",
        "openai_content_parts",
    ):
        assert f"CREATE TABLE {table}" in dump, f"missing table {table}"


def test_qmd_tar_contains_expected_files() -> None:
    tar_path = _runfile("tests/fixtures/ingested/qmd.tar")
    with tarfile.open(tar_path) as tf:
        names = sorted(tf.getnames())

    # Seven rendered conversations across two providers.
    qmd_files = [n for n in names if n.endswith(".qmd")]
    assert len(qmd_files) == 7, qmd_files

    # No dolt internals leaked into the tar.
    assert not any("dolt_repo" in n or ".dolt" in n for n in names), names

    # Spot-check one slug.
    assert any("tea-earl-grey-hot.qmd" in n for n in qmd_files)


def test_dump_sql_loads_into_in_memory_sqlite() -> None:
    """The dump is portable: a downstream consumer can load it into
    in-memory SQLite without a Dolt subprocess and query the data."""
    from ingest.sqlite_load import load_dump_into_memory

    conn = load_dump_into_memory(_runfile("tests/fixtures/ingested/dump.sql"))

    # Row counts match what the genrule logged.
    assert conn.execute("SELECT COUNT(*) FROM anthropic_accounts").fetchone()[0] == 3
    assert (
        conn.execute("SELECT COUNT(*) FROM anthropic_conversations").fetchone()[0] == 5
    )
    assert conn.execute("SELECT COUNT(*) FROM openai_accounts").fetchone()[0] == 1
    assert conn.execute("SELECT COUNT(*) FROM openai_conversations").fetchone()[0] == 2

    # Picard's UUID is a stable provider key; he should be present.
    row = conn.execute(
        "SELECT full_name FROM anthropic_accounts "
        "WHERE account_uuid = '00000001-1701-4d00-8000-000000000001'"
    ).fetchone()
    assert row is not None
    assert "Picard" in row["full_name"]
