"""End-to-end test: run the real `qmd` CLI against the cached
`//tests/fixtures:ingested_tng_qmd` index, map results to grid rows.

Skipped outside Bazel (the artifact lives in runfiles). Tagged
`requires-network` + `no-remote` so it only runs where node/npx are
available and the models cache (`~/.cache/qmd-models`) is populated by
the indexer.

The test extracts qmd.tar + qmd-index.tar into a tmpdir, re-creates the
`models` symlink the indexer set up, and points the runner at it via
`XDG_CACHE_HOME=<tmp>/.frankweiler`.
"""

from __future__ import annotations

import os
import shutil
import sqlite3
import tarfile
from pathlib import Path

import pytest

from ingest.sqlite_load import load_dump_into_memory
from qmd_bridge.mapping import GridIndex, parse_query
from qmd_bridge.runner import QmdRunner, QmdRunnerConfig


_RUNFILES = os.environ.get("RUNFILES_DIR") or os.environ.get("TEST_SRCDIR")
pytestmark = pytest.mark.skipif(
    _RUNFILES is None,
    reason="qmd-index artifact only available under bazel test",
)


def _runfile(rel: str) -> Path:
    return Path(_RUNFILES or "") / "_main" / rel


def _find_host_models_dir() -> Path | None:
    """Locate the user's `.cache/qmd-models` populated by the indexer.

    Bazel's test runner sets `HOME` to a per-test tmpdir, so `~` doesn't
    point at the real home where the indexer parked the models. Try
    explicit overrides first, then crawl up `__file__` (the bazel cache
    lives somewhere under the user's home) looking for the marker.
    """
    for env_key in ("CLAUDE_MIRROR_HOST_HOME", "HOME"):
        h = os.environ.get(env_key)
        if h:
            d = Path(h) / ".cache" / "qmd-models"
            if d.exists():
                return d
    here = Path(__file__).resolve()
    for p in here.parents:
        d = p / ".cache" / "qmd-models"
        if d.exists():
            return d
    return None


@pytest.fixture(scope="module")
def qmd_root(tmp_path_factory) -> Path:
    """Extract qmd.tar + qmd-index.tar and wire up the models symlink."""
    if shutil.which("npx") is None:
        pytest.skip("npx not on PATH")
    qmd_tar = _runfile("tests/fixtures/ingested/qmd.tar")
    idx_tar = _runfile("tests/fixtures/ingested/qmd-index.tar")
    if not qmd_tar.exists() or not idx_tar.exists():
        pytest.skip("qmd.tar / qmd-index.tar not built")

    work = tmp_path_factory.mktemp("qmd_bridge")
    with tarfile.open(qmd_tar) as tf:
        tf.extractall(work)
    with tarfile.open(idx_tar) as tf:
        tf.extractall(work)
    root = work / "qmd"

    # qmd's embedding model is loaded at query time. The indexer
    # symlinks `<.frankweiler>/qmd/models` -> `~/.cache/qmd-models`; we
    # do the same here so vsearch can encode the query.
    models_link = root / ".frankweiler" / "qmd" / "models"
    if not models_link.exists():
        models_dir = _find_host_models_dir()
        if models_dir is None:
            pytest.skip("qmd models cache (.cache/qmd-models) not found")
        models_link.symlink_to(models_dir)

    return root


@pytest.fixture(scope="module")
def runner(qmd_root: Path) -> QmdRunner:
    return QmdRunner(QmdRunnerConfig(qmd_root=qmd_root))


@pytest.fixture(scope="module")
def conn() -> sqlite3.Connection:
    return load_dump_into_memory(_runfile("tests/fixtures/ingested/dump.sql"))


@pytest.fixture(scope="module")
def index(conn: sqlite3.Connection) -> GridIndex:
    return GridIndex.from_sqlite(conn)


# ---------------------------------------------------------------------------
# End-to-end: vsearch -> mapping -> expected rows
# ---------------------------------------------------------------------------


def test_vsearch_earl_grey_hits_pr42_and_mr17(
    runner: QmdRunner, index: GridIndex
) -> None:
    hits = runner.vsearch("earl grey tea", limit=10)
    assert hits, "qmd returned no hits"
    rows = index.rows_for_hits(hits)
    kinds = {r.kind for r in rows}
    # The earl grey query straddles GitHub PR-42 (comments + the PR itself
    # via index.qmd) and GitLab MR-17. We don't assert an exact ordering;
    # we just want the right neighborhood to come back.
    assert {"GitHub PR", "GitLab MR"} <= kinds, kinds
    # And specifically the PR-42 / MR-17 container rows should be in
    # there (index.qmd is the path-fallback case). qmd_path lives on
    # the grid side, where casing/`__` are preserved.
    paths = {r.qmd_path for r in rows}
    assert any("/pr-42__" in p and p.endswith("/index.qmd") for p in paths), paths
    assert any("/mr-17__" in p and p.endswith("/index.qmd") for p in paths), paths


def test_vsearch_holodeck_lands_in_safety_neighborhood(
    runner: QmdRunner, index: GridIndex
) -> None:
    hits = runner.vsearch("holodeck safety interlock", limit=10)
    rows = index.rows_for_hits(hits)
    assert rows
    providers = {r.provider for r in rows}
    # PR-43 (github) and MR-18 (gitlab) both discuss the holodeck safety
    # interlock; semantic search should surface both providers.
    assert {"github", "gitlab"} <= providers, providers


def test_vsearch_llm_chat_resolves_to_chat_row(
    runner: QmdRunner, index: GridIndex
) -> None:
    # Wording from the Anthropic c0000001 "Tea, Earl Grey, Hot" chat —
    # the Picard quote alone surfaces GitHub PR-42 strongly, so we use
    # vocabulary unique to the chat itself (preset tannin discussion).
    # The hit on that .qmd file maps (via path fallback — LLM messages
    # aren't grid rows) to the single conversation-level Chat row.
    hits = runner.vsearch("tannin extraction preset replicator", limit=10)
    rows = index.rows_for_hits(hits)
    chat_rows = [r for r in rows if r.kind == "Chat"]
    assert any(r.uuid == "c0000001-1701-4d00-8000-00000000c001" for r in chat_rows), [
        r.uuid for r in chat_rows
    ]


# ---------------------------------------------------------------------------
# Mapping precision: thread hits return *comment-level* rows (strict
# v1 semantics — no hierarchical PR/MR pull-in).
# ---------------------------------------------------------------------------


def test_thread_hit_returns_comment_rows_not_container(
    runner: QmdRunner, index: GridIndex
) -> None:
    hits = runner.vsearch("water temperature drift replicator", limit=5)
    # Find a hit whose path is a PR-42 thread file (not the index).
    # qmd lowercases + collapses `__` -> `-`, so the path that comes
    # back is `.../pr-42-recalibrate-.../threads/...qmd`.
    thread_hits = [h for h in hits if "/pr-42-" in h.path and "/threads/" in h.path]
    assert thread_hits, [h.path for h in hits]
    rows = index.rows_for_hit(thread_hits[0])
    assert rows, "thread hit produced no rows"
    # Strict v1: container PR row is NOT returned by a thread hit.
    assert all(r.kind != "GitHub PR" for r in rows), [r.kind for r in rows]
    # Every returned row should be a github message-level kind.
    assert all(
        r.kind in {"GitHub PR Comment", "GitHub Review", "GitHub Review Comment"}
        for r in rows
    ), [r.kind for r in rows]


# ---------------------------------------------------------------------------
# Reverse direction: pick a known message-level row, ask which hits
# mention it.
# ---------------------------------------------------------------------------


def test_hits_for_known_comment_row(runner: QmdRunner, index: GridIndex) -> None:
    # Geordi's PR-42 issue comment — uuid is stable across runs.
    geordi_uuid = "0a6abb8f-71df-553c-80e0-940c8f0c1213"
    assert geordi_uuid in index.by_uuid
    row = index.by_uuid[geordi_uuid]

    # A query that should pull up his comment ("water temperature drift").
    hits = runner.vsearch("water temperature drift on long replicator runs", limit=10)
    back = index.hits_for_row(row, hits)
    assert back, f"no hits mapped back to row {geordi_uuid}"
    for h in back:
        # qmd-normalized path (lowercased, `__` collapsed to `-`).
        assert "/pr-42-" in h.path
        # Each returned hit either directly names geordi's uuid OR is a
        # path-fallback hit (no m-{uuid}s parsed). In either case the
        # mapping considers it a mention of the row.


# ---------------------------------------------------------------------------
# Bidirectional coverage: every indexed qmd doc maps to ≥1 grid row, and
# every grid row maps to ≥1 indexed qmd doc. Guards against drift between
# the renderer (which decides what becomes a .qmd file) and the ingest
# pipeline (which decides what becomes a grid_row). Also exercises the
# "long message body chunk with no m-{uuid} in the snippet" case for
# every indexed file: we hand the mapper an empty snippet and require
# the path fallback to land somewhere on the grid.
# ---------------------------------------------------------------------------


def _indexed_paths(qmd_root: Path) -> list[str]:
    """All active document paths in qmd's index.sqlite (collection-stripped)."""
    db = sqlite3.connect(qmd_root / ".frankweiler" / "qmd" / "index.sqlite")
    try:
        return [
            r[0]
            for r in db.execute(
                "SELECT DISTINCT path FROM documents "
                "WHERE collection = 'mirror' AND active = 1"
            )
        ]
    finally:
        db.close()


def test_every_indexed_doc_maps_to_a_grid_row(qmd_root: Path, index: GridIndex) -> None:
    from qmd_bridge.mapping import QmdHit

    paths = _indexed_paths(qmd_root)
    assert paths, "qmd index has no documents"
    orphaned = [
        p for p in paths if not index.rows_for_hit(QmdHit(path=p, score=0, snippet=""))
    ]
    assert not orphaned, f"indexed qmd docs with no grid row: {orphaned}"


def test_every_grid_row_has_an_indexed_doc(qmd_root: Path, index: GridIndex) -> None:
    from qmd_bridge.mapping import _norm_path

    norm_indexed = {_norm_path(p) for p in _indexed_paths(qmd_root)}
    missing = [
        r for r in index.by_uuid.values() if _norm_path(r.qmd_path) not in norm_indexed
    ]
    assert not missing, "grid rows with no indexed qmd doc: " + ", ".join(
        f"{r.kind}:{r.qmd_path}" for r in missing
    )


# ---------------------------------------------------------------------------
# Predicate plumbing: `qmd:"..."` vs `qmd_vsearch:"..."` reach the
# right backend mode.
# ---------------------------------------------------------------------------


def test_parse_query_drives_runner_search(runner: QmdRunner, index: GridIndex) -> None:
    mode, inner = parse_query('qmd_vsearch:"earl grey"')
    assert mode == "vsearch"
    hits = runner.search(mode, inner, limit=3)
    assert hits
    rows = index.rows_for_hits(hits)
    assert rows
