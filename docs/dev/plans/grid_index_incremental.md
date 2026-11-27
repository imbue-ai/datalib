# The grid index pays only for what changed

*Proposal (2026-11-26). Order step 1, the keywords as their own part,
is built (`grid_index::Part`). Step 2 is built for the grid pass and the
keyword index (`grid_index::apply_page`, `LOAD_PAGE`); the render store
still writes a document at a time through it. Step 3 is built
(`Part::Contacts`, `contact_cursors`). No real library has been measured. Every number comes from
one real library's rebuild that day: 21 sources, 257k documents, 1.23M
grid rows. The step's own statement counts (`sql_*_total` in
`runs.sqlite`, `docs/dev/logging.md`) and its "sealed the index" lines
are the measurements. Facts about the tree were read the same day;
check one before relying on it.*

## The problem

**An upgrade that changes one table of the grid index rebuilds all of
it.** `unified_grid_and_search_index/db.sqlite` holds the grid (every
source's rows, edges, nodes, filings, photos and terms), the contacts
the sources describe, and the keyword search index. Its recorded shape
has one hash over the DDL of every table in it (`grid_index::shape`).
When a build's hash differs, `remove_if_another_shape` deletes the file
and the step builds it again from every render store. #2019 did that by
adding one small table, `contact_record_numbers`. In this library the
rebuild took 10m43s, and most builds since mid-November have done the
same. A build that left the shape alone ran the step in about 35 s.
While a rebuild runs, the grid and keyword search miss every source it
hasn't reached yet.

**The rebuild itself is slower than it needs to be**, and so is every
large write, for two reasons:

1. **A source read whole goes into memory whole.**
   `IndexedMarkdownStore::documents_matching(None)` reads every
   document of a render store (its `markdowns`, `grid_rows`, `edges`,
   `data_nodes`, `filings`, contacts and photos) into one
   `Vec<RenderedMarkdown>` before anything is written. The largest
   source here is 135k documents, bound into each read as one JSON
   array of 135k uuids, and the step peaked at 2.5 GB. A render, by
   contrast, reads a page of a few hundred render units and drops it
   before the next (`data_architecture_parse_and_render.md`
   §"Pages").
2. **Every row is its own statement.** `load_source` calls `apply_one`
   once per document. `apply_markdown` then runs seven `DELETE`s per
   document (one per table it owns), and one upsert per row, plus an
   owner lookup whenever the upsert wrote nothing. The keyword index's
   `Index::put` does the same: per document a lookup and an insert, per
   section an insert into `search_sections` and another into
   `search_fts`. Download, by contrast, writes through
   `bulk_upsert_in_tx`, many rows to a statement. The render step
   writes its own store through the same per-document code
   (`IndexedMarkdownStore::put_document` calls `grid_index::apply_one`),
   so it pays this on every render.

### What the rebuild spent

| Phase | Time | What it was |
|---|---|---|
| Read the render stores | 43 s | A few large `SELECT`s per table per source |
| Load the rows | 112 s | 4.87M statements. 54 s of it was inside SQLite; the rest is about 12 µs per statement outside it (sqlx handing each statement to its worker thread and back, binding, the Rust around it) |
| Terms | 65 s | Already set-based: `INSERT … SELECT`, a few thousand statements |
| Nodes, seal | 32 s | |
| Build the grid's indexes | 95 s | `create_read_index`, once, after the load |
| Keyword index | 4m42s | Reading the sections' text again (`read_kept_sections`, 113 s), then 929k `insert_section` (56 s), 929k `insert_fts_text` (10 s), 257k `insert_document` (21 s), and FTS5's own writes (10 s) |

In the load, the statements that ran most were the grid-row upsert
(1.23M), the data-node upsert (909k), the contact lookup and the
contact upsert (309k each), and the seven per-document `DELETE`s
(257k each, 1.8M in all). On an empty index every one of those
`DELETE`s finds nothing.

## The rule

**A change costs what it changes.** That holds for data, where it
mostly holds already (a sync that changes three documents writes
three), and with this plan it holds for the index's own shape too:

- **A new build that changes one part of the index rebuilds that part.**
  A change to the contacts tables re-reads the sources' contacts and
  nothing else. A change to the keyword tables rebuilds keyword search
  and leaves the grid as it was. Only a change to the grid's own tables
  rebuilds the grid.
- **There is one writer, for a page of documents, whatever the index
  already holds.** A source read whole and a source with three changed
  documents go through the same code; the first just has more pages.
  Every check the writer makes today stays, written over the page
  instead of over a row.

What a person notices: an upgrade like #2019 costs seconds instead of
ten minutes, and the grid and search stay whole while it runs. A
rebuild that does happen, a first sync of a big source and a full
re-render all get faster, and stop needing memory in proportion to the
largest source. A sync that changes a few documents behaves as now.

## Part 1: a shape for each part of the index

### The parts

| Part | Its tables | What says how far it has read each source |
|---|---|---|
| **Grid** | `grid_rows`, `markdowns`, `edges`, `data_nodes`, nodes, `filings`, `source_photos`, `problems`, `source_cursors`, `store_generation`, the terms tables | `source_cursors` |
| **Contacts** | `contact_records`, `contact_record_handles`, `contact_record_numbers`, `document_contacts` | none yet: today they are written beside the grid rows and ride the grid's cursor |
| **Keywords** | the `search_*` tables (`search_documents`, `search_sections`, `search_fts`, `search_state`, and what records each source's pass) | `Index::read_at` / `set_read_at`, already per source |

`_datalib_meta` is a key-value table, so each part records its own
table hash under its own key. `shape()` becomes the set of those, and
the index's indexes keep their separate hash as now (adding an index
already costs creating it, not a rebuild; this extends the same idea
from indexes to tables).

### On open

Where `remove_if_another_shape` deletes the file today, the open
instead compares each part's hash:

- **Every part matches:** nothing to do.
- **Some parts moved:** drop those parts' tables, create them in this
  build's shape, forget those parts' cursors, and record the new
  hashes. The parts that matched keep their rows.
- **The file records no per-part hashes** (written before this plan):
  it is in no shape, and is deleted as today. That costs one last full
  rebuild on the first build with this plan, and none after.

The pass then reads, per source, only for the parts with no cursor. A
contacts part with no cursor reads every source's contact tables
whole, through Part 2's paged reader, and writes nothing else. The
keyword pass already reads a source whole when `read_at` is `None`.

### Questions to settle first

- **Is dropping a part's tables fast?** *Measured on a synthetic file
  (2026-11-26), not yet on a real library: dropping 1.2 GB of FTS5 and
  section tables, 288k pages of a 2.75 GB WAL file with incremental
  auto-vacuum, took 9.5 s from a cold cache and grew the WAL by 4.6 MB.
  The keyword part stays in the grid index's file.* The file is deleted today
  because dropping its tables one by one, then giving their pages back
  to the disk, took eleven minutes on a 5.8 GB index. Measure
  `DROP TABLE` alone, leaving the freed pages in the file for the refill
  to reuse, on a clone. If the keyword part is still slow to drop, give
  it a file of its own, deleted whole like today's index, and reach it
  from the grid's queries with `ATTACH`. Check why the two share one
  file ([`search_index/README.md`](../../../datalib/backend/search_index/README.md)
  § "Two files, two writers") before choosing.
- **What reads across parts.** *Settled as built: the grid's keyword
  filter (`text_clause`) reads the keyword tables, so emptying them raises
  the grid's generation; the names people are known by are written again
  by any pass that seals (`refresh_people`), which a contacts fill does;
  and emptying the grid empties the contacts, which ride on its
  documents.* "The names people are known by" are
  written from authors (grid) and contacts together; a move of either
  part redoes them, at about a second. List every statement that joins
  two parts, and decide the same for each.
- **What a reader waits for.** A reader requires the whole `shape()`
  today, and waits while a build has dropped the search indexes. With
  parts, the applet's grid queries need the grid part, keyword search
  the keywords part, and a person's chip the contacts part. Each should
  wait only for the part it reads.

## Part 2: one paged writer with bulk statements

### Reading in pages

`documents_matching` takes a page: the first `n` documents after a key
on a whole read (keyset pagination by `markdown_uuid`, as
`RenderUnitPages` does), or the next `n` of the changed set on an
incremental one. Each table's read binds that page's uuids, never the
whole source's, and reads only the tables of the parts being filled.

Working out which documents a source dropped needs every uuid the
store holds (`removed` in `read_source` is "indexed minus present").
The pages keep just those uuids, about 10 MB for 135k documents, and
work out `removed` once the last page is read.

### Writing a page

`apply_page(write_lock, &[RenderedMarkdown])` replaces the body of
`apply_markdown`; `apply_one` becomes a page of one, so the applet's
and the tests' callers keep working.

| Today, per document or per row | Per page |
|---|---|
| `misfiled`, `first_repeated`, `document_row` | The same pure checks, run over the page first. A uuid repeated across two documents of the page is caught here too |
| `delete_rows_document_dropped`, per document | One `DELETE FROM grid_rows WHERE markdown_uuid IN (…) AND uuid NOT IN (…) RETURNING uuid`, both sets bound as JSON arrays |
| `DELETE` from `edges`, `data_nodes`, `filings`, contacts and `source_photos`, per document | One `DELETE … WHERE … IN (SELECT value FROM json_each(?))` per table (`Store::delete_contacts` already takes an array). `data_nodes`' `RETURNING uuid` still feeds `touch_nodes` |
| `put_grid_row`'s upsert, then `find_grid_row_owner` when it wrote nothing | One `SELECT uuid, markdown_uuid, provider FROM grid_rows WHERE uuid IN (page's uuids) AND markdown_uuid NOT IN (page's documents)`. Rust decides "moved" or "two documents minted one id" from it exactly as `put_grid_row` does now (`written_this_run`), and one `DELETE` releases the moved rows |
| One upsert per grid row | Multi-row `INSERT … ON CONFLICT (uuid) DO UPDATE … WHERE <differs> RETURNING uuid`. The rows it returns are the ones that changed, which is what `note_changed` collects today |
| `insert_edge`, `insert_node`, `insert_filing`, `insert_source_photo`, `upsert_markdown`, per row | Multi-row inserts, chunked by `bulk::SQLITE_MAX_VARIABLES` over the table's column count |
| `contact_records::record_id`: look the record up by its JSON, insert it if it is missing, then its handles and numbers | `contact_json` is already `UNIQUE`: one multi-row `INSERT … ON CONFLICT (contact_json) DO NOTHING` for the page's distinct records, one `SELECT record_id, contact_json … WHERE contact_json IN (…)` for their ids, then multi-row inserts of handles, numbers and `document_contacts` |

SQLite does not promise the order of the rows `RETURNING` gives back
from a multi-row insert. Every `RETURNING` above is read as a set, or
carries its own key (a uuid, a `contact_json`) to map it by, never by
position.

The render store's writer (`put_document`) gets a `put_documents` that
calls `apply_page` and writes the page's sections the same way. A
render already works in pages, so this is the page it already has.

### The keyword index

`Index::put` already commits in byte-sized batches (`PUT_BYTES`). Each
batch becomes:

- one `SELECT doc_id, markdown_uuid, source_id, content_hash FROM
  search_documents WHERE markdown_uuid IN (…)`, to sort the batch into
  unchanged, changed and new;
- one `clear` of the changed documents' sections;
- multi-row inserts of the new documents and updates of the changed
  ones, `RETURNING doc_id, markdown_uuid`;
- multi-row inserts of `search_sections`, `RETURNING section_id,
  doc_id, seq`, mapped back by `(doc_id, seq)`;
- multi-row inserts of `search_fts`, each row's `rowid` the section's
  id.

## Order

1. **The keywords as their own part.** It already has its own
   per-source cursor, so this is the shape split, the drop-and-refill on
   open, and the reader's wait. It keeps keyword search through every
   grid change and the grid through every keyword change.
2. **The paged reader and the bulk writer** (Part 2), for the grid and
   the keywords.
3. **The contacts as their own part**, with a per-source cursor of
   their own, filled through the paged reader limited to the contact
   tables. This is the step that turns an upgrade like #2019 into
   seconds.

## How we know it worked

**A part refilled alone equals a part built with the rest.** For each
part, a test builds an index, records the part's tables, drops the part
by changing its hash, runs the pass, and compares the part's tables
with `datalib_store_dump`. A second assertion checks that the other
parts' tables were not written at all (their rows and the file's other
tables are byte-for-byte what they were). The hash change comes from
the production `shape()` with one part's DDL edited, not from a
hand-built shape.

**The same index whether pages hold one document or many.** A test
applies a set of documents a page at a time and again one at a time
through `apply_one` (a page of one, so production code on both sides),
and compares the two stores. The same test runs with a page that moves
a row from another document, a page whose two documents claim one uuid
(which must fail, naming both), and a page applied over an index that
already holds older versions of its documents. The moved-row and
collision tests already in `grid_index.rs` keep passing unchanged.

**The same change sets.** `note_changed`, `note_gone` and
`touch_nodes` drive the nodes, terms and search work after a load. A
test applies a re-rendered page and asserts the three sets equal what
the per-row writer collects for the same documents.

**Measured on a real library.** On a `cp -c` clone, before and after:

- change one contacts table's DDL and run the step: expect it to read
  only the contact tables and take well under a minute, against 10m43s
  today;
- delete the grid index and run the step: compare the "sealed the
  index" lines' `read_ms`, `load_ms` and `seal_ms`, the keyword
  phase's time, the step's `rss_peak_bytes`, and the `sql_runs_total`
  of `etl.render.grid.*` and `search_index.store.*`. Expect the load's
  4.87M statements to drop to the tens of thousands, memory to stop
  growing with the largest source, and perhaps 1.5 to 2.5 minutes off
  the 10m43s. The measurement decides.

## Not in this plan

- **Reading each render store once.** The grid pass and the keyword
  pass each read every store; the second read of the section text cost
  113 s here. Feeding both from one paged read is a natural next step
  once reads are paged.
- **The 95 s of index building after a load into an empty grid part.**
  It is one `CREATE INDEX` per index over 1.23M rows, already the cheap
  way to build them.
- **A column added to `grid_rows`.** It changes the grid part and still
  rebuilds the grid. An added column could be an `ALTER TABLE` and a
  refill of one column instead; that is a narrower case, worth a plan
  of its own if column additions keep coming.
