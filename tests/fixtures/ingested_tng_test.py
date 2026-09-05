"""End-to-end pipeline test.

Drives the fixture-backed sync pipeline three times against a single
data root and asserts on what actually landed in the doltlite stores:

  Run 1: fresh data root — full ingest. Every expected provider must
         appear in the grid index, and signal's resume cursor must be
         recorded.
  Run 2: same data root, no flags — a steady-state re-run. Row counts
         must be IDENTICAL to run 1 (re-running must not duplicate or
         drop rows), signal's cursor must be untouched, and the
         `signal_snapshot_already_ingested` event must be emitted.
  Run 3: `--reset-and-redownload` — the cursor row is wiped and signal
         re-ingests, so the event must NOT be emitted; the store must
         then converge back to exactly the same contents.

The pytest invokes `run_sync_pipeline.py` as a subprocess (same
contract as the prior sh_test).

Stdlib `unittest` rather than third-party pytest to keep the
toolchain dep graph small — one self-contained test doesn't
justify wiring pytest through pip.parse.

Reading the stores directly: `//third-party/doltlite:doltlite` is a
bazel-built sqlite3-shell CLI linked against the same amalgamation the
pipeline uses (doltlite's on-disk format is not sqlite-file-compatible,
so stock sqlite3 cannot open these). It arrives through `data`, so this
stays hermetic — no system binary. Before it existed this test could
only grep the orchestrator's tracing events out of stderr, which meant
"the pipeline re-downloaded everything on run 2" was indistinguishable
from a pass. The stderr assertions are kept: they cover the one
transition (cursor hit / miss) that leaves no trace in the final state,
since run 3 wipes the cursor and then re-creates it.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import unittest
import uuid as uuidlib
from pathlib import Path

# Bazel runfiles layout: under bzlmod, the workspace dir is `_main`.
_BAZEL_WORKSPACE_DIR = "_main"

# Tracing event name emitted by signal download when the
# `ingested_backups` cursor short-circuits a fetch. Source of truth:
# providers/signal/src/download/mod.rs.
EV_SIGNAL_ALREADY_INGESTED = "signal_snapshot_already_ingested"

# Providers that must appear in `grid_rows` after a full fixture run.
#
# These are `grid_rows.provider` values, which are provider *types* and
# so don't always match the DAG step names (chatgpt-api reports
# `openai`, the carddav source reports `contacts`, the mbox source
# reports `jmap`).
#
# Keep this exhaustive over the sources run_sync_pipeline.py
# configures. The first draft had to omit `gitlab`, which turned out to
# be a dead fixture rather than an intended exclusion — its records
# spelled the project path `project_path` while every consumer had
# moved to `project_full_path`, so it silently produced nothing for
# three months. A source that stops producing rows should fail here,
# not disappear quietly.
# Providers still minting `grid_rows.uuid` values that are not UUIDs.
#
# Empty, and it must stay that way. It held `claude` and `openai`,
# the two that passed an upstream id through verbatim (or lightly
# prefixed) instead of deriving a v5: claude emitted
# `tu-{tool_use_id}` / `tr-{tool_use_id}` / `th-{msg_uuid}-{idx}` /
# `pdesc-{project_uuid}` for its structural blocks, and openai used
# ChatGPT's `conversation_id` / `message_id` directly. Both now mint
# through `datalib_id::entity_id`.
#
# Kept as an empty allowlist rather than deleted so the assertion below
# stays an exact-set comparison: a provider that starts leaking an
# upstream id into `grid_rows.uuid` fails here by name, and adding it
# to this set is a deliberate act someone has to justify in review
# rather than a silent drift.
NON_UUID_PK_PROVIDERS: frozenset[str] = frozenset()

# ── datalib_id recipe, reimplemented ────────────────────────────────
#
# Deliberately a second implementation rather than a call into the Rust
# one. Asserting `entity_id(...) == uuid` by invoking the same function
# that produced the uuid proves only that the function is
# deterministic. Recomputing it here from the columns the renderer
# stored pins the actual contract: that `upstream_id` and
# `upstream_entity_kind` really are the inputs `uuid` was derived from,
# and that the wire format hasn't drifted. A renderer that stamps a
# plausible-looking but wrong backpointer fails here and nowhere else.
#
# Source of truth: datalib/backend/id/src/lib.rs. If that file's
# namespace, separator or component order changes, this must change with
# it — which is the point.
DATALIB_ID_NS = uuidlib.UUID(bytes=b"datalib-id-ns-v1")
ID_SEP = "\x1f"

# Which `Scope` variant each ported provider mints under. Scope is a
# provider-level design decision, not a per-row one, so a table is the
# right shape — and it has to live here because `upstream_scope` is NULL
# for both `ProviderGlobal` and `Content`, making the two
# indistinguishable from the row alone.
#
# Grow this as providers are ported; a provider absent from it is
# skipped by the round-trip check below, and `PORTED_PROVIDERS` keeps
# that from being silent.
SCOPE_TAG_BY_PROVIDER = {
    "claude": ("pg", ""),
    "openai": ("pg", ""),
    # Slack scopes on `team_id`, which the row carries in `account`.
    # Resolved per-row rather than from a constant here — see
    # `_roundtrip_failures`.
    "slack": ("up", None),
}

# Providers whose rows MUST round-trip. Separate from the table above so
# a typo in a provider name shows up as "no rows checked" rather than as
# a silent pass.
PORTED_PROVIDERS = frozenset({"claude", "openai", "slack"})


def datalib_entity_id(provider, scope_tag, scope_val, entity_kind, natural_key):
    """UUIDv5 over the five-component recipe, joined with \x1f."""
    name = ID_SEP.join([provider, scope_tag, scope_val, entity_kind, natural_key])
    return str(uuidlib.uuid5(DATALIB_ID_NS, name))


# Canonical 8-4-4-4-12 hex form. Deliberately not a version-specific
# pattern: the point is that the column holds an opaque fixed-width
# identifier we minted, not which v5 recipe produced it.
UUID_SQL_REGEX = (
    "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
)

EXPECTED_PROVIDERS = frozenset(
    {
        "claude",
        "beeper",
        "contacts",
        "github",
        "gitlab",
        "google_takeout",
        "jmap",
        "linkedin",
        "notion",
        "openai",
        # The only file-backed source in this fixture that renders.
        # fsindex and lightroom scan trees too but produce no rows;
        # `pdf` converts what it scans, so it must show up here.
        "pdf",
        "signal",
        "slack",
        "sms_backup_restore",
        "whatsapp",
        # This fixture seeds yolink's raw store with
        # `yolink-make-fixture` rather than downloading it, so the
        # generated config has no yolink download step. Rows here prove
        # the render step ran over that store — see
        # run_sync_pipeline.py's PRESEEDED_RAW.
        "yolink",
    }
)


def _sql_in(names) -> str:
    """Render a set of provider names as a SQL IN-list literal.

    Provider names are compile-time constants in this file, never user
    input, so quoting them inline is safe; the doltlite CLI takes one
    SQL string and has nowhere to bind parameters.
    """
    return ", ".join(f"'{n}'" for n in sorted(names))


def _argv():
    """sys.argv layout, matching `args = [...]` in BUILD.bazel:

    [0]: run_sync_pipeline.py path
    [1]: datalib_dag path
    [2]: datalib_step path
    [3]: signal_make_fixture path
    [4]: whatsapp_make_fixture path
    [5]: doltlite CLI path
    [6]: --now stamp
    [7..]: fixture paths, forwarded verbatim as run_sync_pipeline.py's
           args 7..22 (the last two being yolink-make-fixture + its
           spec; see that script's docstring for why they're appended)
    """
    return sys.argv[1:]


class IngestedTngPipelineTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        argv = _argv()
        cls.driver_script = argv[0]
        cls.dag_bin = argv[1]
        cls.step_bin = argv[2]
        cls.signal_bin = argv[3]
        cls.whatsapp_bin = argv[4]
        cls.doltlite_bin = argv[5]
        cls.now = argv[6]
        cls.fixture_paths = argv[7:]

        cls.workspace = Path(os.environ["TEST_TMPDIR"]) / "sync_workspace"
        cls.workspace.mkdir(parents=True, exist_ok=True)

        runfiles_root = os.environ.get("TEST_SRCDIR")
        if runfiles_root:
            cls.cwd = Path(runfiles_root) / _BAZEL_WORKSPACE_DIR
        else:
            cls.cwd = Path.cwd()

    # ── doltlite store access ───────────────────────────────────────

    @property
    def _index_db(self) -> Path:
        """The grid index the `grid_index` fan-in step writes."""
        return self.workspace / "unified_index" / "grid" / "db.doltlite_db"

    @property
    def _signal_entities_db(self) -> Path:
        """Signal's raw entity store, which holds `ingested_backups`."""
        return self.workspace / "signal" / "raw" / "entities.doltlite_db"

    def _query(self, db: Path, sql: str) -> list[str]:
        """Run one SQL statement, returning stripped non-empty lines."""
        self.assertTrue(db.is_file(), f"expected a doltlite store at {db}")
        result = subprocess.run(
            [str(Path(self.cwd) / self.doltlite_bin), str(db), sql],
            check=True,
            capture_output=True,
            text=True,
        )
        return [ln.strip() for ln in result.stdout.splitlines() if ln.strip()]

    def _scalar(self, db: Path, sql: str) -> str:
        rows = self._query(db, sql)
        self.assertEqual(len(rows), 1, f"expected one row from {sql!r}, got {rows}")
        return rows[0]

    def _count(self, db: Path, table: str) -> int:
        return int(self._scalar(db, f"SELECT COUNT(*) FROM {table};"))

    def _index_shape(self) -> dict[str, int]:
        """The index contents that a re-run must leave unchanged."""
        return {
            "grid_rows": self._count(self._index_db, "grid_rows"),
            "markdowns": self._count(self._index_db, "markdowns"),
        }

    def _index_ids(self) -> dict[str, list[str]]:
        """Every id in the index, so a re-run can be checked id-by-id.

        Counts are not enough, and the gap is specific rather than
        theoretical. `apply_markdown` deletes `grid_rows` by
        `markdown_uuid` and then re-inserts, so within a markdown whose
        id is unchanged, every row id could move and the count would not
        budge — the delete takes the old ones away and the insert puts
        the same number back. Only `markdown_uuid` instability shows up
        in a count, and then only because the delete misses the old rows
        and they accumulate.

        That matters because ids are the durable thing: they are what
        `feedback.target_uuids` stores unqualified and what a
        `/chat/{uuid}` URL that has been handed out resolves through. An
        id that silently moves on re-ingest breaks both, and nothing
        else in this suite would notice — the checked-in golden pins row
        ids against a *snapshot*, which catches a change between commits
        but not one between two runs of the same build.

        Sorted rather than a set so a failure diff reads in a stable
        order.
        """
        return {
            "grid_rows": sorted(
                self._query(self._index_db, "SELECT uuid FROM grid_rows;")
            ),
            "markdowns": sorted(
                self._query(self._index_db, "SELECT markdown_uuid FROM markdowns;")
            ),
        }

    def _providers(self) -> frozenset[str]:
        return frozenset(
            self._query(self._index_db, "SELECT DISTINCT provider FROM grid_rows;")
        )

    def _pdf_shape(self) -> dict[str, int]:
        """The `pdf` source's contribution, by grid_rows kind.

        Pinned rather than merely non-empty because this source is the
        one whose output feeds the qmd index by *page*: silent growth
        here shows up as a slower fixture build for everyone, and a
        silent drop to zero would mean PDFs stopped being searchable
        without any test going red.
        """
        rows = self._query(
            self._index_db,
            "SELECT kind, COUNT(*) FROM grid_rows WHERE provider = 'pdf' "
            "GROUP BY kind ORDER BY kind;",
        )
        out: dict[str, int] = {}
        for r in rows:
            kind, n = r.rsplit("|", 1)
            out[kind] = int(n)
        return out

    def _qmd_path_mismatches(self) -> list[str]:
        """Rows whose `qmd_path` disagrees with their markdown's path.

        The two columns name the same file and must name it the same
        way: `GridIndex` (unified_index/src/qmd/mapping.rs) keys grid
        rows by `norm_path(qmd_path)` and looks each qmd hit up by the
        hit's own data-root-relative path — the same spelling
        `apply_one` stores in `markdowns.md_path`. A provider that
        stamps any other form still renders correct markdown and still
        gets indexed by qmd, so nothing else goes red; its documents
        just vanish from free-text search, because every hit in them
        resolves to zero grid rows and is dropped. `pdf` shipped that
        way — it wrote the out-dir-relative `docs/<blake3>.md` while
        every other provider wrote `<stanza>/rendered_md/...`.

        Rows with no markdown row to join against are left to the
        NULL-`qmd_path` check above; this one is about disagreement.
        """
        return self._query(
            self._index_db,
            "SELECT DISTINCT g.provider, g.qmd_path, m.md_path "
            "FROM grid_rows g JOIN markdowns m "
            "  ON m.markdown_uuid = g.markdown_uuid "
            "WHERE m.md_path IS NOT NULL "
            "  AND (g.qmd_path IS NULL OR g.qmd_path <> m.md_path);",
        )

    def _duplicate_uuids(self) -> list[str]:
        """`grid_rows.uuid` values held by more than one row.

        `grid_rows` declares `PRIMARY KEY (uuid)`, so this can only ever
        come back empty from a store the index actually wrote — which is
        exactly why it is worth asserting from the outside. A duplicate
        here would mean the PK is not being enforced by doltlite, and
        every collision guarantee in the pipeline rests on it being
        enforced. Cheap check, load-bearing assumption.
        """
        return self._query(
            self._index_db,
            "SELECT uuid, COUNT(*) FROM grid_rows GROUP BY uuid HAVING COUNT(*) > 1;",
        )

    def _non_uuid_pk_providers(self) -> frozenset[str]:
        """Providers whose `grid_rows.uuid` is not UUID-shaped.

        `uuid` is the primary key of the union table, the `id=` /
        `data-section-uuid` anchor the renderer bakes into the markdown
        body, and the value `feedback.target_uuids` stores unqualified.
        A provider that passes an upstream string through instead of
        minting its own is one upstream id-reuse away from colliding
        with another source in a keyspace nothing can disambiguate
        after the fact.

        Asserted as an exact set against `NON_UUID_PK_PROVIDERS`, in
        both directions: a *new* offender fails, and so does a provider
        left on the allowlist after it has been cleaned up — so the
        list cannot rot into a permanent exemption.
        """
        return frozenset(
            self._query(
                self._index_db,
                "SELECT DISTINCT provider FROM grid_rows "
                f"WHERE uuid NOT REGEXP '{UUID_SQL_REGEX}';",
            )
        )

    def _cross_source_shared_markdowns(self) -> list[str]:
        """`markdown_uuid`s claimed by more than one `source_name`.

        The failure this catches is silent by construction:
        `apply_markdown` DELETEs `grid_rows` by `markdown_uuid` before
        inserting, so when two configured sources mint the same id the
        sidecar applied second erases the first one's rows and rewrites
        the `markdowns` row with its own `md_path` and `source_name`.
        The run reports success and the row count looks plausible — one
        source has simply vanished from the index.

        `IdClaims` in `datalib_etl::grid_index` now fails the run when
        it sees this, so in a green pipeline this is a second, external
        witness rather than the primary check. It is asserted here
        anyway because it reads the store rather than the code path:
        if the in-process tracker is ever bypassed (a caller reaching
        `apply_one` directly, a partial re-index), this still sees it.
        """
        return self._query(
            self._index_db,
            "SELECT markdown_uuid, COUNT(DISTINCT source_name) FROM markdowns "
            "GROUP BY markdown_uuid HAVING COUNT(DISTINCT source_name) > 1;",
        )

    def _roundtrip_failures(self) -> list[str]:
        """Ported rows whose backpointer does not regenerate their uuid.

        For every row from a provider in `SCOPE_TAG_BY_PROVIDER`,
        recompute `entity_id(provider, scope, upstream_entity_kind,
        upstream_id)` and compare to the stored `uuid`. A mismatch
        means the backpointer is decorative — it names something that
        would not produce this row — and the round-trip back to the
        upstream API is broken in a way nothing else would notice,
        because both columns still look perfectly plausible.
        """
        rows = self._query(
            self._index_db,
            "SELECT provider, uuid, IFNULL(upstream_entity_kind, ''), "
            "       IFNULL(upstream_id, ''), IFNULL(upstream_scope, '') "
            "FROM grid_rows "
            f"WHERE provider IN ({_sql_in(SCOPE_TAG_BY_PROVIDER)}) "
            "ORDER BY uuid;",
        )
        failures = []
        for line in rows:
            provider, row_uuid, entity_kind, native_id, row_scope = line.split("|", 4)
            if not entity_kind or not native_id:
                failures.append(
                    f"{provider} {row_uuid}: ported provider left "
                    f"upstream_entity_kind={entity_kind!r} "
                    f"upstream_id={native_id!r}"
                )
                continue
            scope_tag, scope_val = SCOPE_TAG_BY_PROVIDER[provider]
            # An `Upstream` scope's value is per-row, so the table
            # stores None and the row supplies it. A ported provider
            # that scopes upstream but leaves `upstream_scope` empty is
            # itself the bug.
            if scope_val is None:
                if not row_scope:
                    failures.append(
                        f"{provider} {row_uuid}: upstream-scoped but "
                        f"upstream_scope is empty"
                    )
                    continue
                scope_val = row_scope
            want = datalib_entity_id(
                provider, scope_tag, scope_val, entity_kind, native_id
            )
            if want != row_uuid:
                failures.append(
                    f"{provider} {row_uuid}: ({entity_kind!r}, "
                    f"{native_id!r}) regenerates {want}"
                )
        return failures

    def _ported_provider_row_counts(self) -> dict[str, int]:
        out: dict[str, int] = {}
        for line in self._query(
            self._index_db,
            "SELECT provider, COUNT(*) FROM grid_rows "
            f"WHERE provider IN ({_sql_in(PORTED_PROVIDERS)}) "
            "GROUP BY provider;",
        ):
            name, n = line.rsplit("|", 1)
            out[name] = int(n)
        return out

    def _signal_cursor(self) -> list[str]:
        """Signal's `ingested_backups` rows as `<snapshot_dir>|<blake3>`."""
        return self._query(
            self._signal_entities_db,
            "SELECT snapshot_dir, blake3 FROM ingested_backups ORDER BY snapshot_dir;",
        )

    # ── pipeline driver ────────────────────────────────────────────

    def _render_stores(self) -> dict[str, int]:
        """Per-source render stores, as `source -> grid_rows count`.

        Each source's render step writes
        `<source>/rendered_md/indexed_markdown.doltlite_db` holding the
        same rows that stack into the unified index — same derived DDL,
        same writer (`grid_index::apply_one`). Asserted from the outside
        with the doltlite CLI, so a store that exists but is empty, or
        holds a shape the CLI cannot read, fails here rather than
        silently indexing nothing.
        """
        out: dict[str, int] = {}
        for store in sorted(
            self.workspace.glob("*/rendered_md/indexed_markdown.doltlite_db")
        ):
            source = store.parent.parent.name
            out[source] = self._count(store, "grid_rows")
        return out

    def _render_problems(self) -> dict[str, list[str]]:
        """Per-source `render_problems` rows, as `source -> [summary…]`.

        Each entry is `<outcome>|<uuid>|<problems-json>` so a failure
        message names what was dropped rather than only how many.
        """
        out: dict[str, list[str]] = {}
        for store in sorted(
            self.workspace.glob("*/rendered_md/indexed_markdown.doltlite_db")
        ):
            rows = self._query(
                store,
                "SELECT outcome, uuid, problems FROM render_problems ORDER BY uuid;",
            )
            if rows:
                out[store.parent.parent.name] = rows
        return out

    def _run_pipeline(self, *, reset: bool) -> subprocess.CompletedProcess:
        env = {**os.environ}
        if reset:
            env["INGESTED_TNG_RESET"] = "1"
        else:
            env.pop("INGESTED_TNG_RESET", None)
        argv = [
            sys.executable,
            self.driver_script,
            self.dag_bin,
            self.step_bin,
            self.signal_bin,
            self.whatsapp_bin,
            self.now,
            str(self.workspace),
            *self.fixture_paths,
        ]
        result = subprocess.run(
            argv,
            check=False,
            cwd=str(self.cwd),
            env=env,
            capture_output=True,
            text=True,
        )
        # Print the captured streams so a test failure leaves the
        # orchestrator's output in the test's own log for debugging.
        #
        # `check=False` plus an explicit raise below, rather than
        # `check=True`: raising inside `subprocess.run` skipped these two
        # writes entirely, so the one case this printing exists for — the
        # pipeline exiting non-zero — was the one case that printed
        # nothing, and the failure reached the log as a bare
        # `CalledProcessError` naming a 25-argument command line.
        sys.stdout.write(result.stdout)
        sys.stderr.write(result.stderr)
        result.check_returncode()
        return result

    def test_pipeline_resume_and_reset(self) -> None:
        # --- Run 1: fresh workspace. Full ingest.
        run1 = self._run_pipeline(reset=False)
        self.assertNotIn(
            EV_SIGNAL_ALREADY_INGESTED,
            run1.stderr,
            "run 1 is a fresh ingest — signal must NOT report already_ingested",
        )

        shape1 = self._index_shape()
        ids1 = self._index_ids()
        self.assertGreater(
            shape1["grid_rows"], 0, "run 1 must load grid rows into the index"
        )
        self.assertGreater(
            shape1["markdowns"], 0, "run 1 must load markdowns into the index"
        )
        # Every source that renders must be represented. An exact set
        # comparison (not a subset check) is deliberate: it catches a
        # source silently dropping out of the pipeline, which is the
        # failure this test previously could not see at all.
        self.assertEqual(
            self._providers(),
            EXPECTED_PROVIDERS,
            "grid_rows providers after a full run",
        )

        # PDFs specifically: 4 renderable documents, 5 pages between
        # them (the scanned blueprints are recorded but not rendered,
        # and the corrupt file is skipped). The mixed hull survey is one
        # of the four and contributes one page, not two — its image-only
        # second page becomes a note in the markdown rather than a row.
        # Every page row must carry a `qmd_path`, since that column is
        # what lets a qmd hit resolve back to a grid row — a page indexed
        # without one is findable by search but unreachable from the UI.
        self.assertEqual(
            self._pdf_shape(),
            {"PDF Document": 4, "PDF Page": 5},
            "pdf grid_rows shape",
        )
        self.assertEqual(
            self._scalar(
                self._index_db,
                "SELECT COUNT(*) FROM grid_rows "
                "WHERE provider = 'pdf' AND qmd_path IS NULL;",
            ),
            "0",
            "every pdf row needs a qmd_path to be reachable from search",
        )
        # Cross-provider: `grid_rows.qmd_path` must be byte-equal to
        # its markdown's `markdowns.md_path`. Asserted over the whole
        # index rather than per-provider so a new source inherits the
        # check for free — this is the invariant the qmd hit→row
        # mapping is built on.
        self.assertEqual(
            self._qmd_path_mismatches(),
            [],
            "every grid row's qmd_path must equal its markdown's md_path",
        )
        # ...and the join must actually have matched something, or the
        # emptiness above would prove nothing.
        self.assertGreater(
            int(
                self._scalar(
                    self._index_db,
                    "SELECT COUNT(*) FROM grid_rows g JOIN markdowns m "
                    "  ON m.markdown_uuid = g.markdown_uuid "
                    "WHERE g.provider = 'pdf' AND m.md_path IS NOT NULL;",
                )
            ),
            0,
            "the qmd_path/md_path join must cover pdf rows",
        )

        # ── the per-source render stores ────────────────────────
        # Each source's render step writes its rows into
        # `<source>/rendered_md/indexed_markdown.doltlite_db` — the same
        # rows, same derived DDL and same writer as the unified index,
        # so "what stacks into the index" needs no second projection to
        # keep in step.
        #
        # Asserted against the index rather than against a number: a
        # store that silently wrote nothing would still pass a
        # "file exists" check, and a bare row count would drift with
        # every fixture tweak.
        stores = self._render_stores()
        self.assertNotEqual(stores, {}, "every rendering source must write a store")
        for source, rows in stores.items():
            self.assertGreater(rows, 0, f"{source}'s render store holds no rows")
        self.assertEqual(
            sum(stores.values()),
            self._count(self._index_db, "grid_rows"),
            "the stores and the index must hold the same number of rows — "
            f"per-source: {stores}",
        )

        # Nothing in the TNG fixture may land in the problem sink.
        #
        # This is the assertion that keeps the sink honest in both
        # directions. Every renderer now drops-and-records a row it
        # cannot build instead of failing the step, which is what stops
        # one bad record from poisoning `grid_index` for every other
        # source — but the same change means a projection that quietly
        # started dropping rows would no longer show up as a failure
        # anywhere. Here it does: the fixture is known-good, so any
        # `render_problems` row is a regression, and the message names
        # the row and the reason rather than just a count.
        self.assertEqual(
            self._render_problems(),
            {},
            "the TNG fixture must render clean; a row here means a "
            "projection started dropping or nulling data",
        )

        # ── id-space guardrails ─────────────────────────────────
        # Every row uuid is unique. The PK makes this true by
        # construction; asserting it from outside the writer is what
        # confirms the PK is real in doltlite, which is the assumption
        # the whole collision story rests on.
        self.assertEqual(self._duplicate_uuids(), [], "grid_rows.uuid must be unique")
        # No two sources may claim one markdown. This is the overlap
        # that used to erase a source's rows without an error.
        self.assertEqual(
            self._cross_source_shared_markdowns(),
            [],
            "a markdown_uuid claimed by two source_names means one "
            "source's rows were silently overwritten",
        )
        # Exactly the known offenders still mint non-UUID primary keys.
        # Equality (not a subset check) in both directions: a new
        # offender fails, and so does a stale allowlist entry.
        self.assertEqual(
            self._non_uuid_pk_providers(),
            NON_UUID_PK_PROVIDERS,
            "providers minting non-UUID grid_rows.uuid values; shrink "
            "NON_UUID_PK_PROVIDERS as each is ported, and do not grow it",
        )
        # Guard against the regex silently matching nothing — a typo
        # that made it match zero rows would satisfy the empty-set
        # assertion above for entirely the wrong reason. Every row must
        # be UUID-shaped, and there must be rows.
        self.assertEqual(
            self._scalar(
                self._index_db,
                f"SELECT COUNT(*) FROM grid_rows WHERE uuid REGEXP '{UUID_SQL_REGEX}';",
            ),
            str(shape1["grid_rows"]),
            "every grid_rows.uuid must match the UUID shape — if this "
            "equals 0 the regex itself is broken, not the data",
        )

        # Ported providers must round-trip: the backpointer columns
        # regenerate the row's own uuid. See `datalib_entity_id` for why
        # this is reimplemented rather than delegated.
        self.assertEqual(
            self._roundtrip_failures(),
            [],
            "a ported provider's (upstream_entity_kind, upstream_id) "
            "must regenerate its uuid",
        )
        # ...and every ported provider must actually have rows, or the
        # emptiness above proves nothing.
        counts = self._ported_provider_row_counts()
        self.assertEqual(
            frozenset(counts),
            PORTED_PROVIDERS,
            "every ported provider must contribute rows to check",
        )
        for name, n in counts.items():
            self.assertGreater(n, 0, f"{name} must have rows")

        # The index is committed, so its version history is non-empty.
        self.assertGreater(
            int(self._scalar(self._index_db, "SELECT COUNT(*) FROM dolt_log;")),
            0,
            "grid_index must commit, leaving a dolt_log entry",
        )
        # Signal recorded exactly one snapshot in its resume cursor.
        cursor1 = self._signal_cursor()
        self.assertEqual(
            len(cursor1), 1, f"expected one ingested_backups row, got {cursor1}"
        )

        # --- Run 2: same data root, no flags. Signal's
        # ingested_backups cursor MUST short-circuit the second
        # download.
        run2 = self._run_pipeline(reset=False)
        self.assertIn(
            EV_SIGNAL_ALREADY_INGESTED,
            run2.stderr,
            "run 2 must hit signal's ingested_backups cursor and emit "
            f"the {EV_SIGNAL_ALREADY_INGESTED!r} event",
        )
        # The load-bearing state assertion: a steady-state re-run is
        # idempotent. Re-rendering and re-loading the same documents
        # must not duplicate rows (upserts keyed correctly) or drop
        # them (a cursor short-circuit skipping too much).
        self.assertEqual(
            self._index_shape(), shape1, "run 2 must leave the index unchanged"
        )
        # Ids specifically, not just how many of them there are.
        #
        # Note what this does and does not prove. A steady-state re-run
        # skips every unchanged markdown on its `source_fingerprint`, so
        # almost nothing is re-rendered and almost no id is re-derived —
        # this is "the index sat still", which is worth pinning but is
        # weaker than it looks. Re-derivation is tested by run 4.
        self.assertEqual(
            self._index_ids(),
            ids1,
            "run 2 must leave every id in place: a moved uuid orphans "
            "filed feedback and breaks any /chat/{uuid} URL already "
            "handed out",
        )
        self.assertEqual(self._providers(), EXPECTED_PROVIDERS, "run 2 providers")
        self.assertEqual(
            self._signal_cursor(), cursor1, "run 2 must not disturb signal's cursor"
        )

        # --- Run 3: --reset-and-redownload. The flag wipes signal's
        # ingested_backups row before fetch, so the cursor MUST NOT
        # short-circuit. (If --reset-and-redownload were silently
        # dropped, this run would behave like run 2.)
        run3 = self._run_pipeline(reset=True)
        self.assertNotIn(
            EV_SIGNAL_ALREADY_INGESTED,
            run3.stderr,
            "after --reset-and-redownload wipes ingested_backups, "
            "signal must NOT report already_ingested on run 3",
        )
        # A reset re-downloads from scratch and must converge to the
        # same store, not to a duplicated or partial one.
        self.assertEqual(
            self._index_shape(),
            shape1,
            "run 3 (--reset-and-redownload) must converge to the same index",
        )
        # The strongest form of the check: run 3 threw the raw stores
        # away and fetched again, so these ids were derived a second
        # time from a fresh download rather than read back off an
        # existing index.
        self.assertEqual(
            self._index_ids(),
            ids1,
            "run 3 re-downloaded from scratch and must still mint byte-identical ids",
        )
        self.assertEqual(self._providers(), EXPECTED_PROVIDERS, "run 3 providers")
        # The cursor is wiped mid-run, so by the end it must be back —
        # same snapshot, same fingerprint.
        self.assertEqual(
            self._signal_cursor(),
            cursor1,
            "run 3 must re-record the same signal snapshot fingerprint",
        )

        # --- Run 4: a brand-new data root over the same fixture.
        #
        # The one run that actually re-derives every id. Runs 2 and 3
        # both skip on `source_fingerprint`, so no id in them is
        # recomputed — they pin that the index sits still, not that the
        # recipes are reproducible. Wiping the root removes every
        # fingerprint along with the index, so every renderer runs again
        # from scratch.
        #
        # What this catches: a recipe that reads the clock, an RNG, or
        # anything whose order is not fixed (HashMap iteration, readdir)
        # — and a renderer that reads an id back off the existing index
        # instead of deriving it, since there is no index to read.
        #
        # What it does NOT catch, because both runs see them identically:
        #
        #   * an id derived from `config.toml`. The driver regenerates
        #     the same step ids every run, so a recipe keyed on the
        #     source name is byte-identical here. Catching that needs a
        #     run over the same fixture under *different* step ids,
        #     which the driver cannot do today — its source names are a
        #     hardcoded dict and three raw stores are seeded at paths
        #     built from them. What stands in for it is
        #     `SCOPE_TAG_BY_PROVIDER`: a ported provider whose ids
        #     depend on configuration has to declare that as the `src`
        #     scope and store the value in `upstream_scope`, or the
        #     round-trip check above fails it.
        #   * an id derived from the data-root path, which is the same
        #     directory both times.
        #   * anything that varies between upstream *responses* rather
        #     than within one. The fixture replays fixed tapes, so the
        #     two known instabilities — slack's count-only reactions and
        #     claude's positional thinking-block index — cannot
        #     surface here. See `docs/dev/entity_ids.md`.
        shutil.rmtree(self.workspace)
        self.workspace.mkdir(parents=True, exist_ok=True)
        self._run_pipeline(reset=False)
        self.assertEqual(
            self._index_shape(), shape1, "run 4 must rebuild the same index"
        )
        self.assertEqual(
            self._index_ids(),
            ids1,
            "a fresh data root over the same fixture must mint "
            "byte-identical ids — an id recipe that reads the clock, the "
            "scan order, or the config would differ here and nowhere else",
        )


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
