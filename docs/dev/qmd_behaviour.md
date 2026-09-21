# How qmd behaves when driven from a step

What was measured about `qmd` 2.8.3 (the pin in
`datalib/backend/runtime/src/qmd.rs`) while designing per-source
embedding on PR #456. Nine facts, each measured on a mac against the
TNG fixture's rendered tree (16 groups, 79 documents, one qmd
collection per group, exactly as the shipped `qmd_index` step registers
them), with the recipe at the end so they can be re-measured after a
qmd bump. File references are into `third-party/qmd/src`, the vendored
reference snapshot (`docs/dev/qmd_vendored.md`); function names are
given so a line number that drifts is still findable.

Read this before touching `qmd_indexer/src/lib.rs` or anything that
shells `qmd embed`. Findings 5 and 6 describe how the step **today**
can report success for work qmd did not do.

## What was measured

1. **`qmd update` cannot be scoped to a collection.** `updateCollections`
   (`cli/qmd.ts`) loops over `listCollections(db)` — every collection in
   the store — and there is no `-c` on `update` in 2.8.3 or on upstream
   `main` (checked 2026-09-15). Only `embed` takes `-c`, and only one
   name. So a per-source step that shells `qmd update` would re-hash
   every source's files on every run: O(sources × corpus), and a step
   whose declared input is one tree while it reads all of them.

2. **Writing a source's `documents`/`content` rows ourselves works, and
   qmd cannot tell the difference.** `reindexCollection` (`store.ts`)
   does exactly this per file: `path` is the literal relative path (the
   `handelize` mangling `plans/qmd_index_ui.md` describes is display-only
   now), `hash` is a plain SHA-256 of the UTF-8 bytes, `title` is the
   first `#`/`##` heading, and the FTS rows come from triggers on
   `documents` (`documents_ai`/`_au`/`_ad`). Measured: after an `UPDATE
   documents SET hash=…` plus an `INSERT OR IGNORE INTO content` from
   the sqlite3 shell, `qmd search` found the new text, `qmd embed -c`
   embedded it, `qmd vsearch` found it, and a following `qmd update`
   reported it `unchanged`. One quirk: the `INSERT … ON CONFLICT DO
   UPDATE` upsert qmd's own `insertDocument` uses failed with
   `constraint failed (19)` from the stock sqlite3 3.51 shell, while a
   plain UPDATE and a plain INSERT both worked. A Rust writer for these
   tables was built on #456 and backed out (see below); the branch has
   it if it is ever wanted.

3. **`qmd embed -c <group>` is scoped, and it is the only lever needed
   on the embed side.** Embedding `slack` alone left every other
   collection at zero vectors (per-collection counts by SQL, recipe
   below). qmd's own notion of pending is `getHashesNeedingEmbedding`
   (`store.ts`): active documents whose hash has no `content_vectors`
   rows for the current `(model, embed_fingerprint)`, or fewer than
   `total_chunks`. The fingerprint is computed in qmd's own code
   (`store.ts`, the `EMBED_FINGERPRINT_*` probes: model name plus
   chunking and formatting constants), so Rust cannot compute "pending"
   without asking qmd or re-deriving that.

4. **`update` and `embed` can overlap on one file.** qmd opens the store
   with `busy_timeout = 120000` and WAL (`db.ts`), explicitly so that a
   parallel `update` or `query` racing a long `embed` queues at batch
   boundaries instead of failing. Measured: three `qmd update` runs
   during one full `qmd embed`, all four exited 0, no errors, and the
   vector count was right afterwards.

5. **Two `embed`s cannot overlap, and the loser exits 0.** 2.8.3 has a
   process lock (`cli/embed-lock.ts`, `.qmd-embed.lock` beside the index)
   because concurrent embeds race on `vectors_vec`. Measured: the second
   of two concurrent `embed -c` calls printed `Another embed process is
   already running. Skipping.` and **exited 0 having embedded nothing**.
   A step that trusts that exit code reports success for work it did
   not do — the fallback-that-succeeds `AGENTS.md` warns about. The
   lock file is recovered by PID check after a crash (measured: a
   SIGTERM'd embed left the file; the next embed took it and finished).

6. **`qmd embed` stops itself after 30 minutes and still exits 0.**
   `DEFAULT_EMBED_MAX_DURATION_MS = 30 * 60 * 1000` (`store.ts`); on the
   cap it prints `⚠ Session expired — skipping remaining document
   batches` and then `✓ Done!` (`cli/qmd.ts`). `--timeout <minutes>`
   sets it; `0` disables it. **This is a live bug in the shipped step:**
   `run_index` (`qmd_indexer/src/lib.rs`) runs a bare `qmd embed` and
   trusts its status, so a root whose first embed needs more than half
   an hour reports `Succeeded` with most documents unembedded, and
   nothing re-runs it until a render moves. Tracked as #617.

7. **Embedding resumes.** Vectors are committed per batch; an
   interrupted run leaves what it finished, and the pending query counts
   the rest (partial documents count as pending, and
   `removeIncompleteEmbeddings` cleans them up). A re-run picks up where
   it stopped.

8. **Cost, for scale.** 79 documents: `update` 0.1s, full embed 9–11s on
   a mac GPU (CI is CPU-only and the fixture's embed action is ~90s).
   Model load is ~2–3s per `qmd embed` invocation, so a loop that
   spawns qmd per collection pays that per collection.

9. **`qmd collection add` indexes the tree as it registers it**
   (`collectionAdd` → `indexFiles`, `cli/qmd.ts`), and a collection is
   a YAML entry in `<XDG_CONFIG_HOME>/qmd/index.yml` — with the data
   root's *absolute* path — that qmd syncs into `store_collections` on
   every start. Registering a new source therefore costs one qmd-side
   scan at `collection add` and a second at the `qmd update` that
   follows; the second finds every row unchanged.

One more, about our side rather than qmd's: `QmdDaemon`
(`unified_index/src/qmd/daemon.rs`) respawns `qmd mcp` whenever
`index.sqlite`'s mtime differs from the one it spawned against. The
mtime moves on every embed batch, so during a long embed every search
reloads the model. Whether a live `qmd mcp` sees rows committed after
it started (WAL says yes; whether qmd caches a document list is the
question) was not measured.

## What was tried with these, and where it stands

PR #456 built per-source `<g>/qmd_embed` steps on top of findings 3–7:
a loop over `qmd embed -c <g> --timeout 0` that re-reads pending
between invocations rather than trusting the exit code, a `lock` key
in the runner so two embeds never overlap, and a per-collection gauge
polled from qmd's SQLite for progress. A first cut also made the
keyword index per source by writing qmd's tables from Rust (finding 2);
that worked and was backed out as a third of the change. The PR was
closed unmerged as larger than the ask deserved; issue #468 is the
decision record — what was built, what still bothered, and the options
(trim and merge; a per-source `embed` flag on the one global step; ask
upstream for `status -c` and `embed --json`). The branch
`claude/per-source-indexing-embeddings-735411` has the code.

## Re-running the measurements

```sh
S=/tmp/qmd-exp; mkdir -p $S/root
tar -xf bazel-bin/tests/fixtures/ingested/qmd_md.tar -C $S/root --strip-components=1
export XDG_CACHE_HOME=$S/root/unified_index/qmd_index XDG_CONFIG_HOME=$XDG_CACHE_HOME NO_COLOR=1
mkdir -p $XDG_CACHE_HOME/qmd && ln -sfn ~/.cache/qmd/models $XDG_CACHE_HOME/qmd/models
qmd() { node bazel-bin/third-party/qmd/runtime/node_modules/@tobilu/qmd/dist/cli/qmd.js "$@"; }
for g in $(ls $S/root | grep -v unified_index); do
  qmd collection add $S/root --name $g --mask "$g/render_markdown/**/*.md"; done
qmd update
qmd embed -c slack                       # finding 3
(qmd embed -c notion &) ; sleep 0.3; qmd embed -c beeper; echo $?   # finding 5: "Skipping", exit 0
(qmd embed &); for i in 1 2 3; do qmd update; done                  # finding 4
sqlite3 $XDG_CACHE_HOME/qmd/index.sqlite \
  "select d.collection, count(distinct d.hash), count(distinct v.hash)
     from documents d left join content_vectors v on v.hash=d.hash
    where d.active=1 group by 1;"
```

Finding 2's direct write is an `UPDATE documents … / INSERT OR IGNORE
INTO content …` pair against that file with `readfile()`, then `qmd
search`, `qmd embed -c`, `qmd update`.
