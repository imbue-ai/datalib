# Audit: does the tree keep the deletion record? (2026-09-11)

**This is measurement, not intent.** It checks the tree against the
first two conditions of
[`render_inputs.md`](render_inputs.md#the-principle-the-raw-stores-diff-is-the-deletion-record)
§"The principle":

1. **Never lose the range** — the commit a render cursor names is what
   the next `dolt_diff` starts from; lose it and there is no deletion
   information, only a full walk.
2. **Every raw commit is a consistent snapshot** — a perfect diff of a
   torn commit reports exactly the wrong thing.

Every claim below was read off the tree on 2026-09-11 at `254e469b`;
file pointers are to that state. Where a doc in the tree says
otherwise, the tree wins and the doc is named. Condition 3 (ingest owns
"upstream deleted it") is touched only where it fell out of checking
the other two.

## Summary

| # | finding | condition | reachable how | severity |
|---|---|---|---|---|
| 1.1 | A renderer-version bump `rm -rf`s `render_markdown/`, and the cursor lives inside it | 1 | every version bump | **loses the range every time**; also loses `grid_index`'s range, whose cold path deletes nothing, so re-keyed documents stay in the index |
| 1.2 | A render-param change ignores the range for one run; the cold start it triggers deletes nothing | 1 | every param change | orphans (the known gap in parse_and_render §5) |
| 1.3 | The cursor file is written by the provider *before* the driver's `dolt_commit`, and the render step runs no transaction | 1 | crash in the window | nothing is lost today, by luck of ordering; the shape (a cursor ahead of its commit) is what `grid_index` already avoids |
| 1.4 | The cursor is written with `std::fs::write`, no rename | 1 | crash mid-write | step fails loudly; recovery is a hand delete, which is a cold start, which deletes nothing |
| 1.5 | The migration recipe says to wipe the cursor on `--reset-and-redownload`; nothing in the tree does | 1 | — | prose stale, tree correct |
| 1.6 | datalib never replaces a raw store file; a cursor the raw store cannot resolve is logged at `info` | 1 | manual deletion | should be `warn` — it now means "someone replaced the file" |
| 2.1 | The Ctrl-C hook commits regardless of policy, so an interrupted wiping run commits the wipe | 2 | Ctrl-C during reset / `always_clear_before_ingest` / any mirror or scan ingest | **torn commit in history**; the streaming plan's "checkpointing is disabled for the whole run" is false for this path |
| 2.2 | A wiping run that dies uncommitted is rescue-committed by the next writer's `open` | 2 | crash, then any run | torn commit in history |
| 2.3 | 2.1 and 2.2 are unreachable through the runner, and reachable by hand | 2 | `datalib-step` render after either, before a successful ingest | **the delete-then-re-add the principle exists to prevent** |
| 2.4 | The slack applet opens slack's render store read-write, which runs `dolt_status` and rescue-commits | 2 | open the Slack card during a render | the #400 hazard on a store lint check 5 does not cover: the render's checkpoint commit fails and its rows are lost; a torn document reaches the index transiently |
| 2.5 | The five wiping ingests are protected by omission, not declaration: none calls `wrote()`, none takes `Policy::Never` | 2 | — | nothing says so in code; 2.1 is the proof it is not enough |
| 2.6 | The four streaming ingests delete only inside SQL transactions, and only real upstream deletions | 2 ✓ | — | honours the condition; recorded so the good shape is named |

## How it was checked

```sh
# every reader, writer and deleter of the render cursor
grep -rn 'render_cursor::\(write\|read\|read_for_params\|cursor_path\)' datalib/backend --include='*.rs'
grep -rn 'remove_dir_all\|remove_file' datalib/backend --include='*.rs'   # nothing removes a .doltlite_db outside tests

# who can seal a raw store mid-run, and who reports writes to the sealer
grep -rn 'fn streams_output' -A2 datalib/backend/etl/providers/*/src      # true: chatgpt claude email slack
grep -rn '\.wrote(' datalib/backend/etl/providers/*/src                    # same four, and nobody else
grep -rn 'Policy::Never' datalib/backend/etl/providers/*/src               # nobody

# what commits without asking the policy
sed -n 186,200p datalib/backend/etl/src/raw_store.rs      # RawStoreCheckpoint::checkpoint → commit_run
sed -n 615,660p datalib/backend/etl/src/doltlite_raw.rs   # rescue_dirty_working_tree

# who opens a store somebody else owns, and how
grep -rn 'doltlite_raw::open\b\|open_derived\|open_reader' datalib/backend/{applets,http,datalib_step}/src
```

## Condition 1: never lose the range

### 1.1 A renderer-version bump removes the directory the cursor lives in

[`render_cursor::cursor_path`](../../../datalib/backend/etl/src/render_cursor.rs)
puts the cursor at `<source>/render_markdown/_render_cursor.json`.
[`discard_tree`](../../../datalib/backend/datalib_step/src/render.rs)
does `remove_dir_all(rendered_root)` on that same directory when
`tree_is_from_an_older_renderer` says the stored versions are not a
subset of the declared ones. The cursor goes with it, so the next render
has no `from_ref`, cold-starts, and — because a cold start asks
`buckets_without_rows` about nothing — deletes nothing.

It is worse one stage down. The same `remove_dir_all` takes
`indexed_markdown.doltlite_db`, the render store, whose commit history
is what `grid_index`'s `source_cursors` names. The rebuilt store has a
fresh history; `changed_since` cannot resolve the old cursor; the index
falls back to reading the store whole
([`build_grid_index`](../../../datalib/backend/etl/render/src/grid_index.rs),
the `(None, Some(from))` arm) — and that path builds no `removed` list.
So a version bump that changed a uuid recipe, which is the common reason
to bump, leaves every old uuid in `grid_rows` beside every new one, in
the index and in qmd, until somebody rebuilds the index by hand.

The fix in `render_inputs.md` step 1 covers both: never discard the
store, re-render every bucket in place, and the cursor moves into the
store so there is no separate file to lose.

### 1.2 A param change ignores the range for one run

[`read_for_params`](../../../datalib/backend/etl/src/render_cursor.rs)
returns `None` when the stored params differ from the current ones,
which every ported provider then passes to `scan_buckets` as "no
cursor". The file is not deleted — it is overwritten at the end of the
run with the new params and the new HEAD — so the range is *ignored*
rather than destroyed. But the run it is ignored for is a cold start,
and a cold start deletes nothing, so a document the new params no
longer produce (a different `period`, a narrower `only_render_labels`)
stays on disk and in the index. This is the "known gap: nothing prunes
`render_markdown/`" in
[parse_and_render.md §5](../data_architecture_parse_and_render.md#the-same-problem-on-the-render-side),
found again from the other direction.

"Every bucket must be rendered again" and "forget where I was" are
different requests; only the first is wanted here.

### 1.3 The range advances before the store commits

Every ported provider writes the cursor at the end of its own `run`
(`render_cursor::write(&cursor_path, head, …)` after `render_all`
returns — e.g. [slack](../../../datalib/backend/etl/providers/slack_render/src/render/render.rs)
line 124). The driver commits the store afterwards: `store.commit(&msg)`
in [`datalib_step/src/render.rs`](../../../datalib/backend/datalib_step/src/render.rs)
runs after the processor loop, the retain sweep and the storage report.
And the render step never calls `begin_transaction` — every statement
in `apply_markdown` auto-commits at the SQL level, on a working set
doltlite keeps in the file.

So there is a window, from the provider's cursor write to the driver's
`dolt_commit`, in which the cursor already claims `HEAD` was consumed
and the store has not said so. Checked what sits in it: nothing that
would be lost. Every ported provider calls `remove_conversation` for
its vanished buckets *before* `render_all`, so the deletions precede
the write; each source has one render processor, so no second render
follows it; and a crash after the write leaves a working set that is
complete (every document applied whole, every removal done) and
persisted, which the next `open` rescue-commits. A crash *inside*
`render_all` — including mid-`apply_markdown`, which leaves a document
with its rows deleted and not yet re-inserted — is before the write,
so the cursor is unmoved, the next scan names that bucket again, and
the tear is repaired.

That is the right outcome reached by ordering rather than by
structure. The shape is a cursor that can run ahead of the commit it
describes, and it stays safe only as long as nothing render-relevant
moves between the two — the retain sweep already does, for the
whole-store renderers, though those have no cursor to advance.
`grid_index` got this right: `write_source_cursor` runs inside the
same transaction as the rows. The render side should match it, which
putting the cursor in the store does by construction.

### 1.4 The cursor write is not atomic

`render_cursor::write` is `std::fs::write`. A crash mid-write leaves
truncated JSON. `read` returns `Err` on that, and every provider's
`read_for_params(…)?` propagates it, so the step fails — loudly, which
is right. `rendered_tree_version` in the driver catches the same error
and reports no version so the runner content-hashes the tree, with a
`warn`. Recovery is deleting the file by hand, which is a cold start,
which deletes nothing. Subsumed by moving the cursor into the store.

### 1.5 Nothing wipes the cursor on a reset; the recipe says to

[`provider_migration_dolt_diff_and_cas_edge.md`](../provider_migration_dolt_diff_and_cas_edge.md)
§"Edge cases" item 2 recommends "wipe the cursor" on
`--reset-and-redownload`. No code does: the only places that touch the
cursor file are the reads and writes above, and nothing under
`control.rs`, `datalib_step/src/ingest.rs` or any provider's reset
branch names it. The tree is right and the recipe is stale. A reset is
a committed truncate followed by a committed refill — one commit,
because `checkpoint_policy` returns `Never` under the flag — and the
diff from the pre-reset cursor to that commit is exactly the upstream
delta. Keep it that way; fix the recipe.

### 1.6 The raw side keeps its range; the fallback that covers losing it is too quiet

No code outside tests removes a `.doltlite_db` (the `remove_file` /
`remove_dir_all` sites are the node runtime cache, the applet frontend
dir, the api-token file, the run store, the render tree in 1.1, and
perseus's `.xml` files). So a render cursor that names a commit the raw
store does not have means the file was replaced by hand.
[`scan_buckets`](../../../datalib/backend/etl/src/doltlite_raw.rs)
handles it correctly — cold start — but logs it at `info`
("dolt_diff scan could not use this cursor — cold-starting"). Under the
principle that is the one event that should be `warn`, every run until
somebody looks, because it is the only way left to lose the range once
1.1–1.3 are fixed.

One cursor that is already right: `grid_index` writes `source_cursors`
inside the index's write transaction, so it can never claim more than
the index holds. Its range is lost only through 1.1.

## Condition 2: every raw commit is a consistent snapshot

### 2.1 The interrupt hook commits whatever is there

[`RawStoreCheckpoint::checkpoint`](../../../datalib/backend/etl/src/raw_store.rs)
is `commit_run(&self.pool, "download <source>: interrupted (Ctrl-C)")`,
unconditionally. It is registered by every `open_store`, which every
doltlite-backed ingest calls, and fired by the SIGINT handler in
[`datalib_step/src/main.rs`](../../../datalib/backend/datalib_step/src/main.rs)
for every registered hook. It does not consult `checkpoint_policy`.

So the sentence in the streaming plan — "Checkpointing is disabled for
the whole run when `DATALIB_DAG_RESET_AND_REDOWNLOAD` is set" — is true
of `wrote()` and false of Ctrl-C. A Ctrl-C during any of these commits
a store that is mid-wipe:

| run | what the working set holds when interrupted |
|---|---|
| `--reset-and-redownload` or `always_clear_before_ingest`, any provider | every table truncated, refill partway |
| lightroom, apple_photos, whatsapp ([`sqlite_mirror`](../../../datalib/backend/etl/sqlite_mirror/src/mirror.rs)) | `drop_all_mirror_tables` in one SQL transaction, then `rebuild_table` per table each in its own — so anywhere from "every table gone" to "all but the last rebuilt" |
| pdf | `reset_paths` before the walk, walk partway |
| fsindex | `db.reset()` before the index, index partway |

The commit lands in `dolt_log` with a message saying it was
interrupted, and the diff from any earlier commit to it says every
row left.

### 2.2 A crashed wiping run is committed by the next writer

A run that dies without Ctrl-C — a panic, a `kill -9`, a lost
laptop — leaves the wipe in the working set, which doltlite keeps in
the file. Nothing commits it *then*: readers use `open_reader` and
`pin::head` resolves the last real commit, so a render that runs
against the crashed store sees the pre-wipe state, which is the right
answer. But the next writer to open the file —
[`rescue_dirty_working_tree`](../../../datalib/backend/etl/src/doltlite_raw.rs)
inside `open` — commits it as `rescue: pre-run snapshot of orphaned
working tree`. That is the right thing for a run that died mid-*insert*
(the rows are real and would be folded into the next commit anyway) and
the wrong thing for one that died mid-*wipe*, and `open` cannot tell
which it was.

### 2.3 Both are unreachable through the runner and reachable by hand

Through `datalib-dag`, neither torn commit is ever read: a failed or
cancelled step "blocks its dependents this run"
([dag README](../../../datalib/backend/dag/README.md#what-a-run-executes-and-what-makes-a-step-stale)),
the ingest is stale on the next run (never succeeded since), and a
render pins `HEAD` at its own start (`pin::head`, not the announced
checkpoint), which by then is the completed refill. The diff from the
pre-wipe cursor to that commit skips over the torn one and is correct.
The streaming dispatch does not change this — under a wiping run no
`wrote()` fires (2.5), so no consumer is dispatched early.

By hand, both are one command away. After a Ctrl-C'd reset, or after a
crash and one `open`, run the source's `render_markdown` step directly
— `datalib-step` with the step's environment, or any future "run just
this step" affordance in the Manage screen — before a successful
ingest. Render pins the torn commit, the diff says every bucket's rows
were removed, `buckets_without_rows` confirms it against the wiped
tables, and `remove_conversation` deletes every document the source
has. The next full run puts them back. That is the delete-then-re-add
pair the principle exists to rule out, and nothing but the runner's
ordering stands between it and a user today.

### 2.4 A reader that commits: the slack applet

[`applets/src/slack/mod.rs`](../../../datalib/backend/applets/src/slack/mod.rs)
`read_rows` opens the source's `indexed_markdown.doltlite_db` — the
render store, which the render step owns — through
`doltlite_raw::open_derived`. That is the write path: it runs
`SELECT count(*) FROM dolt_status`, rescue-commits whatever is dirty,
and commits the schema with `-Am`. AGENTS.md §"One open per doltlite
file" is exact about what each of those costs a live writer:
`dolt_status` from a second connection fails the writer's in-flight
commit and **the rows it inserted before that commit are gone
afterwards** (dolthub/doltlite#2832, #400), and a `-Am` commit sweeps
the writer's uncommitted rows into a commit it did not make.

The render step is that writer whenever a user opens the Slack card
during a sync. Its checkpoint commits can fail and lose documents; and
because the step runs no transaction (1.3), an applet commit landing
between `apply_markdown`'s `DELETE FROM grid_rows` and its inserts
seals a document with no rows, which `grid_index` — streaming from
this store — can pin. That last one is transient: the render's own
final commit names the document again and the index re-applies it.
The lost rows are not.

`lint_repo.py` check 5 exists for exactly this and does not see it: it
walks `_render` crates only. The applet needs `open_reader`, and the
check needs to cover `applets/` and `http/`.

### 2.5 The wiping ingests are protected by omission

The streaming plan says whatsapp, pdf and fsindex "take `Never`". They
do not: no provider names `Policy::Never` anywhere, and
`checkpoint_policy` returns `Never` only under the reset flag. What
actually protects them is that none of the five wiping ingests
(lightroom, apple_photos, whatsapp, pdf, fsindex) ever calls `wrote()`,
so the default cadence has nothing to fire on. Nothing in any of those
crates says "I wipe, so I must not checkpoint"; a future contributor
adding a progress call to one of them would start sealing mid-wipe
with no test to stop them, and 2.1 shows the interrupt path already
does. (The plan's table also names `download.rs` /
`truncate_wa_tables` for whatsapp; whatsapp now goes through
`sqlite_mirror` from `ingest.rs` and has no truncate of its own.)

### 2.6 What honours the condition

The four streaming ingests — claude, chatgpt, slack, email — are the
only ones that seal mid-run, and every deletion they make is inside a
SQL transaction and reflects something upstream said:

- claude and chatgpt prune conversations the listing no longer returns
  (`db.rs`, "begin prune tx");
- slack deletes replies gone from a thread it just fetched whole
  (`delete_messages`, one transaction);
- email's `refresh_email_joins` deletes and re-inserts an email's
  mailbox and keyword rows inside the caller's transaction, so no
  checkpoint can see the gap.

`wrote()` is called after those transactions commit, so a seal lands
only between consistent states. This is condition 3 in miniature as
well: these are the ingests that *record* an upstream deletion as a
`removed` row, which is what makes render's diff meaningful. The
mirror and scan ingests record it through the wipe. The rest (github,
gitlab, notion, beeper, contacts, linkedin, google_takeout, signal,
sms_backup_restore, yolink, media) were not audited for it here; #27's
list and `render_inputs.md` condition 3 are where that belongs.

## What to fix, in order

1. **Cursor into the render store, written in the final commit.**
   Closes 1.3 (structurally, where today it is safe by ordering) and
   1.4, and is what 1.1 and 1.2 build on. `grid_index`
   already has the shape (`source_cursors`, in-transaction).
2. **Stop `discard_tree`.** A version bump puts every bucket in the
   re-render set and keeps both ranges. Closes 1.1 for render and for
   the index.
3. **A param change is "render everything", not "no cursor".** Closes
   1.2, and with `render_inputs` the cold-start sweep finally prunes
   what the new params no longer produce.
4. **Wipe at the end, not the start.** The structural fix for 2.1, 2.2
   and 2.5 together: a refill that upserts and then, in one transaction
   at the end, deletes every row not seen this run (the bookkeeping
   sidecar's `last_seen_at` is already the mark) is never torn, at any
   commit, so the interrupt hook and the rescue commit need no
   special case and `Never` stops being load-bearing. The
   `sqlite_mirror` engine is the hard case — its drop-and-recreate is
   deliberate (lightroom `INGEST.md` §"reconcile") and the doltlite
   blob bug it found is why — so measure before assuming; the fallback
   is the smaller fix below.
5. **Failing that: make the interrupt hook and the rescue honour the
   wipe.** The hook asks the policy and, under `Never`, discards the
   working set instead of committing it (doltlite's `dolt_reset
   --hard` equivalent — not verified to exist; check). The rescue at
   `open` has no way to know the prior run wiped, so a wiping run
   records that it is one (a `sync_runs` row, written and committed
   before the wipe) and `open` discards rather than commits when the
   last run row says so and never finished.
6. **The slack applet uses `open_reader`; lint check 5 covers
   `applets/` and `http/`.** Closes 2.4.
7. **`scan_buckets`'s unusable-cursor path logs at `warn`.** 1.6.
8. **Fix the prose:** the recipe's "wipe the cursor" (1.5); the
   streaming plan's "disabled for the whole run" (2.1), its "take
   `Never`" (2.5), and its whatsapp row (2.5).

Items 1–3 are `render_inputs.md` step 1 spelled out. Items 4–6 are
new, and 4 or 5 has to land before the render side can claim the
principle, because until then a torn commit is one hand-run step away.
