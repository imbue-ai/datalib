# Folding doltlite history: keep each run, drop its checkpoints

**Status: proposal (2026-10-20); nothing here is built.** Follows
imbue-ai/datalib#1259 (raw stores keep 10–17× their data in checkpoint
history) and #1260 (the growing seal interval). Every number marked
*synthetic* was measured on made-up data with
`scripts/doltlite_history_fold.py` on doltlite 0.50.16
([results](../../../hack/doltlite_history_fold/README.md)); the
real-root numbers are the sizes and counts reported in #1259. Section 9
holds the questions only Thad can answer. PRs 1–3 (§7) are needed
whatever the answers; 4 and 5 wait on them.

## 1. The problem

A doltlite store keeps every page that any commit can still reach
([what a write costs](../doltlite.md#what-a-write-costs)). A step seals
a checkpoint every 15 s or so, so that its readers can start early and a
killed run keeps what it had. Each checkpoint becomes a commit, and each
commit keeps every page that its writes touched. In a table whose keys
are random relative to the order rows are written, that is about one
page per row written. The rows are kept once; the pages around them are
kept once per checkpoint.

On one real data root (#1259), all 24 doltlite stores held 42.3 GB.
`dolt_gc()` alone took them to 31.9 GB. Folding each store's history
into one commit and then running gc took them to 9.0 GB. The email raw
store went from 12.8 GB to 0.74 GB that way, and the grid index from
10.2 GB to 0.98 GB. A synthetic Slack render store went from 7.5 GB to
1.5 GB, every row intact.

#1260 makes a long run seal less often: about 70 times in six hours
instead of 1,400. That helps a single long run of one step. It does not
help a source that is rendered and indexed in many short passes while
its download streams, because each pass is a new process and starts
its interval at 15 s again. The grid index seals once per pass, so its
3,115 commits are probably mostly such passes (§8 has the count that
would say).

**Folding** here means rewriting a stretch of history as fewer commits
that end on the same trees, then running gc so the pages only the old
commits reached are freed. The rows at each kept commit are exactly
what they were; the checkpoints between them are gone.

## 2. What was measured

### What each strategy saves (synthetic)

A store shaped like the JMAP email raw store: emails keyed in the order
the download writes them, threads and a secondary index keyed at
random, a bookkeeping sidecar updated at random. One backfill run sealed
80 times, then 90 daily runs sealed twice each. 136,000 emails and
92,500 threads at the end, 262 commits.

| strategy | commits | size after gc | gc time | most the directory held during gc |
|---|---|---|---|---|
| as stored | 262 | 1,036 MB | — | — |
| gc only | 262 | 988 MB (−5%) | 12.1 s | 2,024 MB |
| fold to run ends | 93 | 547 MB (−47%) | 7.3 s | 1,583 MB |
| keep the last 30 run ends | 33 | 258 MB (−75%) | 1.8 s | 1,294 MB |
| keep the last 7 run ends | 10 | 145 MB (−86%) | 0.8 s | 1,181 MB |
| fold to one commit | 3 | 110 MB (−89%) | 0.6 s | 1,146 MB |

What this says:

- **Folding to run ends gets about half.** Nearly all of it is the
  backfill's 80 checkpoints. The other half is the daily runs: each one
  touched ~1,100 rows in random-keyed trees and keeps ~4.8 MB of pages
  for it. History kept per run costs roughly *rows changed in
  random-keyed trees × one page (~4 KB)*, whatever the run's checkpoint
  count.
- **A short retention window gets nearly all of it.** Keeping a week of
  runs cost 35 MB over folding everything.
- **gc needs the compacted size in free disk, not the store's size.**
  The directory peaked at the old file plus the new one, then the old
  one went. Folding first shrinks what gc must write: 110 MB of
  headroom instead of 988 MB. (Upstream says "about the store's size",
  which is the gc-only case.)
- **Folding is cheap; gc is the cost.** Rebuilding 262 commits as 93
  took 1.3 s through the shell. The real 12.8 GB email store took about
  12 minutes to gc alone (#1259).

### What doltlite does (shell probes on 0.50.16, synthetic stores)

- **A rebuilt commit has the same tree hash as the one it replaces.**
  `dolt_hashof_db('<commit>')` is the hash of the commit's tree, and it
  was equal at every boundary of every fold above.
  A commit's own hash changes, because it names its parent.
- **`dolt_reset('--soft', x)` works when `x` is not an ancestor**, and
  `dolt_commit` takes `'--date'` in `YYYY-MM-DDTHH:MM:SS` form (not
  the `YYYY-MM-DD HH:MM:SS` that `dolt_log()` prints). Together they
  build a commit with any tree on any parent, so a chain can be rebuilt
  on a side branch without touching `main` or `datalib_writer`.
- **A branch cannot be force-moved while a connection is on it**
  (`cannot force-update the current branch`): the fold moves
  `datalib_writer` and `main` from its own branch.
- **gc writes a new file and renames it over the old one** (the inode
  changes). A reader that was already open, whether detached at a commit
  the fold removed or holding a read transaction on `main`, kept
  reading its rows after the fold and the gc. A held reader's next
  transaction moved to the new file and saw a later write; it let go of
  the old file then. So the old file's space comes back only when its
  last reader moves on.
- **A new open of a folded-away commit fails**: `branch or revision
  "<hash>" not found`.
- **Killing gc part-way left the store as it was** (killed at 5 s and
  at 9 s of a ~12 s gc; same head, same rows, no stray file; by hand, not
  in the script). A kill in the moment the new file is renamed into
  place was not caught.

These are shell probes. The plan's PR 3 turns each one that the code
leans on into a test through sqlx.

## 3. What holds a commit hash, and what it does when the hash is gone

Every fold changes the hash of every commit after its first folded one,
the newest included. So the question is never "will a consumer's cursor
change?" — it will — but "what does the consumer do then?".

| holder | which store's commit | where it is kept | when the hash is gone, today |
|---|---|---|---|
| `render_cursor.raw_commit` | the raw store | the render store | **Renders everything but sweeps nothing** (`render.rs`: `full_walk` needs `render_everything`, which only a version or params change sets). A document whose rows were deleted inside the folded range stays, until the next version bump. |
| whatsapp's render cursor | the raw store | the render store | **Fails, every run** (`whatsapp_render/src/render/render.rs`: `changed_chats` has no fallback, and a failed run never moves the cursor). |
| `params.diff = { from, to }` | the raw store | a person's `config.toml` | The diff group fails at `open_reader`; the person has to pick new commits. |
| `source_cursors.store_commit` | each render store | the grid index | Reads that source whole and removes what is gone. Correct, and as slow as a first index. |
| `terms_meta['grid_commit']` | the grid index | `search_terms.sqlite` | Rebuilds the terms file whole. Correct. |
| `time_series_sources.commit_hash` | each time-series store | the time-series index | **The applet answers "unreadable"** for that source until `time_series_index` runs again. |
| `draft/<key>` branches | the contacts store | branches in that store | A fold of `datalib_writer` leaves each draft with no common ancestor, and the drafts keep the old pages alive. |
| a step's version | its own store's head | `supervisor.sqlite` (`sinks`, `steps.reads`) | Only compared as a string: a fold changes it, so every consumer runs once. |
| the History card | any store | nothing stored | Reads `dolt_log` live; loses the checkpoints, keeps each run's message and `run=<id>`. |

Not hashes, and untouched by a fold: owed/coverage/`held_version`
bookkeeping, upstream delta tokens, `sync_runs`, the migration ladder's
rung, the CAS.

`docs/dev/doltlite.md` § "Disk space and `dolt_gc`" says a consumer
whose cursor named a folded commit "falls back to a full pass". The
first two rows show that is not true everywhere yet, and the first is a
correctness bug, not a cost: it also fires today when someone replaces
a raw store by hand.

## 4. The design

### 4.1 Fold to run ends

A **run** is one busy period of the loop: every commit a step seals
carries ` run=<id>` at the end of its message (`stamp_run`), and every
store we fold seals through `commit_run`. The fold keeps, for each
store:

- the last commit of every run;
- every commit something outside the store names and cannot re-find
  (diff groups, §4.5);
- the head;
- any commit with no stamp (sealed outside the loop, or by an older
  build), as a run of its own.

and folds the commits between two kept ones into the later of them, with
that commit's message and date. A run's checkpoints are what go. That
loses only states in the middle of a run, which no person asked for and
no consumer needs once the run is over: a consumer that settled read
the run's last commit.

Each kept commit is rebuilt with its tree, so a consumer can find it
again by that tree (§4.3).

### 4.2 The recipe, and what a crash leaves

The recipe #1259 sketches, and the one the render-store measurement
used, works on `datalib_writer` itself: `dolt_reset('--soft', base)`,
commit, move `main`. **That loses data if the process dies between the
reset and the commit** (read from `open`'s code, not run): The writer's branch then points at `base` with the whole
range uncommitted; the next writer's `open` discards uncommitted rows
(`dolt_reset --hard`) and publishes `datalib_writer` to `main` because
the two differ (`publish_on`). `main` lands on `base`. The pages are
still in the file until a gc, but nothing names them.

Instead, rebuild on a branch of its own, `datalib_fold`, and touch the
two real branches only at the end:

1. `dolt_branch('datalib_fold', <base>)`, where `base` is the newest
   kept commit that is not being rewritten (the schema commit, the
   first time).
2. For each kept commit `k`, oldest first, on `datalib_fold`:
   `dolt_reset('--hard', k)`, `dolt_reset('--soft', <previous rebuilt
   commit>)`, `dolt_commit('-Am', <k's message>, '--date', <k's
   date>)`. Check `dolt_hashof_db` of the new commit equals `k`'s.
3. `dolt_branch('-f', 'datalib_writer', 'datalib_fold')`.
4. `dolt_branch('-f', 'main', 'datalib_fold')`.
5. Back onto `datalib_writer`; `dolt_branch('-D', 'datalib_fold')`.
6. `dolt_gc()`, if there is room (§4.4).

| killed during | the store after the next writer's `open` |
|---|---|
| 1–2 | as before the fold, plus a stray `datalib_fold` |
| between 3 and 4 | `open` publishes `datalib_writer` to `main`: the fold completes |
| 5 | folded, plus a stray `datalib_fold` |
| 6 | folded; the old pages wait for the next gc |

`open` deletes a stray `datalib_fold`: it names pages nothing else
needs. The writer lock is held throughout, because the fold runs inside
the step that owns the store, through its own `open`.

The order of 3 and 4 matters: `main` must never point at a fold the
writer's branch does not, or `open` would publish the old chain back
over it. That is safe too (nothing is lost), but the fold is undone.

### 4.3 Cursors find their commit by its tree

A cursor keeps the commit it read **and that commit's tree hash**. To
diff from it:

1. the commit, if the store still has it;
2. otherwise, the newest commit on `main` with the same tree
   (`dolt_hashof_db` over `dolt_log()`; after a fold the log is short);
3. otherwise, a full pass that also removes what is gone.

Step 2 makes a fold at run ends cost nothing downstream. Step 3 is
what every consumer must do correctly anyway (PR 1). This needs no map
from old hashes to new, which would be state of its own to keep in
step with the store; the tree is already the content.

It applies to `render_cursor` (render and diff groups),
`source_cursors`, `terms_meta` and `time_series_sources`. Each is in a
derived store, so a new column is an additive change.

### 4.4 Who runs it, and when

**A new step verb, `--compact`, carried out by the loop when it is
idle.** Like `--migrate`, the loop knows nothing of doltlite: it
invokes the verb, and the step folds and collects its own store. The
loop runs it:

- **after a busy period ends** and nothing else is open, so every
  consumer has read its producer's last run and no pinned reader will
  reopen a commit the fold removes;
- **one step at a time, smallest store first**, so each gc frees disk
  for the next, larger one;
- **stopped by any new request**, by SIGINT, as a sync is: the step
  stops at its next consistent point (before step 3 or after step 5;
  a gc in progress is abandoned, which the probe says is safe).

Running inside the next sync's download instead would be simpler, but
the gc is minutes on a large store, and a person pressing Sync should
not wait for it.

**Disk headroom.** gc writes the compacted store beside the old one. The
step cannot know the compacted size beforehand; the old size is the
bound. So the step collects only when `free − old size` stays above the
config's `[disk_space]` pause line, and otherwise folds without
collecting and says so in its log. Smallest-first makes a tight disk
work through what it can. The space comes back as the store's last
reader moves to the new file.

**Which stores.**

| store | fold to run ends | why |
|---|---|---|
| raw stores (downloads, SQLite mirrors) | yes | where the checkpoint cost is (#1259) |
| render stores | yes | a cold render alone is 5× its rows |
| the grid index | yes | thousands of one-pass commits |
| time-series stores | not at first | keyed `(device_id, ts_ms)`, so history is cheap; measure, then decide |
| the contacts store | no | its history is a person's edits, and drafts are branched from it |
| `fsindex` | no | one branch per scan root, none published |

### 4.5 Commits a person named: diff groups

A diff group names two raw commits in `config.toml`, typed in or picked
from the History card. Run ends survive a fold and are found again by
tree, but a diff group that names a mid-run checkpoint would lose it.
Two parts:

- The diff group records each endpoint's tree in its own store the
  first time it runs, and resolves by tree after that.
- The loop hands `--compact` the commits any diff group reading the
  store names (it already parses the config), and the fold keeps them.
  Without this, a diff group whose endpoint was folded before it first
  ran is broken for good.

Whether a purge (§6) may ever drop one is question 3.

### 4.6 Versions

A step's version is its store's head commit. A fold moves the head's
hash and not its tree, so every consumer runs once, finds by tree that
nothing changed, and seals nothing; its own version stays, and the wave
stops there. That is cheap once §4.3 is in.

Reporting the **tree hash** as the version instead would make a fold
invisible to the loop, and the dag README already asks for a version
that is "a function of the output's content", which the tree hash is
more exactly than the commit. It costs one wake of every step on the
upgrade, and it would stop the fold from waking `time_series_index`,
which today is what heals the applet's record. Worth doing, after
PR 2; not needed for anything else.

## 5. The options weighed

1. **Fold within a run** — this plan, with "run" meaning the loop's busy
   period rather than one process, so the grid index's many passes fold
   too. About half the synthetic store; most of a backfill.
2. **Fold only what is older than every consumer's cursor.** Folding
   anything before a commit rewrites that commit's hash, so this still
   needs §4.3; and it never reclaims the newest run. Subsumed: run ends
   are exactly what settled consumers name.
3. **Seal less often during a full render** (a version bump). A render
   killed mid-walk starts the walk over anyway, so its checkpoints buy
   only early visibility. But a render's SQL transactions are its
   checkpoints (memory grows with the transaction, #1258), so sealing
   less means `dolt_commit`ting less while still committing SQL; the
   intermediate pages are then garbage only a gc frees. The fold gets
   the same result without changing the render. Not worth a PR of its
   own.

   The other direction costs more. On the synthetic Slack render store,
   a render that also committed SQL every ~32 MB of documents (a
   throwaway knob, not in the tree) left 10.6 GB and 46 commits, against
   7.5 GB and 22 for the 15 s checkpoints alone. gc alone took it to
   9.65 GB (−9%); folding then gc took it to 1.47 GB, the same as the
   other render. So whatever a render's batching does to its pages
   between seals, only folding gives it back. (The commit count doubled
   too, so this run does not say how much of the extra came from SQL
   commits and how much from the extra seals.)
4. **Retention across runs**, opt-in: §6.

## 6. Retention and "really delete what's been deleted"

Folding to run ends keeps every run. A raw store may hold what upstream
has since deleted, and its history is the backstop for a wrong prune
(AGENTS.md § "Keep a forward path for existing data"). So across runs
nothing should fold unless a person asks. Two shapes:

- **A retention window per source**: keep the end of every run from the
  last N days; fold everything older into one commit. Synthetic: a
  week of daily runs is 145 MB against 547 MB for every run end.
- **A purge**, on demand: fold everything to one commit and collect.

Either one loses, for the span it folds: getting back a row upstream
deleted (or our prune deleted wrongly); `dolt_diff` across it, including
the History card's "what changed in this run" for those runs; and any
diff group pair inside it.

A purge that promises the data is gone must also clear the **blob
CAS**, which is plain SQLite beside the raw store and today never
deletes anything ("Nothing resets the CAS", `etl/README.md`). It has to
delete every blob that no kept commit's rows reference. Two other
places can still hold the text after a purge and need checking: qmd's
own SQLite file (freed pages keep their bytes until a VACUUM) and the
attachment copies under `render_markdown/*/blobs/`.

Derived stores (render stores, the grid index) are rebuildable from the
raw stores, so their history is no one's backstop. Folding them to the
last run plus whatever their consumers name, every time, is safe;
whether to is question 1.

A purge is a wipe like a reset: a row in the mailbox's `wipes` table
(`supervisor/wipe.rs`) the loop carries out by holding the source's
steps and invoking `--compact` with "everything" on each, raw store
first.

## 7. Order of work

1. **Every consumer survives a missing commit, correctly.** Render:
   a cursor the store cannot resolve is a full walk *with* the sweep
   (it rendered everything at a pin it read, so it knows what should
   exist). whatsapp falls back like the others. The time-series applet
   reads the store's head when its recorded commit is gone, and says so.
   Fix the "falls back to a full pass" sentences in `doltlite.md` and
   `etl/README.md`. Test: in a fixture raw store, delete a row, fold
   history past the render's cursor, render, and assert the document is
   gone; watch it fail on today's code. No fold ships; this is a bug fix
   that a hand-replaced store already hits.
2. **Cursors carry their tree** (§4.3), through one resolver in
   `datalib_pin`. Facts in `doltlite_facts_test`: the tree hash
   survives a rebuild; `reset --soft` to a non-ancestor; the `--date`
   format; `cannot force-update the current branch`.
3. **The fold, in `datalib_etl`** (`doltlite_raw::fold_to_run_ends` or
   similar), with `open` deleting a stray `datalib_fold`.
   `doltlite_two_process_test` kills it at each row of §4.2's table, and
   runs a detached reader and a held transaction through sqlx across a
   peer's fold and gc (which retires the "open reader after a peer's
   gc" item under "Not yet measured" in `doltlite.md`).
4. **`--compact` and the loop running it when idle** (§4.4): the verb
   in `step_protocol.md`, the built-in steps for raw stores, render
   stores and the grid index, the diff-group keep list (§4.5), the disk
   rule, smallest first, stopped by a request. Probably a Manage-screen
   line saying what the last compaction freed.
5. **Retention and purge** (§6): config, Manage UI, CAS sweep. Waits on
   questions 1–4.
6. *(optional)* **Versions as tree hashes** (§4.6).

1 and 2 are independent of each other and can go in parallel.

## 8. Still to measure

- **On the real root, counts only: commits per run per store.** This
  says what folding to run ends would save there, where the synthetic
  store says "about half". For each store, from the shell:
  `select count(*), count(distinct case when instr(message, ' run=') > 0 then substr(message, instr(message, ' run=')) end) from dolt_log();`
  (commits, and runs; commits sealed outside the loop carry no stamp
  and are not counted as runs).
- **A render store's shape.** Fold to run ends against fold to one
  commit, for a synthetic Slack render streamed in many passes, the
  shape the 7.5 GB → 1.5 GB number came from.
- **gc at 10 GB after a fold**: time and peak RSS. Synthetic: 0.6 s
  for the store folded to one commit against 12.1 s for gc alone.
- **gc killed while it renames the new file into place.**
- **Time-series stores**: what their history costs, to settle whether
  they stay out.
- **Every probe in §2 through sqlx**, as PR 3 does.

## 9. Questions for Thad

1. **The default.** Recommended: fold every store to its run ends,
   automatically, when the loop is idle; fold derived stores (render,
   grid index) further, to their last run; never fold raw stores across
   runs unless a person asks. Or should even the run-end fold be
   opt-in?
2. **Retention.** Is a per-source "keep history for N days" wanted, or
   only a purge button? If a window, what default — off (keep every
   run), or something like 90 days?
3. **Diff groups.** May a purge or a retention window ever drop a commit
   a diff group names? Recommended: never silently; a purge that would
   lists the diff groups it breaks and asks.
4. **Where the purge lives, and what it promises.** A line in a
   source's Manage row menu ("Delete history and anything upstream
   deleted"), or a Storage card listing every store's size and what a
   compaction would free? And must a purge also clear the CAS, qmd's
   file and the attachment copies before it says the data is gone, or
   is "history folded" enough for a first version?
5. **When gc may run.** Idle after every busy period, or at most once a
   day, or only on AC power? A gc of a large store is minutes of disk
   I/O.
