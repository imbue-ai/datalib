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
two render stores' logical content. Two mutations per table: delete
every row, and append a marker to every text column and every top-level
string in every JSON payload — the first is what a source losing an
entity looks like, the second what an edit looks like. A whole table at
a time rather than a row at a time, so this is tables×2 renders per
source rather than rows×2; a failure names the source, the table and the
mutation, which is the fix in one line.

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

_BAZEL_WORKSPACE_DIR = "_main"

# Bookkeeping the ingest side owns and render never reads. Mutating it
# proves nothing, and deleting `sync_runs` would only exercise the
# framework's own skip.
_SKIP_TABLES = ("sync_runs", "sync_scope_state", "sync_scope_config", "ingested_files")
_SKIP_SUFFIXES = ("_bookkeeping",)

# Mutations under which a provider is known not to keep the contract
# today, each with why. The list is exact: a fixed provider makes its
# entry fail the run until it is removed, so it can only shrink. A new
# entry needs a reason, which is the finding.
KNOWN_GAPS: dict[str, str] = {
    # ── a change that does not reach the render at all ──
    # (contract clause 2 for a diff-narrowed renderer; for a whole-store
    # one, a row its walk never reads — not traced yet)
    "beeper: tweak rooms": "not traced",
    "sms-backup-restore: delete sms_attachments": "not traced",
    "sms-backup-restore: tweak sms_attachments": "same",
    # ── a table the bucket query does not name ──
    # (contract clause 2)
    "tng_pdfs: delete pdf_scan_meta": "scan provenance shown on the page; not in the scan",
    "tng_pdfs: tweak pdf_scan_meta": "same",
}

# Columns of the render store whose value is a stamp of *when* rather
# than *what*: identical content renders them differently on every run.
_VOLATILE = {
    "render_problems": ("first_seen_at_utc", "last_seen_at_utc", "tz_offset"),
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
            argv, check=False, cwd=str(cls.cwd), capture_output=True, text=True
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
        result = subprocess.run(
            [str(self.cwd / self.doltlite_bin), str(db), sql],
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            if not must_succeed:
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
            argv += ["--params", json.dumps(step["params"])]
        result = subprocess.run(
            argv,
            check=False,
            cwd=str(self.cwd),
            env=env,
            capture_output=True,
            text=True,
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
        out = []
        for t in names:
            if t in _SKIP_TABLES or t.endswith(_SKIP_SUFFIXES):
                continue
            if self._rows(db, f"SELECT COUNT(*) FROM {t};")[0] == "0":
                continue
            cols = []
            for line in self._rows(
                db, f"SELECT name, type, pk FROM pragma_table_info('{t}');"
            ):
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
        sets = []
        for name, _typ, pk in cols:
            if pk or self._identity_like(name):
                continue

            # A payload is a JSONB blob or JSON text, depending on the
            # provider; rewritten in kind, the marker lands on the
            # top-level content strings and nowhere else.
            def rewrite(agg: str, col: str = name) -> str:
                return (
                    f"(SELECT {agg}(key, CASE WHEN type = 'text' THEN "
                    f"(CASE WHEN {self._JSON_ID_KEY} THEN value ELSE value || '~' END) "
                    f"ELSE json(value) END) FROM json_each({table}.{col}))"
                )

            text_case = (
                ""
                if payload_only
                else f"WHEN typeof({name}) = 'text' THEN {name} || '~' "
            )
            sets.append(
                f"{name} = CASE "
                f"WHEN typeof({name}) = 'blob' AND json_valid({name}, 8) "
                f"AND json_type({name}) = 'object' THEN {rewrite('jsonb_group_object')} "
                f"WHEN typeof({name}) = 'text' AND json_valid({name}) "
                f"AND json_type({name}) = 'object' THEN {rewrite('json_group_object')} "
                f"{text_case}"
                f"ELSE {name} END"
            )
        if not sets:
            return None
        return f"UPDATE {table} SET {', '.join(sets)};"

    # ── the render store, logically ─────────────────────────────────

    def _logical_dump(self, source_dir: Path) -> dict[str, list[str]]:
        """Every row the provider wrote to the render store, minus the
        volatile stamps, plus the `.md` tree as (path, digest) — the
        content two renders of one raw store must agree on. Datalib's own
        storage report is left out: it carries a byte-count history, so
        it is a function of the run, not of the raw store."""
        db = source_dir / "render_markdown" / "indexed_markdown.doltlite_db"
        out: dict[str, list[str]] = {}
        if db.is_file():
            for t in (
                "markdowns",
                "grid_rows",
                "edges",
                "render_problems",
                "render_cursor",
            ):
                cols = [
                    line.split("|")[0]
                    for line in self._rows(
                        db, f"SELECT name FROM pragma_table_info('{t}');"
                    )
                ]
                keep = [c for c in cols if c not in _VOLATILE.get(t, ())]
                if not keep:
                    continue
                where = " WHERE provider <> 'datalib'" if "provider" in cols else ""
                out[t] = self._rows(
                    db,
                    f"SELECT {', '.join(keep)} FROM {t}{where} ORDER BY {', '.join(keep)};",
                )
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

    def _rendered(self, source_dir: Path) -> int | None:
        """How many documents the last render of this store wrote, off
        its commit message — the only trace, since an unchanged rewrite
        leaves no diff."""
        db = source_dir / "render_markdown" / "indexed_markdown.doltlite_db"
        rows = self._rows(
            db, "SELECT message FROM dolt_log() ORDER BY date DESC LIMIT 1;"
        )
        m = re.match(r"render \S+: (\d+) document\(s\)", rows[0]) if rows else None
        return int(m.group(1)) if m else None

    def _expected_rerender(
        self, source_dir: Path, table: str, row_id: str
    ) -> int | None:
        """How many documents a change to one row should re-render, by
        what the store declares: every document under a bucket that
        recorded reading `(table, row_id)`. `None` when the provider has
        declared nothing for the table — its scan is on its own then."""
        db = source_dir / "render_markdown" / "indexed_markdown.doltlite_db"
        declared = self._rows(
            db, f"SELECT COUNT(*) FROM render_inputs WHERE input_table = '{table}';"
        )[0]
        if declared == "0":
            return None
        return int(
            self._rows(
                db,
                "SELECT COUNT(*) FROM markdowns WHERE bucket_key IN "
                "(SELECT bucket_key FROM render_inputs "
                f"WHERE input_table = '{table}' AND input_id = '{row_id}');",
            )[0]
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
        for sql in sqls:
            if self._doltlite(db, sql, must_succeed=False) is None:
                continue
            if self._rows(db, "SELECT COUNT(*) FROM dolt_status;")[0] != "0":
                applied = True
                break
        if not applied:
            skipped.append(f"{source}: {kind} {table}")
            shutil.rmtree(inc)
            return False
        self._rows(db, f"SELECT dolt_commit('-Am', 'contract: {kind} {table}');")
        expected = (
            self._expected_rerender(inc / source, table, one_row)
            if one_row is not None
            else None
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
            d = self._diff(
                self._logical_dump(cold / source), self._logical_dump(inc / source)
            )
            if d:
                failures.append(f"{source}: {kind} {table}\n{d}")
            elif expected is not None:
                rendered = self._rendered(inc / source)
                if rendered != expected:
                    failures.append(
                        f"{source}: {kind} {table}\n  one row changed; the store declares "
                        f"{expected} document(s) reading it, the run rendered {rendered}"
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
        checked = 0
        # `RENDER_CONTRACT_ONLY=<source>` narrows a debugging run.
        only = os.environ.get("RENDER_CONTRACT_ONLY")
        for source in self._sources():
            if only and source != only:
                continue
            db = self.workspace / source / "ingest" / "entities.doltlite_db"
            for table, cols in self._entity_tables(db):
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
                # The same edit on one row — the first by primary key —
                # for the narrow half of the contract. A composite key
                # cannot be named in one column, so those tables get
                # only the whole-table form.
                pks = [name for name, _, pk in cols if pk]
                if tweaks and len(pks) == 1:
                    row_id = self._rows(
                        db, f"SELECT CAST(MIN({pks[0]}) AS TEXT) FROM {table};"
                    )[0]
                    checked += self._check(
                        source,
                        table,
                        "tweak one row",
                        [
                            f"{t[:-1]} WHERE CAST({pks[0]} AS TEXT) = '{row_id}';"
                            for t in tweaks
                        ],
                        failures,
                        skipped,
                        one_row=row_id,
                    )
        sys.stderr.write(
            f"[render contract] {checked} mutation(s) checked, {len(skipped)} skipped\n"
        )
        for line in skipped:
            sys.stderr.write(f"[render contract]   skipped {line}\n")
        self.assertGreater(checked, 0, "no source had a table to mutate")

        # A gap under the whole-table edit covers its one-row form: the
        # same rows went unread either way.
        def gap_key(failure: str) -> str:
            return failure.split("\n", 1)[0].replace("tweak one row", "tweak")

        failed = {gap_key(f) for f in failures}
        new = [f for f in failures if gap_key(f) not in KNOWN_GAPS]
        fixed = sorted(k for k in KNOWN_GAPS if k not in failed and only is None)
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
