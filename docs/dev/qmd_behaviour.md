# How qmd behaves when driven from a step

Measured facts about `qmd` 2.8.3, which is also the version pinned now
(`DEFAULT_QMD_VERSION` in `datalib/backend/runtime/src/qmd.rs`, and
`third-party/qmd/package.json`). Eleven facts, each measured on a mac
against the TNG fixture's rendered tree (one qmd collection per group,
exactly as the shipped qmd steps register them), with the recipe at
the end so they can be re-measured after a qmd bump, and a twelfth read
from qmd's code and not measured. Findings 2–9 were measured on a
fixture of 16 groups and 79 documents; findings 1 (its SDK half), 10
and 11 on one of 22 groups and 111 documents. File references are into
`third-party/qmd/src`, the vendored reference snapshot
([`qmd_vendored.md`](qmd_vendored.md)); function names are given so a
line number that drifts is still findable.

Read this before touching `qmd_indexer/src/lib.rs`,
`qmd_indexer/src/js/qmd_sdk.mjs` or anything that drives `qmd embed`.
Finding 6 is why the embed step calls past `store.embed()`. Findings 1
and 3 are held by `//datalib/backend/qmd_indexer:qmd_indexer_tests`,
which drives the real qmd through `Index`. The facts about the search
side, under "How a running `qmd mcp` behaves" below, are each a test in
`//datalib/backend/qmd_facts:qmd_facts_test`, named for the fact: run
it after a qmd bump, and a fact that moved fails by name.

**The CLI and the SDK are not the same program.** `@tobilu/qmd` ships
both a CLI (`dist/cli/qmd.js`) and a library entry (`dist/index.js`,
its only declared export). Several of the limits below are the CLI's
alone — finding 1 most of all — so a fact measured by shelling `qmd`
does not carry over to `createStore()` without being re-measured.

## What was measured

1. **`qmd update` cannot be scoped to a collection *from the CLI*.**
   `updateCollections` (`cli/qmd.ts`) loops over `listCollections(db)` —
   every collection in the store — and there is no `-c` on `update` in
   2.8.3. Only `embed` takes
   `-c`, and only one name. So a per-source step that shells `qmd
   update` would re-hash every source's files on every run:
   O(sources × corpus), and a step whose declared input is one tree
   while it reads all of them.

   **The SDK can scope it.** `createStore().update({ collections: [...] })`
   filters `getStoreCollections(db)` by name and calls the same
   `reindexCollection` per surviving collection (`index.ts`, the
   `update` member). Measured on the fixture: with one file edited in
   `slack` and one in `notion`, `update({collections:["slack"]})`
   reported `{collections: 1, updated: 1, unchanged: 12}` — slack's 13
   documents — fired `onProgress` for `slack` and no other collection,
   moved slack's row to a new hash, and left notion's row on its old
   one. A second run scoped to `notion` then picked notion's edit up.
   Checked on the keyword index itself and not just on
   `documents.hash`: after the slack-scoped run, `documents_fts` did
   not match a sentinel word written into the notion file, and after
   the notion-scoped run it did. With nothing to do, three runs each:
   scoped to `slack` 8/12/11 ms, unscoped 27/39/29 ms.

   So the keyword index **can** be built per source through qmd's own
   code, without finding 2's direct writes.

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
   tables was built on #456 and backed out; the shipped steps use
   finding 1 instead.

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

   The shipped step does not inherit this: `qmd_sdk.mjs` takes the lock
   itself (`dist/cli/embed-lock.js`, which the SDK does not re-export),
   and `qmd_indexer` fails the step on the `busy` event it emits when
   the lock is held, on the grounds that the step owns the index and
   nothing else should be writing it. The hazard is still qmd's, so
   anything new that shells `qmd embed` gets it back.

6. **`qmd embed` stops itself after 30 minutes and still exits 0.**
   `DEFAULT_EMBED_MAX_DURATION_MS = 30 * 60 * 1000` (`store.ts`); on the
   cap it prints `⚠ Session expired — skipping remaining document
   batches` and then `✓ Done!` (`cli/qmd.ts`). `--timeout <minutes>`
   sets it; `0` disables it. The SDK's `embed()` forwards `force`,
   `model`, `collection`, `maxDocsPerBatch`, `maxBatchBytes`,
   `chunkStrategy` and `onProgress` and **drops `maxDurationMs`**
   (`index.ts`, the `embed` member), so through it every embed takes
   the 30-minute default. The shipped step therefore calls
   `generateEmbeddings` from `dist/store.js` directly, with
   `maxDurationMs: 0` (see "What the shipped steps do", below).

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

10. **`createStore({ dbPath, configPath })` deletes every collection the
    config file does not name — including when that file does not
    exist.** `createStore` calls `loadConfig()` and then
    `syncConfigToDb` (`store.ts`), whose last step is `DELETE FROM
    store_collections WHERE name = ?` for every row not in the config.
    A missing file loads as an empty config, so the delete takes all of
    them. Measured: `store_collections` was empty after one
    `createStore` against a `configPath` that was not there, having held
    every registered collection the moment before. The `documents` rows
    survive — it
    is the registry that goes — but a later scoped `update` then matches
    nothing and silently does no work, which is how this was noticed.
    `embed` and `status` in `qmd_indexer/src/js/qmd_sdk.mjs` open with
    `{ dbPath }` alone, the DB-only mode that reads `store_collections`
    and syncs nothing. `register` and `update` pass the file, because
    they are its writers and register their collections as they open;
    a file deleted by hand costs the other collections' registration
    until `qmd_aggregator` next runs, never their documents.

11. **The SDK reports progress the CLI keeps to itself.** `embed`'s
    `onProgress` fires per batch with chunks embedded, bytes processed
    and total, and the active error count; `update`'s fires per file
    with the collection, the file and a position. Both are plain
    callbacks on the store, so nothing has to parse a progress bar or
    poll qmd's SQLite. The CLI computes the same embed numbers and
    writes them only when `process.stderr.isTTY` (`cli/qmd.ts`), as a
    `\r`-redrawn bar — which is why a piped `qmd embed` says nothing at
    all between its model line and its last one. The step's progress
    reporting is built on these callbacks (#679).

12. **Two keyword updates on one index can lose a document's body.**
    Read from the code, not measured. `reindexCollection` (`store.ts`)
    runs statement by statement with no transaction around a file: it
    inserts the file's `content` row, then the `documents` row naming
    it. And every pass ends with `cleanupOrphanedContent`, which deletes
    every `content` row no `documents` row names. A second update
    finishing between the first one's two statements deletes a body the
    first is about to point at, leaving a document with no content —
    and since the next update finds its hash unchanged, nothing repairs
    it. Scoped updates on different collections still share the
    `content` table, so scoping does not help. The per-source steps
    therefore hold the runner's one-slot `qmd_keyword` lock, and so does
    `qmd_aggregator`, which registers collections too: registering
    through the SDK rewrites `index.yml` whole, and two of those at once
    would lose one's collection.

## How a running `qmd mcp` behaves

What the search (`QmdDaemon`, `unified_index/src/qmd/daemon.rs`)
relies on. Each is a test in `qmd_facts_test`, which builds a small
index through `Index` and talks to the pinned `qmd mcp` directly, so
the facts are about qmd and not about our daemon.

1. **A running server reads the index live.** A document keyword
   indexed after the server started is found by a `lex` query, and one
   embedded after it started is found by a `vec` query, with no
   restart. qmd prepares each query's statements against the open
   database (`dist/store.js`), and nothing it caches holds rows.
2. **Only the first server after a keyword update writes.** At startup
   `createStore` reconciles the collection registry with `index.yml`
   (`syncConfigToDb`) unless the index already holds that file's hash
   in `store_config.config_hash`, and a keyword update through the SDK
   leaves the hash stale. So the first server writes the registry and
   the hash, which reach `index.sqlite` when its WAL is checkpointed
   on stop; the next server starts, searches and stops without
   touching the file.
3. **With no `collections`, a query searches the collections the
   server read at startup** (`defaultCollectionNames`,
   `dist/mcp/server.js`). A collection registered later is searched
   only when a query names it.
4. **An empty `collections` list is no scope at all.** qmd answers it
   from every collection the index holds, one registered after the
   server started included, as a single search with no collection
   filter. So the daemon sends `[]` for an unscoped search, and
   answers an empty *scope* (no source can match) itself, never
   asking.
5. **A scope applies before the limit.** With room for one hit, a
   query scoped to a collection gets that collection's best hit even
   when another collection's ranks above it. This is why `source_id:`
   is sent to qmd as a scope (`collection_scope` in
   `applets/src/unified_index/mod.rs`) and not only applied to its
   answer: a source whose hits would fall outside the global top-N
   still fills the answer. `source_id_scopes_qmd_before_its_limit` in
   the applet's tests checks that the scope reaches qmd.
6. **A keyword query needs no model.** A `lex` query answers with no
   embedding model anywhere qmd could load one from. Measured on a
   real root, a `lex` query took 0.07–0.23 s and a hybrid one
   5.8–10.6 s on a fresh server.
7. **Each sub-query takes 20 documents from each collection, and the
   merged list is cut to `candidateLimit`, 40 unless asked.** qmd's
   structured search fetches the best 20 of each collection it searches,
   for each `lex` and each `vec` sub-query (hard-coded in
   `structuredSearch`, `dist/store.js`), merges them, and keeps the
   first `candidateLimit`, with rerank off as much as on, whatever
   `limit` says. `QmdDaemon` sends `candidateLimit` equal to `limit`, so
   a search reaches 20 per source per sub-query. On a real root with 11
   sources a hybrid search for one common word went from 40 hits to
   224, in the same time. One source never answers with more than 20 a
   sub-query; nothing in the MCP arguments moves that. An unscoped
   search, sent as `[]` (fact 4), is one collection's worth: 20 a
   sub-query in all.
8. **Several named collections are ranked apart and merged by rank
   alone.** `structuredSearch` runs each sub-query once per named
   collection and fuses the lists with reciprocal rank fusion, which
   sees only each document's place in its own list, and weighs the
   first list double. So every collection's best document scores
   alike, the first collection named leads whatever its match, and the
   answer is each collection's best, then each one's second, in the
   order the collections were named. On the TNG fixture, a vector
   search for each document's own opening words put that document in
   its top ten 56 times in 139 with every collection named, and never
   for any source after the tenth alphabetically; with `[]`, 135
   times.

What follows for our side. By facts 3, 4 and 8, an unscoped search
sends `collections: []` (`query_arguments` in
`unified_index/src/qmd/daemon.rs`): it reaches every collection,
however new, and ranks them as one list, at the price of fact 7's
depth, 20 documents a sub-query in all. Only `source_id:` names a
collection, and only one. Then by fact 1 a write into the
index needs no new `qmd mcp`, so `QmdDaemon` starts another only when
`index.sqlite` is a different file than the one its child opened (its
device and inode), not when its mtime moves, which every keyword and
embed batch does.

## What the shipped steps do with these

`qmd_aggregator` keeps the collection set to the sources it reads and
indexes nothing. Each source has a `keyword_index` step (registers its
own collection, then `update({collections:[g]})`, finding 1) and an
`embed` step (embedding scoped to `g`), both driven through
`qmd_indexer/src/js/qmd_sdk.mjs` and kept apart by the runner's
one-slot `qmd_keyword` and `qmd_embed` locks
(`dag/src/supervisor/locks.rs`; findings 5 and 12). Each step is its
own process, so each pays finding 8's model load; that is the price of
per-source steps over one loop calling `embed({collection})`.

Finding 6 is fixed rather than looped around: the script calls
`generateEmbeddings` with `maxDurationMs: 0`. Calling `store.embed()`
until it embeds nothing would never end on a document with a chunk that
always fails, because `removeIncompleteEmbeddings` drops that
document's good chunks at the end of each pass.

The alternative that was built and closed unmerged — a loop over `qmd
embed -c <g>` re-reading pending between calls, and a Rust writer for
qmd's tables (finding 2) — is PR #456, with the decision record in
issue #468.

## Re-running the measurements

**Pin `QMD_CONFIG_DIR`, and set the variables on separate lines.**
`getConfigDir` (`collections.ts`) takes `QMD_CONFIG_DIR`, else
`$XDG_CONFIG_HOME/qmd`, else `$HOME/.config/qmd` — and the checks are
for truthiness, so an *empty* `XDG_CONFIG_HOME` falls through to your
home directory. `export A=$S/x B=$A` leaves `B` empty, because the
shell expands the whole line before any of it is assigned. Get that
wrong and `collection add` writes the fixture's collections into your
own `~/.config/qmd/index.yml` (it merges, so nothing there is lost, but
they have to be taken back out by hand).

```sh
S=/tmp/qmd-exp; mkdir -p $S/root
tar -xf bazel-bin/tests/fixtures/ingested/qmd_md.tar -C $S/root --strip-components=1
export XDG_CACHE_HOME=$S/root/unified_index/qmd_index
export XDG_CONFIG_HOME=$XDG_CACHE_HOME
export QMD_CONFIG_DIR=$XDG_CACHE_HOME/qmd
export NO_COLOR=1
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

Finding 1's SDK half needs a script, since there is no CLI for it.
Append a sentinel word to one file in `slack` and one in `notion`,
then scope an update to one of them and ask the *keyword* index which
sentinel it can see:

```js
// node scoped.mjs <pkg-dir> <index.sqlite> <index.yml> [collection...]
import { pathToFileURL } from "node:url"; import { join } from "node:path";
const [pkg, dbPath, configPath, ...only] = process.argv.slice(2);
const { createStore } = await import(pathToFileURL(join(pkg, "dist/index.js")).href);
const store = await createStore({ dbPath, configPath });   // finding 10: must exist
const seen = new Set();
const t0 = Date.now();
const res = await store.update({
  collections: only.length ? only : undefined,
  onProgress: (p) => seen.add(p.collection),
});
console.log(Date.now() - t0, "ms", JSON.stringify(res), [...seen].sort());
await store.close();
```

```sh
sqlite3 $XDG_CACHE_HOME/qmd/index.sqlite \
  "select count(*) from documents_fts where documents_fts match 'SENTINEL';"
```

Finding 2's direct write is an `UPDATE documents … / INSERT OR IGNORE
INTO content …` pair against that file with `readfile()`, then `qmd
search`, `qmd embed -c`, `qmd update`.
