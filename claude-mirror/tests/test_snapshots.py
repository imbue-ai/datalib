"""Golden-file snapshot tests over the ingestion + render pipeline.

Inputs come from the cached genrule artifact
(`//claude-mirror/tests/fixtures:ingested_tng`):

  * `dump.sql` — deterministic SQL dump of the Dolt database
  * `qmd.tar` — rendered Quarto markdown, one file per conversation

Each table gets its own `.sql` golden (the `CREATE TABLE` + sorted
`INSERT`s for that table only) and each rendered conversation gets its
own `.md` golden. Both use `SingleFileSnapshotExtension` subclasses
declared in `snapshot_extensions.py` so the goldens are plain text — you
can preview the markdown directly and load the SQL into any client.

To regenerate after an intentional change:

    bazelisk test //claude-mirror:test_snapshots --test_arg=--snapshot-update

Or under uv (after a successful Bazel build):

    uv run pytest tests/test_snapshots.py --snapshot-update
"""

from __future__ import annotations

import os
import re
import tarfile
from pathlib import Path

import pytest

from tests.snapshot_extensions import MarkdownSnapshotExtension, SqlSnapshotExtension


def _locate_artifact() -> Path | None:
    """Return the directory containing dump.sql + qmd.tar, or None."""
    runfiles = os.environ.get("RUNFILES_DIR") or os.environ.get("TEST_SRCDIR")
    if runfiles:
        cand = Path(runfiles) / "_main" / "claude-mirror" / "tests" / "fixtures" / "ingested"
        if (cand / "dump.sql").exists():
            return cand
    # Fallback for `uv run pytest`: walk up to a `bazel-bin/` sibling.
    here = Path(__file__).resolve()
    for parent in here.parents:
        cand = parent / "bazel-bin" / "claude-mirror" / "tests" / "fixtures" / "ingested"
        if (cand / "dump.sql").exists():
            return cand
    return None


_ARTIFACT_DIR = _locate_artifact()
pytestmark = pytest.mark.skipif(
    _ARTIFACT_DIR is None,
    reason="ingested_tng artifact not built; run `bazelisk build //claude-mirror/tests/fixtures:ingested_tng`",
)


# ---------------------------------------------------------------------------
# Per-table SQL snapshots
# ---------------------------------------------------------------------------

_TABLE_HEADER_RE = re.compile(r"^-- ([a-z0-9_]+)$", re.MULTILINE)


def _split_dump_by_table(dump_text: str) -> dict[str, str]:
    """Split the dump into `{table_name: section_text}` chunks."""
    sections: dict[str, str] = {}
    matches = list(_TABLE_HEADER_RE.finditer(dump_text))
    for i, m in enumerate(matches):
        end = matches[i + 1].start() if i + 1 < len(matches) else len(dump_text)
        sections[m.group(1)] = dump_text[m.start():end].rstrip() + "\n"
    return sections


@pytest.fixture(scope="module")
def table_sections() -> dict[str, str]:
    assert _ARTIFACT_DIR is not None
    return _split_dump_by_table((_ARTIFACT_DIR / "dump.sql").read_text())


# Table list mirrors `claude_mirror.dump._TABLES` — kept hard-coded here so
# the parametrize ids are visible at collection time without importing the
# genrule's working set.
_TABLES = (
    "anthropic_accounts",
    "anthropic_attachments",
    "anthropic_content_blocks",
    "anthropic_conversations",
    "anthropic_messages",
    "anthropic_projects",
    "openai_accounts",
    "openai_content_parts",
    "openai_conversations",
    "openai_messages",
)


@pytest.fixture
def sql_snapshot(snapshot):
    return snapshot.use_extension(SqlSnapshotExtension)


@pytest.mark.parametrize("table", _TABLES)
def test_table_dump_matches_snapshot(
    table: str,
    table_sections: dict[str, str],
    sql_snapshot,
) -> None:
    section = table_sections.get(table)
    assert section is not None, f"table {table!r} missing from dump.sql"
    assert section == sql_snapshot


# ---------------------------------------------------------------------------
# Per-conversation Markdown snapshots
# ---------------------------------------------------------------------------


@pytest.fixture(scope="module")
def qmd_files() -> dict[str, str]:
    """Map of `<provider>/<account>/<slug>.qmd` → file contents."""
    assert _ARTIFACT_DIR is not None
    out: dict[str, str] = {}
    with tarfile.open(_ARTIFACT_DIR / "qmd.tar") as tf:
        for member in tf.getmembers():
            if not member.isfile() or not member.name.endswith(".qmd"):
                continue
            f = tf.extractfile(member)
            if f is None:
                continue
            out[member.name] = f.read().decode("utf-8")
    return out


# Discover qmd paths once at collection time so the parametrize ids show up
# in test output. The id is the leaf slug — full paths are too long for the
# filesystem when combined with syrupy's per-test snapshot naming.
def _discover_qmd_params() -> list:
    if _ARTIFACT_DIR is None:
        return []
    with tarfile.open(_ARTIFACT_DIR / "qmd.tar") as tf:
        names = sorted(m.name for m in tf.getmembers() if m.isfile() and m.name.endswith(".qmd"))
    return [pytest.param(n, id=Path(n).stem) for n in names]


@pytest.fixture
def md_snapshot(snapshot):
    return snapshot.use_extension(MarkdownSnapshotExtension)


@pytest.mark.parametrize("qmd_path", _discover_qmd_params())
def test_qmd_file_matches_snapshot(
    qmd_path: str,
    qmd_files: dict[str, str],
    md_snapshot,
) -> None:
    assert qmd_files[qmd_path] == md_snapshot
