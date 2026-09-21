"""The provider contract, checked against every render provider's TNG store.

`docs/dev/plans/one_mode.md` §"Layer 2: the provider contract". The one
clause worth a machine check is the hard one: *the changed set is
complete* — for any change to the raw store, every document whose output
differs is one the provider's diff scan names. A miss is silent (the run
succeeds, the document is simply old), which is why it gets a test and
not a review checklist.

The check is the property itself, incremental ≡ cold: run the fixture
pipeline once, then for every source and every table of its raw store,
mutate the table in a scratch copy, commit, render the source
*incrementally* (the copy carries the render store and its cursor) and
render it *cold* (a copy with no render store at all), and compare the
two render stores' logical content. Per table: delete every row, and
append a marker to every text column and every top-level string in
every JSON payload — the first is what a source losing an entity looks
like, the second what an edit looks like; then, on one row (the first
by primary key), the same edit, a deletion, and an insertion of a copy
under a fresh key — an edit, a loss and an arrival of one entity, with
the incremental run also checked to have rendered exactly the documents
that declared the row; and for a table that points at blob bytes, two
rows' digests swapped — an attachment whose bytes changed. A failure
names the source, the table and the mutation, which is the fix in one
line.

Runs `datalib-step` the way the runner would, with the params the
generated config gives the step, so the provider code under test is
exactly the shipped code.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tomllib
import unittest
from pathlib import Path
from typing import NamedTuple

_BAZEL_WORKSPACE_DIR = "_main"

# No spawn here should take more than a second or two; one that takes
# this long has hung, and the failure names it. The test has three
# times reached bazel's 900 s ceiling on CI with nothing written, which
# is the one outcome this test must never produce again.
_SPAWN_TIMEOUT_SECS = 120

# Bookkeeping the ingest side owns and render never reads. Mutating it
# proves nothing, and deleting `sync_runs` would only exercise the
# framework's own skip. `_datalib_meta` is which build wrote the store.
_SKIP_TABLES = (
    "_datalib_meta",
    "sync_runs",
    "sync_scope_state",
    "sync_scope_config",
    "ingested_files",
)
_SKIP_SUFFIXES = ("_bookkeeping",)

# Mutations under which a provider is known not to keep the contract
# today, each with why. The list is exact: a fixed provider makes its
# entry fail the run until it is removed, so it can only shrink. A new
# entry needs a reason, which is the finding.
KNOWN_GAPS: dict[str, str] = {
    # ── a failed render, on purpose ──
    # The edit points the scan root at a directory that does not exist,
    # so every conversion fails. Cold then has no pages; incremental
    # keeps the last good ones, because a document whose conversion
    # failed is left undeclared rather than swept — a stale page over a
    # missing one, since nothing would bring it back until its inputs
    # move again. Not a gap in what the provider declares.
    "tng_pdfs: tweak pdf_scan_meta": "a failed conversion keeps its last page",
}

# Columns of the render store whose value is a stamp of *when* rather
# than *what*: identical content renders them differently on every run.
_VOLATILE = {
    "problems": ("first_seen_at_utc", "last_seen_at_utc", "tz_offset"),
    "render_cursor": ("rendered_at_utc", "tz_offset", "raw_commit"),
}


class RenderContractTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        argv = sys.argv[1:]
        cls.driver_script = argv[0]
        cls.dag_bin = argv[1]
        cls.step_bin = argv[2]
        cls.signal_bin = argv[3]
        cls.whatsapp_bin = argv[4]
        cls.doltlite_bin = argv[5]
        cls.now = argv[6]
        cls.fixture_paths = argv[7:]
        runfiles_root = os.environ.get("TEST_SRCDIR")
        cls.cwd = (
            Path(runfiles_root) / _BAZEL_WORKSPACE_DIR if runfiles_root else Path.cwd()
        )
        # `bazelisk run` (outside the sandbox, for a look at a failing
        # scratch root) has no TEST_TMPDIR; `RENDER_CONTRACT_TMP` stands in.
        cls.tmp = Path(
            os.environ.get("TEST_TMPDIR") or os.environ["RENDER_CONTRACT_TMP"]
        )
        cls.workspace = cls.tmp / "sync_workspace"
        cls.workspace.mkdir(parents=True, exist_ok=True)
        # Bazel fails a sharded test whose runner never touched this.
        if status := os.environ.get("TEST_SHARD_STATUS_FILE"):
            Path(status).touch()
        cls._run_pipeline()
        cls.config = tomllib.loads(
            (cls.workspace / "dag.toml").read_text(encoding="utf-8")
        )

    # ── running things ──────────────────────────────────────────────

    @classmethod
    def _run_pipeline(cls) -> None:
        argv = [
            sys.executable,
            cls.driver_script,
            cls.dag_bin,
            cls.step_bin,
            cls.signal_bin,
            cls.whatsapp_bin,
            cls.now,
            str(cls.workspace),
            *cls.fixture_paths,
        ]
        result = subprocess.run(
            argv,
            check=False,
            cwd=str(cls.cwd),
            capture_output=True,
            text=True,
            timeout=_SPAWN_TIMEOUT_SECS * 5,
        )
        if result.returncode != 0:
            sys.stdout.write(result.stdout)
            sys.stderr.write(result.stderr)
            result.check_returncode()

    def _doltlite(
        self, db: Path, sql: str, *, must_succeed: bool = True
    ) -> list[str] | None:
        """One statement (or a `;`-joined few) against a store; the rows,
        or `None` when it failed and the caller said that may happen."""
        try:
            result = subprocess.run(
                [str(self.cwd / self.doltlite_bin), str(db), sql],
                check=False,
                capture_output=True,
                text=True,
                timeout=_SPAWN_TIMEOUT_SECS,
            )
        except subprocess.TimeoutExpired:
            self.fail(
                f"doltlite hung for {_SPAWN_TIMEOUT_SECS}s on {db}:\n  {sql[:300]}"
            )
        if result.returncode != 0:
            if not must_succeed:
                self.last_refusal = result.stderr.strip().splitlines()[-1:]
                return None
            self.fail(f"doltlite failed on {db.name}:\n  {sql[:300]}\n{result.stderr}")
        return [ln for ln in result.stdout.splitlines() if ln.strip()]

    def _rows(self, db: Path, sql: str) -> list[str]:
        rows = self._doltlite(db, sql)
        assert rows is not None
        return rows

    def _render_step(self, data_root: Path, group: str) -> str | None:
        """One `datalib-step render_markdown` for `group`, as the runner
        would invoke it: the group's type and the step's params from the
        generated config, its ingest tree as the one input. `None` on
        success, else the step's error line — a provider may refuse a
        raw state on purpose (an enum value it does not know), and the
        contract then only asks that cold refuse it the same way."""
        gtype = next(g["type"] for g in self.config["groups"] if g["id"] == group)
        step = next(
            s
            for s in self.config["steps"]
            if s["group"] == group and s["function"] == "render_markdown"
        )
        env = {
            **os.environ,
            "DATALIB_DAG_DATA_ROOT": str(data_root),
            "DATALIB_DAG_STEP": f"{group}/render_markdown",
            "DATALIB_DAG_GROUP": group,
            "DATALIB_DAG_GROUP_TYPE": gtype,
            "DATALIB_DAG_FUNCTION": "render_markdown",
            "DATALIB_DAG_INPUTS": f"{group}/ingest",
            "DATALIB_DAG_NOW": self.now,
        }
        argv = [str(self.cwd / self.step_bin)]
        if step.get("params"):
            params_file = data_root / f"{group}.render.params.json"
            params_file.write_text(json.dumps(step["params"]))
            argv += ["--params-file", str(params_file)]
        try:
            result = subprocess.run(
                argv,
                check=False,
                cwd=str(self.cwd),
                env=env,
                capture_output=True,
                text=True,
                timeout=_SPAWN_TIMEOUT_SECS,
            )
        except subprocess.TimeoutExpired:
            self.fail(
                f"datalib-step render_markdown for {group} hung for "
                f"{_SPAWN_TIMEOUT_SECS}s in {data_root}"
            )
        if result.returncode == 0:
            return None
        causes = [
            ln
            for ln in result.stderr.splitlines()
            if ln.startswith(("error:", "caused by:"))
        ]
        return causes[-1] if causes else f"exit {result.returncode}"

    # ── the raw store ───────────────────────────────────────────────

    def _entity_tables(self, db: Path) -> list[tuple[str, list[tuple[str, str, bool]]]]:
        """`(table, [(column, declared type, is primary key)…])` for every
        entity table the render side might read, populated ones only."""
        names = self._rows(
            db,
            "SELECT name FROM sqlite_master WHERE type = 'table' "
            "AND name NOT LIKE 'dolt_%' AND name NOT LIKE 'sqlite_%' ORDER BY name;",
        )
        candidates = [
            t for t in names if t not in _SKIP_TABLES and not t.endswith(_SKIP_SUFFIXES)
        ]
        if not candidates:
            return []
        # The count and the columns of every table in one spawn; the
        # first row under each sentinel is the count.
        by_table = self._split_by_sentinel(
            self._rows(
                db,
                " ".join(
                    f"SELECT '#{t}'; SELECT COUNT(*) FROM {t}; "
                    f"SELECT name, type, pk FROM pragma_table_info('{t}');"
                    for t in candidates
                ),
            )
        )
        out = []
        for t in candidates:
            count, *pragma = by_table[t]
            if count == "0":
                continue
            cols = []
            for line in pragma:
                name, typ, pk = line.split("|")
                cols.append((name, typ.upper(), pk != "0"))
            out.append((t, cols))
        return out

    @staticmethod
    def _identity_like(name: str) -> bool:
        """A column or JSON key that names something rather than saying
        something: an id, a reference, a digest, a stamp. Editing one
        makes a raw state ingest could never have produced — a payload
        whose `id` disagrees with its row's — which is not an edit."""
        n = name.lower()
        return (
            n == "id"
            or n.endswith(
                ("_id", "id", "uuid", "_key", "_ref", "_ts", "_at", "_utc", "offset")
            )
            or any(h in n for h in ("blake3", "sha", "hash", "cursor", "token"))
        )

    _JSON_ID_KEY = (
        "key = 'id' OR key LIKE '%id' OR key LIKE '%uuid' OR key LIKE '%_key' "
        "OR key LIKE '%_ref' OR key LIKE '%_ts' OR key LIKE '%_at' OR key LIKE '%time%' "
        "OR key LIKE '%hash%' OR key LIKE '%blake3%' OR key LIKE '%sha%'"
    )

    def _tweak_sql(
        self, table: str, cols: list[tuple[str, str, bool]], *, payload_only: bool
    ) -> str | None:
        """Append a marker to every content text value, and to every
        top-level content string of every JSON payload, leaving keys,
        identity and non-text values alone. By value, not by declared
        type: `payload` is declared TEXT and holds a JSONB blob.
        `payload_only` leaves the plain text columns alone too: always
        for a table that carries a payload, since its other columns are
        promoted copies of payload fields and a column edited on its own
        is a state ingest never writes; and as a fallback for a table
        whose CHECK constraints refuse the general form."""
        payload_only = payload_only or any(name == "payload" for name, _, _ in cols)
        sets = [
            f"{name} = {self._tweak_expr(table, name, payload_only=payload_only)}"
            for name, _typ, pk in cols
            if not pk and not self._identity_like(name)
        ]
        if not sets:
            return None
        return f"UPDATE {table} SET {', '.join(sets)};"

    def _tweak_expr(self, table: str, name: str, *, payload_only: bool) -> str:
        """The marked value of column `name`, as an expression."""

        # A payload is a JSONB blob or JSON text, depending on the
        # provider; rewritten in kind, the marker lands on the
        # top-level content strings and nowhere else.
        def rewrite(agg: str) -> str:
            return (
                f"(SELECT {agg}(key, CASE WHEN type = 'text' THEN "
                f"(CASE WHEN {self._JSON_ID_KEY} THEN value ELSE value || '~' END) "
                f"ELSE json(value) END) FROM json_each({table}.{name}))"
            )

        text_case = (
            "" if payload_only else f"WHEN typeof({name}) = 'text' THEN {name} || '~' "
        )
        return (
            f"CASE "
            f"WHEN typeof({name}) = 'blob' AND json_valid({name}, 8) "
            f"AND json_type({name}) = 'object' THEN {rewrite('jsonb_group_object')} "
            f"WHEN typeof({name}) = 'text' AND json_valid({name}) "
            f"AND json_type({name}) = 'object' THEN {rewrite('json_group_object')} "
            f"{text_case}"
            f"ELSE {name} END"
        )

    def _insert_sql(
        self,
        db: Path,
        table: str,
        cols: list[tuple[str, str, bool]],
        row_id: str,
    ) -> tuple[str, str] | None:
        """Copy row `row_id` under a fresh primary key, content marked
        and its own identity made fresh in kind (see [`_fresh_payload`]),
        so the copy is a new entity in the same place: a message that
        arrived in a thread, a page in a workspace. Returns the SQL and
        the new key."""
        pk_col, pk_type = next((name, typ) for name, typ, pk in cols if pk)
        if "INT" in pk_type:
            new_key = str(
                int(self._rows(db, f"SELECT MAX({pk_col}) FROM {table};")[0]) + 1
            )
            new_literal = new_key
        else:
            new_key = f"{row_id}~new"
            new_literal = f"'{new_key}'"
        payload_only = any(name == "payload" for name, _, _ in cols)
        exprs = []
        for name, _typ, pk in cols:
            if pk:
                exprs.append(new_literal)
            elif name == "payload":
                exprs.append(
                    self._fresh_payload_sql(db, table, pk_col, row_id, new_key)
                )
            elif self._own_time_key(name):
                exprs.append(self._bumped_ts(name))
            elif self._identity_like(name):
                exprs.append(name)
            else:
                exprs.append(self._tweak_expr(table, name, payload_only=payload_only))
        names = ", ".join(name for name, _, _ in cols)
        sql = (
            f"INSERT INTO {table} ({names}) SELECT {', '.join(exprs)} FROM {table} "
            f"WHERE CAST({pk_col} AS TEXT) = '{row_id}';"
        )
        return sql, new_key

    @staticmethod
    def _bumped_ts(value: str) -> str:
        """`value` one second later, in the shape it came: Slack's
        `"1700000000.000100"` text, or a plain number."""
        return (
            f"CASE WHEN typeof({value}) = 'text' AND {value} GLOB '[0-9]*.[0-9]*' "
            f"AND {value} NOT GLOB '*[^0-9.]*' "
            f"THEN printf('%.6f', CAST({value} AS REAL) + 1) "
            f"WHEN typeof({value}) IN ('integer', 'real') THEN {value} + 1 "
            f"ELSE {value} END"
        )

    def _fresh_payload_sql(
        self, db: Path, table: str, pk_col: str, row_id: str, new_key: str
    ) -> str:
        """The copy's payload as a literal, in the store's own encoding."""
        raw = self._rows(
            db,
            f"SELECT typeof(payload) || '|' || json(payload) FROM {table} "
            f"WHERE CAST({pk_col} AS TEXT) = '{row_id}';",
        )[0]
        kind, text = raw.split("|", 1)
        try:
            doc = json.loads(text)
        except ValueError:
            return "payload"
        if not isinstance(doc, dict):
            return "payload"
        fresh = json.dumps(self._fresh_payload(doc, row_id, new_key)).replace("'", "''")
        return f"jsonb('{fresh}')" if kind == "blob" else f"'{fresh}'"

    # A payload key that is the row's own identity rather than a
    # reference to another row's: its `id`, and the send time several
    # providers mint a message's id from (Slack's `ts`, Signal's
    # `date_sent`, an SMS `date`). A reference — `channel_id`,
    # `page_id` — stays, so the copy lands where the original did.
    @staticmethod
    def _own_time_key(key: str) -> bool:
        k = key.lower()
        return k == "ts" or k.endswith(("_ts", "_ms")) or "date" in k or "time" in k

    def _fresh_payload(self, doc: dict, row_id: str, new_key: str) -> object:
        """The copy's payload: content marked as an edit would, and every
        key that is its own identity made fresh — the top-level `id`,
        any value equal to the old key, its send time bumped, and each
        nested `id` / `uuid` (a message inside a conversation) — since a
        row whose payload says another row's id is a state ingest never
        writes, and a uuid minted from it would collide with the
        original's."""

        def fresh_id(v):
            if isinstance(v, bool):
                return v
            if isinstance(v, int):
                return v + 1_000_000
            if isinstance(v, str):
                return f"{v}~new"
            return v

        def bump(v):
            if isinstance(v, bool):
                return v
            if isinstance(v, (int, float)):
                return v + 1
            if isinstance(v, str) and re.fullmatch(r"[0-9]+\.[0-9]+", v):
                return f"{float(v) + 1:.6f}"
            return v

        def walk(node, top: bool):
            if isinstance(node, dict):
                out = {}
                for k, v in node.items():
                    if k in ("id", "uuid"):
                        out[k] = fresh_id(v)
                    elif top and isinstance(v, str) and v == row_id:
                        out[k] = new_key
                    elif top and self._own_time_key(k):
                        out[k] = bump(v)
                    elif isinstance(v, str) and not self._identity_like(k):
                        out[k] = v + "~"
                    else:
                        out[k] = walk(v, False)
                return out
            if isinstance(node, list):
                return [walk(v, False) for v in node]
            return node

        return walk(doc, True)

    def _swap_blobs_sql(
        self, db: Path, table: str, cols: list[tuple[str, str, bool]]
    ) -> str | None:
        """Exchange two rows' `blake3` — the bytes behind two attachments
        swapped, which is the only shape a changed blob can take: bytes
        are content-addressed, so different bytes are a different digest
        on the edge row."""
        if not any(name == "blake3" for name, _, _ in cols):
            return None
        pks = [name for name, _, pk in cols if pk]
        if len(pks) != 1:
            return None
        rows = self._rows(
            db,
            f"SELECT CAST({pks[0]} AS TEXT), blake3 FROM {table} "
            "WHERE blake3 IS NOT NULL GROUP BY blake3 ORDER BY 1 LIMIT 2;",
        )
        if len(rows) < 2:
            return None
        (a, ha), (b, hb) = (r.split("|", 1) for r in rows)
        # Through a placeholder, for a table where the digest is unique.
        hold = "0" * 64
        where = f"WHERE CAST({pks[0]} AS TEXT) ="
        return (
            f"UPDATE {table} SET blake3 = '{hold}' {where} '{a}'; "
            f"UPDATE {table} SET blake3 = '{ha}' {where} '{b}'; "
            f"UPDATE {table} SET blake3 = '{hb}' {where} '{a}';"
        )

    # ── the render store, logically ─────────────────────────────────

    _DUMPED_TABLES = (
        "markdowns",
        "grid_rows",
        "edges",
        "problems",
        "render_cursor",
    )

    def _render_columns(self, db: Path) -> dict[str, list[str]]:
        """The columns of every dumped table. Read once: the render store's
        schema is `datalib_schema`'s, the same in every source's store,
        and this test dumps stores thousands of times."""
        if not hasattr(self, "_render_columns_cache"):
            sql = " ".join(
                f"SELECT '#{t}'; SELECT name FROM pragma_table_info('{t}');"
                for t in self._DUMPED_TABLES
            )
            self._render_columns_cache = {
                t: [line.split("|")[0] for line in rows]
                for t, rows in self._split_by_sentinel(self._rows(db, sql)).items()
            }
        return self._render_columns_cache

    @staticmethod
    def _split_by_sentinel(lines: list[str]) -> dict[str, list[str]]:
        """Rows of several SELECTs run in one invocation, split at the
        `#name` rows a `SELECT '#name';` between them emits. Safe because
        every dumped table leads with a key column, which never starts
        with `#`."""
        out: dict[str, list[str]] = {}
        current: list[str] | None = None
        for line in lines:
            if line.startswith("#"):
                current = out.setdefault(line[1:], [])
            elif current is not None:
                current.append(line)
        return out

    def _logical_dump(self, source_dir: Path) -> dict[str, list[str]]:
        """Every row the provider wrote to the render store, minus the
        volatile stamps, plus the `.md` tree as (path, digest) — the
        content two renders of one raw store must agree on. Datalib's own
        storage report is left out: it carries a byte-count history, so
        it is a function of the run, not of the raw store. One doltlite
        invocation for all the tables: spawning the shell is what this
        test's runtime is made of."""
        db = source_dir / "render_markdown" / "indexed_markdown.doltlite_db"
        out: dict[str, list[str]] = {}
        if db.is_file():
            selects = []
            for t, cols in self._render_columns(db).items():
                keep = [c for c in cols if c not in _VOLATILE.get(t, ())]
                if not keep:
                    continue
                where = " WHERE provider <> 'datalib'" if "provider" in cols else ""
                selects.append(
                    f"SELECT '#{t}'; SELECT {', '.join(keep)} FROM {t}{where} "
                    f"ORDER BY {', '.join(keep)};"
                )
            out = self._split_by_sentinel(self._rows(db, " ".join(selects)))
        files = []
        root = source_dir / "render_markdown"
        for p in sorted(root.rglob("*")):
            if p.is_file() and p.suffix == ".md" and "_datalib" not in p.parts:
                digest = hashlib.sha256(p.read_bytes()).hexdigest()[:16]
                files.append(f"{p.relative_to(root)} {digest}")
        out["files"] = files
        return out

    def _diff(self, want: dict[str, list[str]], got: dict[str, list[str]]) -> str:
        lines = []
        for t in sorted(set(want) | set(got)):
            a, b = set(want.get(t, [])), set(got.get(t, []))
            for row in sorted(a - b)[:5]:
                lines.append(f"  {t}: cold has, incremental lacks: {row[:160]}")
            for row in sorted(b - a)[:5]:
                lines.append(f"  {t}: incremental has, cold lacks: {row[:160]}")
        return "\n".join(lines)

    # ── the sweep ───────────────────────────────────────────────────

    def _sources(self) -> list[str]:
        """Groups with a render step and a doltlite raw store."""
        rendered = {
            s["group"]
            for s in self.config["steps"]
            if s["function"] == "render_markdown"
        }
        return sorted(
            g
            for g in rendered
            if (self.workspace / g / "ingest" / "entities.doltlite_db").is_file()
        )

    def _this_shards_sources(self, tables: dict[str, list]) -> list[str]:
        """The sources this bazel shard sweeps. Every shard runs the
        pipeline and lists every source's tables (cheap), then the
        sources are dealt out largest-first to the emptiest shard, so
        the shards finish together; the deal is deterministic, so the
        shards partition the sources."""
        index = int(os.environ.get("TEST_SHARD_INDEX", "0"))
        total = int(os.environ.get("TEST_TOTAL_SHARDS", "1"))
        load = [0] * total
        mine = []
        for source in sorted(tables, key=lambda s: (-len(tables[s]), s)):
            shard = load.index(min(load))
            load[shard] += len(tables[source])
            if shard == index:
                mine.append(source)
        return sorted(mine)

    def _scratch(
        self, tag: str, source: str, ingest_from: Path, *, with_render_store: bool
    ) -> Path:
        """A data root holding one source: its ingest tree copied from
        `ingest_from`, and its render store from the pipeline's workspace
        or none at all."""
        root = self.tmp / "contract" / tag
        if root.exists():
            shutil.rmtree(root)
        (root / source).mkdir(parents=True)
        shutil.copytree(ingest_from, root / source / "ingest")
        if with_render_store:
            shutil.copytree(
                self.workspace / source / "render_markdown",
                root / source / "render_markdown",
            )
        return root

    class Readers(NamedTuple):
        """What the render store declares about one raw row: the buckets
        that read it (`None` when the provider has declared nothing for
        the table — its scan is on its own then), how many documents
        those buckets hold, and how many documents the last render
        wrote, off its commit message — the only trace, since an
        unchanged rewrite leaves no diff."""

        buckets: list[str] | None
        documents: int
        rendered: int | None

    def _readers_of(self, source_dir: Path, table: str, row_id: str) -> Readers:
        """One spawn for all three: this runs twice per one-row mutation."""
        db = source_dir / "render_markdown" / "indexed_markdown.doltlite_db"
        buckets_sql = (
            "SELECT DISTINCT bucket_key FROM render_inputs "
            f"WHERE input_table = '{table}' AND input_id IN ('{row_id}', '*')"
        )
        got = self._split_by_sentinel(
            self._rows(
                db,
                f"SELECT '#declared'; SELECT COUNT(*) FROM render_inputs "
                f"WHERE input_table = '{table}'; "
                f"SELECT '#buckets'; {buckets_sql}; "
                f"SELECT '#documents'; SELECT COUNT(*) FROM markdowns "
                f"WHERE bucket_key IN ({buckets_sql}); "
                "SELECT '#log'; SELECT message FROM dolt_log() ORDER BY date DESC LIMIT 1;",
            )
        )
        log = got["log"]
        m = re.match(r"render \S+: (\d+) document\(s\)", log[0]) if log else None
        return self.Readers(
            buckets=None if got["declared"][0] == "0" else got["buckets"],
            documents=int(got["documents"][0]),
            rendered=int(m.group(1)) if m else None,
        )

    def _check(
        self,
        source: str,
        table: str,
        kind: str,
        sqls: list[str],
        failures: list[str],
        skipped: list[str],
        one_row: str | None = None,
        no_effect: list[str] | None = None,
    ) -> bool:
        """Apply the first of `sqls` the store accepts and that changes a
        row, then compare incremental against cold. `False` when none
        applied — a mutation the store refuses proves nothing. With
        `one_row` (the row's primary key), the mutation touched that row
        alone, and the incremental run must also have rendered exactly
        the documents the store declares as reading it: the contract's
        clause 2 in its narrow form, which a whole-table mutation cannot
        tell from "rendered everything"."""
        # Incremental: the copy carries the render store and its cursor.
        inc = self._scratch(
            f"{source}-{table}-{kind.replace(' ', '-')}-inc",
            source,
            self.workspace / source / "ingest",
            with_render_store=True,
        )
        db = inc / source / "ingest" / "entities.doltlite_db"
        applied = False
        self.last_refusal: list[str] = []
        for sql in sqls:
            # A refused statement fails the whole invocation, so the
            # count only comes back when the mutation went in.
            changed = self._doltlite(
                db, f"{sql} SELECT COUNT(*) FROM dolt_status;", must_succeed=False
            )
            if changed is not None and changed[-1] != "0":
                applied = True
                break
        if not applied:
            why = f" ({self.last_refusal[0][:120]})" if self.last_refusal else ""
            skipped.append(f"{source}: {kind} {table}{why}")
            shutil.rmtree(inc)
            return False
        self._rows(db, f"SELECT dolt_commit('-Am', 'contract: {kind} {table}');")
        # The buckets declared for the row and the documents under them
        # before the run; the edit may move a message into another
        # period, and an inserted row has readers only afterwards, so
        # the count after the run — under whichever buckets declare the
        # row then — is the other bound.
        readers = before = None
        if one_row is not None:
            declared = self._readers_of(inc / source, table, one_row)
            if declared.buckets is not None:
                readers, before = declared.buckets, declared.documents
        # What the store held before this run, for the insert: a copied
        # row that changes nothing in the output — an attachment edge no
        # payload points at, a reaction with no emoji row — was read by
        # nobody, and nobody has to declare it.
        untouched = (
            self._logical_dump(inc / source) if kind == "insert one row" else None
        )
        inc_err = self._render_step(inc, source)
        # Cold: the same mutated raw store, no render store.
        cold = self._scratch(
            f"{source}-{table}-{kind.replace(' ', '-')}-cold",
            source,
            inc / source / "ingest",
            with_render_store=False,
        )
        cold_err = self._render_step(cold, source)
        if inc_err or cold_err:
            if inc_err != cold_err:
                failures.append(
                    f"{source}: {kind} {table}\n  incremental: {inc_err or 'ok'}\n  cold: {cold_err or 'ok'}"
                )
            else:
                skipped.append(f"{source}: {kind} {table} (both refuse: {inc_err})")
        else:
            cold_dump = self._logical_dump(cold / source)
            d = self._diff(cold_dump, self._logical_dump(inc / source))
            if d:
                failures.append(f"{source}: {kind} {table}\n{d}")
            elif untouched is not None and not self._diff(untouched, cold_dump):
                if no_effect is not None:
                    no_effect.append(f"{source}: {table}")
            elif readers is not None and before is not None and one_row is not None:
                declared = self._readers_of(inc / source, table, one_row)
                readers_after = declared.buckets or []
                after, rendered = declared.documents, declared.rendered
                lo, hi = min(before, after), max(before, after)
                if rendered is None or not lo <= rendered <= hi:
                    failures.append(
                        f"{source}: {kind} {table}\n  one row changed; the store declared "
                        f"{len(readers)} bucket(s) reading it, holding {before} document(s), "
                        f"before the run and {len(readers_after)} holding {after} after; "
                        f"the run rendered {rendered}"
                    )
        # `RENDER_CONTRACT_KEEP` leaves the scratch roots for inspection.
        if not os.environ.get("RENDER_CONTRACT_KEEP"):
            shutil.rmtree(inc)
            shutil.rmtree(cold)
        return True

    def test_incremental_render_equals_cold_render_under_every_table_mutation(
        self,
    ) -> None:
        failures: list[str] = []
        skipped: list[str] = []
        # Inserted copies that changed no output: a row nothing reads on
        # its own — an attachment edge no payload points at, an account
        # row — proves only that rendering it is harmless. Listed so a
        # provider whose messages land here is visibly unchecked.
        no_effect: list[str] = []
        checked = 0
        # `RENDER_CONTRACT_ONLY=<source>` narrows a debugging run.
        only = os.environ.get("RENDER_CONTRACT_ONLY")
        tables = {
            source: self._entity_tables(
                self.workspace / source / "ingest" / "entities.doltlite_db"
            )
            for source in self._sources()
            if not only or source == only
        }
        sources = self._this_shards_sources(tables)
        for source in sources:
            # Written as it goes, so a shard killed at bazel's ceiling
            # still says how far it got.
            sys.stderr.write(
                f"[render contract] {source}: {len(tables[source])} table(s)\n"
            )
            db = self.workspace / source / "ingest" / "entities.doltlite_db"
            for table, cols in tables[source]:
                checked += self._check(
                    source,
                    table,
                    "delete",
                    [f"DELETE FROM {table};"],
                    failures,
                    skipped,
                )
                tweaks = [
                    t
                    for t in (
                        self._tweak_sql(table, cols, payload_only=False),
                        self._tweak_sql(table, cols, payload_only=True),
                    )
                    if t
                ]
                if tweaks:
                    checked += self._check(
                        source, table, "tweak", tweaks, failures, skipped
                    )
                # One row — the first by primary key — edited, deleted
                # and copied in under a fresh key, for the narrow half
                # of the contract. A composite key cannot be named in
                # one column, so those tables get only the whole-table
                # forms.
                pks = [name for name, _, pk in cols if pk]
                if len(pks) == 1:
                    row_id = self._rows(
                        db, f"SELECT CAST(MIN({pks[0]}) AS TEXT) FROM {table};"
                    )[0]
                    where = f"WHERE CAST({pks[0]} AS TEXT) = '{row_id}';"
                    if tweaks:
                        checked += self._check(
                            source,
                            table,
                            "tweak one row",
                            [f"{t[:-1]} {where}" for t in tweaks],
                            failures,
                            skipped,
                            one_row=row_id,
                        )
                    checked += self._check(
                        source,
                        table,
                        "delete one row",
                        [f"DELETE FROM {table} {where}"],
                        failures,
                        skipped,
                        one_row=row_id,
                    )
                    if inserted := self._insert_sql(db, table, cols, row_id):
                        sql, new_key = inserted
                        checked += self._check(
                            source,
                            table,
                            "insert one row",
                            [sql],
                            failures,
                            skipped,
                            one_row=new_key,
                            no_effect=no_effect,
                        )
                if swap := self._swap_blobs_sql(db, table, cols):
                    checked += self._check(
                        source, table, "swap blobs", [swap], failures, skipped
                    )
        sys.stderr.write(
            f"[render contract] {checked} mutation(s) checked, {len(skipped)} skipped "
            f"over {', '.join(sources)}\n"
        )
        for line in skipped:
            sys.stderr.write(f"[render contract]   skipped {line}\n")
        for line in no_effect:
            sys.stderr.write(
                f"[render contract]   an inserted copy changed no output: {line}\n"
            )
        if sources:
            self.assertGreater(checked, 0, "no source had a table to mutate")

        # A gap under the whole-table edit covers its one-row form: the
        # same rows went unread either way.
        def gap_key(failure: str) -> str:
            return failure.split("\n", 1)[0].replace("tweak one row", "tweak")

        failed = {gap_key(f) for f in failures}
        new = [f for f in failures if gap_key(f) not in KNOWN_GAPS]
        # Only the gaps of this shard's sources can be seen to have closed.
        fixed = sorted(
            k
            for k in KNOWN_GAPS
            if k not in failed and only is None and k.split(":", 1)[0] in sources
        )
        self.assertEqual(
            new,
            [],
            "incremental ≠ cold after a mutation the provider's diff scan must name:\n"
            + "\n".join(new),
        )
        self.assertEqual(
            fixed,
            [],
            "these KNOWN_GAPS entries no longer fail — remove them so the list keeps shrinking",
        )


if __name__ == "__main__":
    # Positional args are the pipeline's, not test names.
    unittest.main(argv=[sys.argv[0]])
