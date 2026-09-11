# Render inputs: record what each document was rendered from

**Status: proposal (2026-09-11), nothing built.** The measurements and
file pointers below were checked against the tree on that date; the
design has not been tried.

## The problem in one paragraph

Incremental render has to answer one question: *given the raw rows that
changed since the last render, which documents need rendering again?*
Today nothing stores that mapping. Each provider hand-writes SQL that
projects a changed raw row **forward** onto a document key, and where
that projection is not derivable from the row alone the provider falls
back to re-rendering everything. Deletion — the other half of the same
question — is answered by a second, different mechanism, and that
second mechanism is where every mass-deletion bug this repo has shipped
has lived. This proposal adds one table to the per-source render store
that records, for each rendered document, the raw rows it was rendered
from, and derives both halves of the question from it.

## What exists today

The render store (`<source>/render_markdown/indexed_markdown.doltlite_db`,
[`indexed_markdown.rs`](../../../datalib/backend/etl/render/src/indexed_markdown.rs))
holds `markdowns`, `grid_rows`, `edges`, `render_problems` and
`measurements`. Nothing in it says which raw rows a document came from.
`grid_rows.upstream_id` is a backpointer for the rows a document
*emits*, which is not the same set as the rows it *reads*: a Slack
thread document reads its messages, but also the `users` rows for every
author, the `channels` row and the `workspaces` row, and emits nothing
for any of those.

Which documents to render is decided per provider, in
`parse.rs`, by handing
[`scan_buckets`](../../../datalib/backend/etl/src/doltlite_raw.rs) a
`DiffScanSpec`:

- `bucket_query` — a `UNION` over `dolt_diff_<table>` for the tables
  the provider considers primary, projecting each changed row to a
  bucket key (a conversation, a thread, a PR). The projection has to
  work for removed rows too, so it reads `coalesce(to_x, from_x)`, and
  where the key lives on a parent row it joins `pinned_<parent>` to
  recover it — which silently finds nothing when the parent was
  removed in the same range.
- `global_fanout_tables` — tables for which the provider gave up on
  projecting at all. One changed row in any of them re-renders the
  whole source. Slack lists `workspaces`, `users`, `channels`; claude
  lists `users`, `orgs`, `projects`; signal `recipients`; chatgpt `me`;
  notion `users`. A user editing their display name re-renders every
  Slack thread in the workspace. The content fingerprint then skips
  the *write* for documents that came out identical, but every payload
  is still loaded and every document re-rendered to find that out.

Which documents to **delete** is decided by whichever of two
mechanisms the provider was ported onto
([parse_and_render.md §"Two mechanisms"](../data_architecture_parse_and_render.md#two-mechanisms-because-there-are-two-kinds-of-renderer)):

- Diff-narrowed renderers call
  [`buckets_without_rows`](../../../datalib/backend/etl/src/doltlite_raw.rs)
  on the ids the scan named and hand the survivors to
  `RenderCtx::remove_conversation`. This is the half with the "porting
  precondition, learned the hard way" in the
  [migration recipe](../provider_migration_dolt_diff_and_cas_edge.md):
  *you must be able to compute a document's id from a diff row alone.*
  `contacts` fails it (one row holds several vCards) and so cannot be
  ported.
- Whole-store renderers call `RenderCtx::retain_documents` with every
  document they *considered*, and the driver sweeps the rest. This is
  the half with the trap: a renderer that reports what it *emitted*
  rather than what it *considered* deletes its own steady state, and a
  renderer that returns early with an empty set deletes the source.

Count the guards that exist only to keep the second mechanism from
deleting a live source: `RenderPass::Walked | Skipped`
([`processor.rs`](../../../datalib/backend/etl/render/src/processor.rs)),
the "considered, not emitted" rule on `RenderSummary.documents`, the
`pin::head` refusal of a store with tables but no committed schema
(streaming plan, step 5), `buckets_without_rows` treating a failed
probe as "every bucket present", and the driver only sweeping after
every processor succeeded. Five guards, in four files, against one shape
of bug — and the streaming plan records that we shipped it anyway,
through a gap between two of them. That is the smell the opening
question is about: the mechanism needs this many guards because it
infers deletion from *absence*, and absence has three meanings
(gone, not looked at, could not look).

Two more facts about the current shape, both from reading the driver
([`datalib_step/src/render.rs`](../../../datalib/backend/datalib_step/src/render.rs)):

- **A cold start deletes nothing.** When the cursor is missing or
  unusable, `changed_buckets` is `None`, so `buckets_without_rows` is
  asked about nothing. A conversation that vanished upstream while the
  cursor was unusable stays in the render store, the grid and qmd
  until something else names it. This is the "known gap: nothing
  prunes `render_markdown/`" in parse_and_render.md §5, and it is
  why changing a render param leaves orphans beside the new documents.
- **Every provider carries `prior_fingerprints`** in its render
  signature, and compares inside its own loop. #27 asked for that
  parameter to go; it has not, because the whole-store renderers need
  it to build the "considered" set.

## Where "deleted, then added back" comes from

The question that prompted this doc was whether the render store's
history shows a document leaving in one commit and returning in the
next. It can, by these routes. Only some of them are fixed here, and it
is worth being exact about which.

| route | what happens | fixed by this proposal? |
|---|---|---|
| A whole-store renderer's walk was narrower than its store — a filter, a partial failure it swallowed, a bucket it skipped — and it reported the narrower set to `retain_documents`. | Everything outside the walk is swept; the next run that walks it re-adds it. | **Yes.** Deletion no longer follows from absence in a reported set. |
| A `bucket_query` join to a `pinned_<parent>` finds nothing because the parent moved or vanished in the same diff range, so a child's change names no bucket; a later run names it. | Not a delete/add pair, but a document that is stale for one run and correct the next — the same shape from the user's side. | **Yes.** The reverse lookup does not join anything. |
| A cold start (no cursor, unusable cursor, params changed, renderer version bumped) re-renders everything. | With a content fingerprint the writes are skipped, so nothing is deleted or re-added — but every payload is read. `markdowns.rendered_at` moves only for documents actually rewritten. | Not a bug; unchanged. The fan-in table makes cold starts rarer (no `global_fanout_tables`) but does not change what one does. |
| A document's identity moved: a period boundary, a merged JMAP thread, a re-keyed uuid recipe. | Old uuid deleted, new uuid added. | Correct behaviour, unchanged. |
| A truncate-and-refill ingest (whatsapp, pdf, fsindex, `--reset-and-redownload`, an export ingest pruning to its snapshot) is read by render at a checkpoint taken mid-wipe. | Every bucket reads as removed, every document is deleted, the next commit brings them back. | **No.** This is a torn snapshot, not a mapping problem; the fix is the rule in the [streaming plan](streaming_steps_plan.md#the-rule-that-is-easy-to-get-wrong) that a wiping run does not checkpoint. Recording inputs does not make a wiped table look less empty. |
| A row that has not been fetched yet (no-preseed listing: a row appears only after its detail fetch) | Nothing to delete — the row was never there. | n/a |

So the proposal removes the routes that come from *inferring* deletion
and leaves the one that comes from *reading a torn store*, which no
mapping can fix.

## The principle: the raw store's diff *is* the deletion record

Everything below rests on one assumption, and it is worth stating as
a rule so it can be enforced rather than hoped for:

> **`dolt_diff` between two commits of our own raw store reports every
> row that left. Render never infers a deletion from anything else.**

Doltlite gives us this for free at the commit level — a row present at
`from_ref` and absent at `to_ref` is a `removed` diff row, however many
commits lie between, and a row that left and came back in between is
`unchanged` or `modified`, which is also the right answer. So "the raw
store has no row with this id" (what `buckets_without_rows` asks) is
never the primary signal; the diff already said `removed`, and the
probe exists only because the forward projection lost track of which
document that row belonged to. `render_inputs` is what lets the diff's
own `removed` rows name the document directly.

Three conditions turn "assume" into "ensure", and the tree fails two of
them today:

1. **Never lose the range.** The render cursor (`_render_cursor.json`)
   names the `from_ref`. Lose it and there is no diff, and with no diff
   there is no deletion information at all — only a full walk, which is
   the inference-from-absence this whole doc is trying to retire.
   Today it is lost on purpose in two places: `discard_tree` removes
   the whole `render_markdown/` directory on a renderer-version bump,
   and the cursor file lives inside it; and the migration recipe
   recommends wiping the cursor on `--reset-and-redownload`. Both
   conflate "re-render every bucket" with "forget where I was", and
   they are different things. A version bump, a param change and a
   reset all want *every bucket rendered again*; none of them wants the
   diff range dropped. A reset is a committed truncate followed by a
   committed refill, and the diff across that range is exactly right:
   rows in both are `unchanged` or `modified`, rows only in the old
   commit are `removed`. So: the cursor moves into the render store
   (a `render_cursor` table beside `render_inputs`, one row, the same
   two fields plus the params), the store is never discarded — a
   version bump rewrites its rows the way any other full re-render
   does — and `--reset-and-redownload` leaves the cursor alone.
   A cursor naming a commit the raw store no longer has then means the
   raw store *file* was replaced, which is not something datalib does
   and is worth a `warn` every run until somebody looks.
2. **Every raw commit is a consistent snapshot.** A perfect diff of a
   torn commit is perfectly wrong: a checkpoint taken mid-wipe says
   every row left, and the diff will report exactly that. This is the
   [streaming plan's rule](streaming_steps_plan.md#the-rule-that-is-easy-to-get-wrong)
   that a wiping run does not checkpoint; the principle here is why
   that rule is load-bearing for deletion and not only for progress.
3. **Ingest owns "upstream deleted it".** Render mirrors the raw store,
   not upstream. Whether a row leaves the raw store is each ingest's
   decision — an export ingest prunes to its snapshot, a mirror ingest
   truncates and refills, most API ingests never delete at all, and
   beeper must *not* delete when its local `index.db` evicts. Every one
   of those is a question about the source, answered in the ingest
   crate, and render needs no opinion on any of them. That layering is
   the real content of the principle: the diff is perfect *about the
   raw store*, and the raw store is ingest's claim about upstream.

One precondition: `dolt_diff_<t>` needs a primary key, so a mirrored
table without one is not diffable. `sqlite_mirror` already synthesizes
one where it can; a table it cannot key should be refused at DDL time
rather than silently left out of the diff.

With condition 1 held, the only inference-from-absence left in this
design is the cold-start sweep in §"Deletion, derived" step 3, and it
is reachable only when the range is really gone — which becomes an
anomaly to report rather than a path four routine events take.

## The proposal

Two additions to the render store, one to what a renderer emits, and a
driver that derives both halves of the question from them.

### `render_inputs`: what each bucket was rendered from

```sql
CREATE TABLE render_inputs (
    bucket_key   VARCHAR(256) NOT NULL,  -- provider-opaque; the unit the provider loads and renders
    input_table  VARCHAR(64)  NOT NULL,  -- a table in the raw store, bare name
    input_id     VARCHAR(256) NOT NULL,  -- that table's primary key, as text
    PRIMARY KEY (bucket_key, input_table, input_id)
);
CREATE INDEX render_inputs_by_input ON render_inputs (input_table, input_id);
```

One row per raw row a bucket's render **asked for** — every row it
loaded, and every row it looked up and did not find. The second half is
what makes a later arrival re-render the right document: a thread
rendered while its author's `users` row had not been fetched yet
records `(users, U123)` anyway, so when the row lands the thread is
named. "Asked for" is the rule, not "found".

Keyed on the **bucket**, not the document. A bucket is what the provider
loads and renders as a unit — a conversation, a thread, a PR, a page —
and it is what `scan_buckets` already hands back. A periodizing renderer
turns one bucket into several documents; the inputs belong to the
bucket, and the split into documents is the render's business. This is
also what keeps the table from doubling: a chat-level row that every
period document of a thread reads is recorded once.

A composite primary key is rendered as one text value with a separator
the driver owns, the same way `email` already folds `account_id |
thread_id` into one bucket key. The driver reads each raw table's
primary-key columns from `pragma_table_info`, so the same rendering is
applied on the diff side without the provider naming its own keys.

### `markdowns.bucket_key`

One column, so the driver can go from a stale document to the bucket
the provider must reload, and from a bucket to the documents that
should be swept if it re-renders to fewer. The provider emits it on
`RenderedMarkdown` beside `markdown_uuid`; it is the same string the
provider will be handed back in `changed_buckets`.

### What a renderer emits

`RenderedMarkdown` gains `bucket_key: String`, and `RenderCtx` gains
one call:

```rust
/// Every raw row this bucket's render asked for, found or not.
/// Called once per bucket the run looked at, before its documents
/// are emitted — including a bucket that turned out to have nothing
/// to render, which is how the driver learns the bucket is gone.
pub fn declare_bucket(&self, bucket_key: &str, inputs: &[Input]) -> Result<()>;

pub struct Input {
    pub table: &'static str,
    pub id: String,
}
```

`prior_fingerprints` leaves every provider signature. A renderer
always emits what it rendered; the driver compares
`source_fingerprint` against the store and skips the write when nothing
changed, exactly as `load_all_batch` in `grid_index` already does for
the index. That is #27's acceptance criterion, and this is what makes
it possible: the "considered" set that the whole-store renderers were
using the fingerprint map to build no longer exists.

### The scan, done by the driver

The provider still supplies a forward projection, but a smaller one:
`to_`-side columns only, `added` and `modified` rows only, primary and
child tables only. No `coalesce(to_, from_)`, no join to a parent that
may be gone, no `global_fanout_tables`. Removed rows never need
projecting, because a removed row was an input of something and the
reverse lookup names it.

```
changed   = for each table T the provider names or render_inputs mentions:
              SELECT 'T', <pk cols rendered as text>, diff_type
                FROM dolt_diff_T
               WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'

stale     = SELECT DISTINCT bucket_key FROM render_inputs
             WHERE (input_table, input_id) IN changed          -- reverse: old owners, referenced rows
          ∪ provider.forward(changed WHERE diff_type IN ('added','modified'))  -- forward: new owners

render    = stale, handed to the provider as `changed_buckets` — the same
            HashSet<String> it takes today
```

The raw store and the render store are two files, so the `IN changed`
above is a chunked probe from Rust rather than a SQL join, the way
`buckets_without_rows` already probes. Both stores are open for the
whole pass in any case.

"Render everything" — a renderer-version bump, a render-param change,
`--reset-and-redownload` — is the same path with `stale = every
bucket_key in markdowns` plus whatever the forward projection adds,
and the diff range kept, so deletion still comes from `removed` rows.
A true cold start (no range) is that plus a full walk. There is no
second code path for either.

### Deletion, derived

After the provider has run:

1. Every bucket in `stale` must have been **declared** this run. One
   that was not is an error — the provider was asked about it and did
   not answer — and the run fails *before* any sweep. This is the rule
   that replaces the five guards: deletion is never inferred from a
   bucket nobody mentioned.
2. For each declared bucket, its `render_inputs` rows are replaced
   with what it declared, and every document in `markdowns` carrying
   that `bucket_key` that was **not emitted this run** is removed —
   rows and `.md`. A bucket declared with no documents is a bucket
   whose primary entity is gone; a periodized bucket that re-rendered
   to fewer documents drops the extra ones. Same rule, no special
   case.
3. A cold start — the range is gone, per §"The principle" condition 1
   the only reason left — walks everything, and a bucket in
   `markdowns` that the walk did not declare loses its documents. That
   is the one place absence still means deletion. It never happens
   because a store could not be read (that fails the scan, and a
   failed scan has no `stale` set to sweep), and it never happens for
   a version bump, a param change or a reset, which keep the range and
   simply put every bucket into `stale`.

`remove_conversation`, `retain_documents`, `RenderPass`,
`buckets_without_rows`, `documents_for_conversation`,
`all_document_uuids` and `RenderSummary.documents` all go. So does the
storage report's exemption from the sweep — it is emitted under a
reserved bucket key the driver declares itself.

### What the shared loaders do

The rule in step 1 sounds like ceremony for the provider, and for the
ten chat-common providers it is not: their parse helpers already take
the asked set and load rows for it, so the helper declares every asked
bucket — with its inputs if rows came back, with the ids it queried if
none did. One implementation, ten providers. `email`, `github`,
`gitlab` and `notion` have the same shape in their own `parse.rs` and
each declares in one place.

## What a provider has to do

Before, per provider: a `bucket_query` that handles removed rows and
parent joins; a `global_fanout_tables` list; a call to
`buckets_without_rows`; a loop over the result calling
`remove_conversation`; or, instead of the last two, a "considered" set
threaded through the render and handed to `retain_documents`; and a
`prior_fingerprints` compare inside the render loop.

After: a `bucket_query` over `to_` columns of `added`/`modified` rows
in its primary and child tables; a `bucket_key` on each emitted
document; and, per bucket it loads, the list of `(table, id)` it asked
for — which its loader already knows, because it just asked.

The one thing a provider can get wrong is an incomplete input list: a
row it reads but does not declare will not re-render the bucket when
that row changes. That is a silent staleness, the failure class AGENTS.md
§"Fallbacks" warns about, so it gets a test rather than a review
checklist — see below.

## What this makes possible that is not possible now

- **`contacts` becomes portable.** One `contacts` row holding several
  vCards is several buckets declaring the same input. Nothing in the
  reverse lookup needs a diff row to name one document.
- **A user's display name change re-renders the threads that user is
  in**, not the workspace. Slack's `global_fanout_tables` entry for
  `users` is the difference between "load every payload in the
  source" and "load the buckets that declared `(users, U123)`".
- **Cold start deletes.** The orphans-after-a-param-change gap closes
  without a separate pruning pass.
- **Deletion under streaming is positive-evidence only.** A render
  reading a checkpoint of a live ingest deletes a document only when a
  declared input shows as `removed` in a committed diff and the
  re-render came back empty — never because a set was short.
- **A "why is this document stale?" tool** is a query:
  `SELECT * FROM render_inputs WHERE bucket_key = ?` joined against
  `dolt_diff` says exactly which raw row moved. Today that answer is
  "read the provider's bucket query and think".

## Costs and open questions

**Size.** One row per `(bucket, raw row)`. For a source whose documents
emit one grid row per raw row (every chat provider) this is roughly
`grid_rows`' row count again, plus the referenced rows (users, channels)
per bucket, in rows of three short strings. Doltlite's content-addressed
storage keeps unchanged rows free across commits. Measure against the
same real data root `multimodal_retrieval.md` §4 used before building —
that is step 0 below.

**The cross-store probe.** `IN changed` is chunked from Rust. For a
run where a `users` row changed in a 100k-thread workspace, `changed`
is one id and the probe is one indexed lookup. For a cold-ish run where
10k messages changed, it is 10k / `SQL_CHUNK` probes against an indexed
table. Doltlite `ATTACH` would make it one join; nobody has checked
whether doltlite supports attaching a second `.doltlite_db`, and the
one-writer rules in AGENTS.md make it worth not finding out in
production. Chunked probes are known to work.

**Composite and integer keys.** `WirePayloadRow` and `CasEdgeRow`
tables have a text `id`. The `sqlite_mirror` tables (lightroom,
apple_photos, whatsapp) carry whatever the mirrored file had, including
integer rowids and composite keys. The driver renders every key as text
from `pragma_table_info` order; the provider's `Input.id` must render
the same way, so the driver exposes the renderer as a function rather
than leaving each provider to format its own.

**Tables with no primary key.** `dolt_diff_<t>` needs one; a mirrored
table without one is not diffable today either, so nothing regresses.

**Whole-store renderers with no doltlite raw store.** `perseus` reads
`.xml` files. It keeps re-rendering everything; its inputs are file
paths, which the fan-in table can hold (`input_table = 'file'`) but
nothing diffs. Out of scope; it is out of scope for #27 too.

**The fingerprint is a content hash everywhere.** The migration recipe
says `source_fingerprint` "is now the bucket UUID"; that is stale. Every
renderer in the tree hashes what it rendered (`compute_fingerprint` in
chat-common, `fingerprint_for_pr`, `render_fingerprint(&blake3)`, …),
which is what makes the skip below worth keeping.

**Does the fingerprint still earn its place?** A bucket re-renders only
when a declared input changed, so its output nearly always changed too.
"Nearly": a `users` row whose only change is a field we do not render
re-renders every thread that user touched, to identical output. The
compare stays, in the driver, so those writes — and the grid and qmd
work downstream of them — are skipped. It costs one map load.

## Order of work

0. **Measure.** Count `(bucket, input)` pairs for the real data root
   by instrumenting one provider's loader; check it against the
   `grid_rows` count there. If it is not within small-integer multiples,
   revisit the bucket-level keying before writing any DDL.
1. **Keep the range.** Move the render cursor into the render store,
   stop `discard_tree` on a version bump (re-render in place), and
   stop wiping the cursor on `--reset-and-redownload`. Independent of
   the rest and worth landing first: it is the condition everything
   else assumes, and today's code fails it.
2. **The store.** `render_inputs` DDL in `datalib_schema`,
   `markdowns.bucket_key`, `declare_bucket` on `RenderCtx`,
   `bucket_key` on `RenderedMarkdown`. The driver writes both; the scan
   and the sweep are unchanged. Every provider compiles by emitting an
   empty declaration, and the driver logs at `warn` per source with no
   inputs — so the migration state is visible, not silent.
3. **The test that keeps input lists complete.** Over the TNG fixture:
   render cold, record `render_inputs`; then for every raw table, for
   every row, mutate one non-key column in a scratch copy of the raw
   store and run the incremental path; assert that the buckets which
   re-rendered to different output are a subset of the buckets that
   declared that row. Expensive, so it runs per provider as an
   `insta`-style golden of the *declared* sets rather than the
   mutation loop on every CI run — the mutation loop is the `.update`.
4. **chat-common.** Its parse helpers declare; ten providers move
   together. Drop their `global_fanout_tables`; verify with the test
   from step 3 that a `users` change names exactly the threads that
   user is in.
5. **The driver-side scan and sweep**, switching one provider at a time
   off `remove_conversation` / `retain_documents`. The migration
   recipe's "same commit" rule applies in reverse: a provider moves
   off the old deletion path in the same commit that its declarations
   become complete.
6. **Delete** the five guards, the two callbacks, `RenderPass`,
   `buckets_without_rows`, `prior_fingerprints` from every provider
   signature. Close #27.
7. **contacts.** Port it; it was the provider that could not be.

## Relation to other documents

- [#27](https://github.com/imbue-ai/datalib/issues/27) asks every
  provider to converge on `dolt_diff` and `prior_fingerprints` to go.
  This is the design that lets the second half happen.
- [`provider_migration_dolt_diff_and_cas_edge.md`](../provider_migration_dolt_diff_and_cas_edge.md)
  is the recipe this replaces the render half of. Its download half
  (per-provider CAS edges, `WirePayloadRow`, the no-preseed rule) is
  untouched.
- [`data_architecture_parse_and_render.md`](../data_architecture_parse_and_render.md)
  §"Two mechanisms" describes what this collapses into one. When this
  lands, that section is rewritten rather than appended to.
- [`streaming_steps_plan.md`](streaming_steps_plan.md) §"The sink
  contract" P1 — "absent and empty are different answers" — is the
  principle. This proposal is the render step finally honouring it
  structurally rather than by guard.
