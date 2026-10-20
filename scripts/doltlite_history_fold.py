#!/usr/bin/env python3
"""What folding a raw store's history saves, and what a reader sees while a
peer folds and gcs. Made-up data only. The dated results are in
`hack/doltlite_history_fold/README.md`; the plan they feed is
`docs/dev/plans/history_folding.md`.

    bazelisk build //third-party/doltlite:doltlite
    python3 scripts/doltlite_history_fold.py [scratch_dir]

The store is shaped like a JMAP email raw store: emails keyed in the order
the download writes them, threads and a secondary index keyed at random,
a bookkeeping sidecar updated at random. One backfill run seals 80 times
(what the growing checkpoint interval gives a backfill of several hours),
then 90 small daily runs seal twice each. Each strategy works on a copy:
fold the history onto some of its run ends on a side branch, move `main`,
`dolt_gc()`, and report the size, the times, and the most the directory
held during the gc.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time

DOLTLITE = os.path.abspath("bazel-bin/third-party/doltlite/doltlite")
BACKFILL_SEALS, RUNS, SEALS_PER_RUN = 80, 90, 2
BACKFILL_EMAILS, BACKFILL_THREADS = 100_000, 70_000
RUN_EMAILS, RUN_NEW_THREADS, RUN_THREAD_UPDATES, RUN_BOOKKEEPING_UPDATES = (
    400,
    250,
    150,
    300,
)


def sh(store: str, sql: str, readonly: bool = False) -> str:
    argv = [DOLTLITE, *(["-readonly"] if readonly else []), store]
    p = subprocess.run(argv, input=sql, capture_output=True, text=True, check=False)
    if p.returncode != 0 or "Error" in p.stderr:
        sys.exit(f"doltlite failed: {p.stderr}\n{sql[:400]}")
    return p.stdout.strip()


def numbers(n: int, first: int = 1) -> str:
    return f"(with recursive c(x) as (select {first} union all select x+1 from c where x<{first + n - 1}) select x from c)"


def seal_sql(
    next_email: int,
    emails: int,
    threads: int,
    thread_updates: int,
    bk_updates: int,
    msg: str,
) -> str:
    lo = next_email - emails
    return f"""
BEGIN;
insert into threads select lower(hex(randomblob(8))), hex(randomblob(70)) from {numbers(threads)};
update threads set subject = hex(randomblob(70)) where id in
  (select id from threads order by random() limit {thread_updates});
insert into emails select printf('%012d', x), lower(hex(randomblob(8))), hex(randomblob(250)) from {numbers(emails, lo + 1)};
insert into emails_bookkeeping select id, 'v1', '2026-01-01T00:00:00Z' from emails
  where id > printf('%012d', {lo}) and id <= printf('%012d', {next_email});
update emails_bookkeeping set held_version = hex(randomblob(4)) where id in
  (select id from emails_bookkeeping order by random() limit {bk_updates});
COMMIT;
select dolt_commit('-Am', '{msg}');
"""


def build(store: str) -> list[str]:
    """Builds the store and returns each run's last commit, oldest first."""
    sh(
        store,
        """
create table emails(id text primary key, thread_id text, body text);
create index emails_by_thread on emails(thread_id, id);
create table threads(id text primary key, subject text);
create table emails_bookkeeping(id text primary key, held_version text, seen_at text);
select dolt_commit('-Am', 'schema');
""",
    )
    next_email = 10**9  # descending, like a download walking receivedAt newest first
    run_ends = []
    for i in range(BACKFILL_SEALS):
        emails = BACKFILL_EMAILS // BACKFILL_SEALS
        sh(
            store,
            seal_sql(
                next_email,
                emails,
                BACKFILL_THREADS // BACKFILL_SEALS,
                0,
                0,
                f"run 0 seal {i}",
            ),
        )
        next_email -= emails
    run_ends.append(sh(store, "select dolt_hashof('HEAD');"))
    for r in range(1, RUNS + 1):
        for i in range(SEALS_PER_RUN):
            emails = RUN_EMAILS // SEALS_PER_RUN
            sh(
                store,
                seal_sql(
                    next_email,
                    emails,
                    RUN_NEW_THREADS // SEALS_PER_RUN,
                    RUN_THREAD_UPDATES // SEALS_PER_RUN,
                    RUN_BOOKKEEPING_UPDATES // SEALS_PER_RUN,
                    f"run {r} seal {i}",
                ),
            )
            next_email -= emails
        run_ends.append(sh(store, "select dolt_hashof('HEAD');"))
    return run_ends


def fold(store: str, boundaries: list[str]) -> None:
    """Rebuilds history as one commit per boundary on a side branch, each
    holding the boundary's own tree and date, then moves `main` to it."""
    log = [
        line.split("|")
        for line in sh(
            store, "select commit_hash, date, message from dolt_log();", True
        ).splitlines()
    ]
    by_hash = {h: (d, m) for h, d, m in log}
    schema = next(h for h, _, m in log if m == "schema")
    sh(store, f"select dolt_branch('datalib_fold', '{schema}');")
    prev = schema
    for b in boundaries:
        date, msg = by_hash[b]
        prev = sh(
            store,
            f"""
select dolt_checkout('datalib_fold');
select dolt_reset('--hard', '{b}');
select dolt_reset('--soft', '{prev}');
select dolt_commit('-Am', '{msg}', '--date', '{date.replace(" ", "T")}');
""",
        ).splitlines()[-1]
        same_tree = sh(
            store, f"select dolt_hashof_db('{prev}') = dolt_hashof_db('{b}');", True
        )
        assert same_tree == "1", f"the fold of {b} holds a different tree"
    sh(
        store,
        """
select dolt_checkout('datalib_fold');
select dolt_branch('-f', 'main', 'datalib_fold');
select dolt_checkout('main');
select dolt_branch('-D', 'datalib_fold');
""",
    )


def gc_watching_disk(store: str) -> tuple[float, int]:
    """Runs dolt_gc and returns its time and the most the store's directory held."""
    peak, stop = [0], [False]
    folder = os.path.dirname(store)

    def watch() -> None:
        while not stop[0]:
            try:
                peak[0] = max(
                    peak[0],
                    sum(
                        os.path.getsize(os.path.join(folder, f))
                        for f in os.listdir(folder)
                    ),
                )
            except FileNotFoundError:
                pass
            time.sleep(0.05)

    watcher = threading.Thread(target=watch)
    watcher.start()
    started = time.time()
    sh(store, "select dolt_gc();")
    stop[0] = True
    watcher.join()
    return time.time() - started, peak[0]


def measure(
    original: str, scratch: str, name: str, boundaries: list[str] | None
) -> dict[str, object]:
    folder = os.path.join(scratch, name)
    os.makedirs(folder)
    store = os.path.join(folder, os.path.basename(original))
    shutil.copyfile(original, store)
    before = os.path.getsize(store)
    started = time.time()
    if boundaries is not None:
        fold(store, boundaries)
    fold_s = time.time() - started
    gc_s, peak = gc_watching_disk(store)
    result: dict[str, object] = {
        "strategy": name,
        "commits": int(sh(store, "select count(*) from dolt_log();", True)),
        "rows": sh(
            store, "select count(*) from emails; select count(*) from threads;", True
        ).replace("\n", "/"),
        "before_mb": round(before / 1e6, 1),
        "after_mb": round(os.path.getsize(store) / 1e6, 1),
        "fold_s": round(fold_s, 1),
        "gc_s": round(gc_s, 1),
        "gc_peak_dir_mb": round(peak / 1e6, 1),
    }
    shutil.rmtree(folder)
    return result


class Shell:
    """A doltlite shell kept open, so a reader can hold its connection while a peer works."""

    MARK = "__done__"

    def __init__(self, argv: list[str]) -> None:
        self.p = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
        )

    def q(self, sql: str) -> str:
        assert self.p.stdin is not None and self.p.stdout is not None
        self.p.stdin.write(f"{sql}\nselect '{self.MARK}';\n")
        self.p.stdin.flush()
        out = []
        while True:
            line = self.p.stdout.readline()
            if not line:
                out.append("<exited>")
                break
            if line.strip() == self.MARK:
                break
            out.append(line.strip())
        return " | ".join(out)

    def close(self) -> None:
        assert self.p.stdin is not None
        self.p.stdin.close()
        self.p.wait()


def readers_across_a_fold(original: str, scratch: str, run_ends: list[str]) -> None:
    folder = os.path.join(scratch, "readers")
    os.makedirs(folder)
    store = os.path.join(folder, os.path.basename(original))
    shutil.copyfile(original, store)
    mid_run = sh(store, "select commit_hash from dolt_log() limit 1 offset 1;", True)
    rows = (
        "select count(*), sum(length(body)) from emails; select count(*) from threads;"
    )
    detached = Shell(
        [DOLTLITE, "-readonly", f"{store}/{mid_run}"]
    )  # the shell's spelling of <store>@<hash>
    held = Shell([DOLTLITE, "-readonly", store])
    print("detached reader at a mid-run seal, before:", detached.q(rows))
    print("held read transaction on main, before    :", held.q("BEGIN; " + rows))
    fold(store, run_ends)
    gc_watching_disk(store)
    print("detached reader, after the fold and gc   :", detached.q(rows))
    print("held transaction, still open             :", held.q(rows))
    sh(
        store,
        "insert into threads values ('zzzz', 'after'); select dolt_commit('-Am', 'after the gc');",
    )
    print(
        "held reader's next transaction           :",
        held.q("COMMIT; BEGIN; select count(*) from sqlite_master; " + rows),
    )
    fresh = Shell([DOLTLITE, "-readonly", f"{store}/{mid_run}"])
    print("a new open of the folded-away seal       :", fresh.q(rows))
    for s in (detached, held, fresh):
        s.close()
    shutil.rmtree(folder)


def main() -> None:
    scratch = (
        sys.argv[1] if len(sys.argv) > 1 else tempfile.mkdtemp(prefix="history_fold_")
    )
    os.makedirs(scratch, exist_ok=True)
    print("doltlite", sh(":memory:", "select dolt_version();"), "scratch", scratch)
    original = os.path.join(scratch, "entities.doltlite_db")
    run_ends = build(original)
    strategies: list[tuple[str, list[str] | None]] = [
        ("gc only", None),
        ("fold to run ends", run_ends),
        ("keep last 30 run ends", run_ends[-31:]),
        ("keep last 7 run ends", run_ends[-8:]),
        ("fold to one commit", run_ends[-1:]),
    ]
    for name, boundaries in strategies:
        print(json.dumps(measure(original, scratch, name, boundaries)))
    readers_across_a_fold(original, scratch, run_ends)
    shutil.rmtree(scratch)


if __name__ == "__main__":
    main()
