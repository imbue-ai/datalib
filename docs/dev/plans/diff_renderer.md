# Diff renderer: showing how a document changed between two commits

**Status: proposal (2026-09-16). Nothing here is built.** The claims
about what the render store holds are checked against the tree; the
claims about what doltlite can answer, and at what cost, are measured
against doltlite 0.50.3 (the version `MODULE.bazel` pins) on a
synthetic store and on real render stores. Each number below says which.

## What we want

A person looking at a rendered document — a Slack thread, a Claude
conversation, an email — should be able to ask "what changed here since
last week?" and get an answer at the level they think in: this message
was added, that one was edited, this reaction is gone. A step further
back, the same person should be able to ask a source "what changed in
you since I last looked?" and get the documents, not the row counts.

Both questions are diffs between two commits of one render store. The
first thing to settle is which commits are worth diffing between and how
to find them cheaply; the second is what a diff of one document is made
of, given what the store actually holds.

## What the store holds

A source's render store, `<group>/render_markdown/indexed_markdown.doltlite_db`,
holds one row per rendered document in `markdowns` (title, dates,
`renderer_version`, `bucket_key`, `md_path`), one row per message in
`grid_rows` (author, timestamp, the full `text`, and the
`markdown_uuid` it belongs to), the document's outgoing `edges`, and
`render_inputs` — what raw rows the document was rendered from
([`render_inputs.md`](render_inputs.md)). It does **not** hold the
`.md` file's bytes, nor a hash of them; the file is on disk beside the
store and is the only copy.

Two more facts matter here:

- **Nothing per run is written into a row whose content did not
  change.** That rule (`markdowns_carries_no_per_run_stamp` in
  `datalib_schema`) is what makes doltlite's content-addressed tables
  carry a diff for exactly the documents that moved. A render commit
  that changed nothing is an empty diff, and a document whose rows are
  identical across ten commits appears in none of their diffs.
- **`render_cursor.raw_commit` records which raw-store commit each
  render consumed.** It is one row, overwritten at every checkpoint,
  so its history (`dolt_history_render_cursor`) is the full mapping
  from render commit to raw commit. Verified on a real store
  (`z10/work-gmail`, 2026-09-16): every render commit has one.

So the store already knows, per commit, which documents changed and
what they were rendered from. What it does not know is what the
document *looked like* — only what its rows were.

## Which commits changed this document?

### The primitive that answers it

`dolt_diff_<table>` queried **without** a `from_ref` / `to_ref` filter
walks every adjacent commit pair on the branch and emits one row per
row that changed between them, with `to_commit`, `from_commit` and
`diff_type`. That is the upstream Dolt semantics for the same table,
and doltlite's `doltlite_diff_table.c::buildDiffPairs` does the same
walk. Each pair costs a prolly-tree diff, which is proportional to what
changed in that pair, not to the table's size; any column filter — the
primary key or any other — is applied on top by SQLite.

The per-document question is therefore one statement, the
`changed_since` bucket query in
[`indexed_markdown.rs`](../../../datalib/backend/etl/render/src/indexed_markdown.rs)
with the ref filter dropped and a uuid filter added:

```sql
SELECT to_commit, to_commit_date, 'markdowns' AS tbl, diff_type
  FROM dolt_diff_markdowns
 WHERE coalesce(to_markdown_uuid, from_markdown_uuid) = ?
UNION ALL
SELECT to_commit, to_commit_date, 'grid_rows', diff_type
  FROM dolt_diff_grid_rows
 WHERE coalesce(to_markdown_uuid, from_markdown_uuid) = ?
UNION ALL
SELECT to_commit, to_commit_date, 'edges', diff_type
  FROM dolt_diff_edges
 WHERE coalesce(to_src_markdown_uuid, from_src_markdown_uuid) = ?
```

Run verbatim against a real render store it answered in 7ms. Joining
its `to_commit` to `dolt_history_render_cursor.commit_hash` gives the
raw commit each of those renders consumed — the "rendered from
upstream commit" column, without storing one per document.

### Why not `dolt_history_<table>` or `dolt_blame_<table>`

They look like the obvious tools and our own
[`doltlite.md`](../doltlite.md) used to recommend them for exactly this.
Measured on a synthetic store of 200,000 `grid_rows`-shaped rows
(text primary key, a 400-byte `text` column) across 63 commits:

| statement | walks | measured |
|---|---|---|
| `dolt_history_gr WHERE uuid = ?` | every row of every commit | **20s** |
| `dolt_blame_gr WHERE uuid = ?` | every row of every commit | **20s** |
| `dolt_diff_gr WHERE to_uuid = ?` (no ref filter) | rows changed, summed over history | **~1s** for 311k changed rows |
| `dolt_diff` (commit → tables touched) | the commit log | 10ms |

The reason is in `doltlite_history.c` and `doltlite_blame.c`: the
primary-key pushdown (`doltliteBestIndexIntPkRange`,
`prollyCursorSeekInt`) exists only for **integer** primary keys. Every
store in this tree keys on a `VARCHAR`, so the filter is applied after
each commit's whole table has been materialized. `dolt_history_<t>`
also lists a row at every commit it *existed* in, changed or not, so
even where it is fast it still needs a self-compare to find the
changes.

### What the diff walk costs, and when to bound it

Its cost is the store's **total churn**: the initial render's rows plus
every row any later commit changed. A `renderer_version` bump
re-renders everything, so each one adds a table's worth of rows to that
sum for the life of the store. The synthetic store above carried one
initial render, sixty small edits and one 55% rewrite; a real store
with several layout bumps behind it will cost a few seconds per lookup
on a large source. `from_ref = '<old>..HEAD'` (the range-spec form,
`DT_IDX_RANGE_SPEC`) bounds the walk to a window when that matters,
and the commit-level `dolt_diff` says for free which commits touched
`markdowns` / `grid_rows` / `edges` at all — the coarse list of points
worth diffing between.

### Should `markdowns` carry a hash of the `.md`?

Yes, but not for this. The rows changing is *almost* the same event as
the file changing, and the gap is real in both directions: an author
rename from a `users` row shows up in `grid_rows.author`, but a change
to how attachments are materialized, or a layout change under the same
`renderer_version`, changes the file and no row. A blake3 of the file
in `markdowns` makes "did this document change?" exact, lets a tool
verify the file on disk still matches the store, and does not conflict
with the no-per-run-stamp rule: it is derived from content, so an
unchanged document writes the same value. The "no fingerprint" decision
in `render_inputs.md` was about using a hash to *skip writes*; this
uses one to *name what was written*.

## What a diff of one document is

The `.md` bytes at an old commit are unrecoverable — the file is
overwritten in place and the store never held it. So a diff renderer
cannot diff markdown text between commits, and it should not want to:
the rows are the better material. For one `markdown_uuid` and two
commits `a` and `b`:

```sql
SELECT diff_type, to_uuid, from_uuid, to_author, to_when_ts, from_text, to_text
  FROM dolt_diff_grid_rows
 WHERE from_ref = ? AND to_ref = ?
   AND coalesce(to_markdown_uuid, from_markdown_uuid) = ?
```

gives message-level `added` / `removed` / `modified` with both texts,
which is what a person means by "what changed in this thread". The
`markdowns` row's diff says whether the title or dates moved, and the
`edges` diff whether a link came or went. A modified message can be
shown as a word-level diff of `from_text` against `to_text`; an added
or removed one as the message itself, marked. Nothing here parses
markdown, which keeps the "QMDs are write-only" rule intact.

For the source-level question — "what changed since last week?" — the
same statement without the uuid filter, grouped by `markdown_uuid`,
lists the documents, and each is one click from its own diff.

## Where it lives

`GET /api/pipeline/history` (`http/src/history.rs`) already opens
render stores read-only and walks their commits through
`datalib_history`. Two routes beside it:

- `GET /api/pipeline/document_history?tree=<group>&markdown_uuid=<u>`
  — the per-document commit list above, each commit with its raw
  commit from `render_cursor` and its `run=` id from the message.
- `GET /api/pipeline/document_diff?tree=<group>&markdown_uuid=<u>&from=<a>&to=<b>`
  — the row-level diff above, shaped for the preview pane.

Neither touches the unified index; both read one source's render store
at two commits, which is what `history.rs` is for. The Manage screen's
commit-history panel is the natural place for the source-level list,
and the preview pane's page header for the document-level one.

## What has to be true before this ships

**A reader can break the writer.** AGENTS.md's one-open rule has a
clause that applies here word for word: a statement issued from a
read-only connection while a render step is sealing is presumed to fail
that step's commit until `a_history_reader_never_makes_the_writers_commit_fail`
has run with it. The unfiltered `dolt_diff_<t>` walk and the
commit-level `dolt_diff` are both new to that test. Add them before
either route exists; the history route's statements were added the
same way.

**Bound the walk.** A route that runs the unfiltered diff on a store
with years of churn is a route that times out. The per-document route
should take an optional `since=<commit>` and default it to something
the history panel already knows — the oldest commit it is showing.

**Measure a real store.** The numbers above are a synthetic store and
two small real ones. Before choosing the default window, run the
per-document statement against the largest render store on a real data
root (the one `multimodal_retrieval.md` §4 measured) and write the
number here.

## Open questions

- **Attachments.** `grid_rows` names an attachment's path, not its
  bytes; a swapped blob under the same path is invisible to this diff.
  The `blobs.doltlite_db` CAS knows, but the join is across stores.
  Probably out of scope for a first version.
- **Whether to keep old `.md` bytes at all.** A CAS of rendered files
  keyed by the blake3 above would make the file diff possible and
  double the bytes at rest `multimodal_retrieval.md` §4 already counts
  as too many. The row diff is the case against it; this document
  assumes we do not.
- **The applet or the server.** The routes above sit in `datalib-http`
  because that is where render stores are already read. If the preview
  pane's diff view ends up as an applet card, the endpoint moves with
  it; nothing in the queries cares.
