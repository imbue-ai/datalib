# pdf — download

Lists a local directory tree for `*.pdf`, hashes each file as it reads it, classifies
it, and records what it is. Conversion to markdown is the **render**
step's job; this side never produces text.

Row shapes and the identity argument are in
[`src/ingest/schema_raw.rs`](src/ingest/schema_raw.rs); the contracts
every provider honors are in
[`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md).

## Relationship to `file_inventory` and `media`

The three providers that scan a local tree, how they share the walk and
why they are separate sources:
[`../media/INGEST.md`](../media/INGEST.md) §"Relationship to `file_inventory`
and `pdf`".

One difference matters here. A `read(2)` either succeeds for `file_inventory` or
is a real error, but a document can fail for reasons worth retrying (a
file caught mid-write, a half-synced Dropbox placeholder). So a file
that could not be identified is identified again on the next scan
rather than cached as permanently broken, even though the host
fingerprint cache spares re-hashing it. Pinned by
`rescan_reuses_hashes_and_is_idempotent` in `tests/pdf_e2e.rs`: a
rescan hashes nothing and still retries the corrupt fixture.

A scan truncates `pdf_paths` once the walk is done and rebuilds it, so
deletions fall out; content already in `pdf_documents` is not
identified again. A file over `max_bytes` is there and not read, so it
keeps the path row the last scan wrote. Once the paths are rebuilt
after a clean walk, a document no path names goes with its bookkeeping
(`documents_removed`); a file moved within the tree is named at its new
path by then and keeps its document.

A row's `_bookkeeping` sidecar is stamped the first time a scan writes
the row and left alone after; a path's survives the truncate and goes
when the path does (`RawDb::prune_unnamed`). So scanning an unchanged
tree again commits nothing
(`a_second_scan_of_an_unchanged_tree_commits_nothing`), the corrupt
fixture's `problems` row included: a problem recorded again unchanged
keeps its stamps.

## When part of a scan fails

The scan goes on, and what it could not do is a `problems` row:

- **An entry the walk could not read** (a folder it may not list, a
  dangling link) means a path the walk did
  not see may only be one it could not see, so **that scan does not
  truncate `pdf_paths`**: what it saw is upserted over what was there
  and nothing falls out. It leaves a `listing:files` row; the next
  clean walk truncates as usual and clears it
  (`a_walk_with_errors_drops_no_path`).
- **A file that would not open** is `record:files:<path>`. It is
  there, so it keeps its path row and holds back no other deletion; the
  row goes the scan it opens.
- **A document that would not identify** is
  `record:pdf_paths:<path>`, with what the parser said. It is retried
  every scan (above), and the scan that identifies it clears the row; a
  scan that cannot see it (under an entry its walk could not read) keeps
  it.
  The fixture's `holodeck/corrupt.pdf` is one, on purpose. No grid row
  carries it: a document that never identified never renders. A file refused
  for what it would cost to read is one too (§"A PDF can be made by anyone").
- **A file that changed after its hash was taken.** The scan's hash only
  decides whether to look; a document read is named by the hash of the
  bytes it was classified from
  (`a_document_the_scan_misjudged_is_named_by_what_was_read`). Render
  converts only bytes that hash to the document. A file that no longer
  does renders as a stand-in: its document row, a page saying its pages
  are missing, and a `problems` row, until the next sync reads it
  (`a_document_whose_file_changed_since_the_download_is_a_stand_in`).
  A cold render produces the same stand-in, so nothing depends on what
  an earlier run left behind.

## A PDF can be made by anyone

A person may be handed a PDF, and two kilobytes of one can ask a parser for
gigabytes. `max_bytes` says nothing about that: it caps the file on disk, and a
stream inflates a thousand times, or a million behind two layers of Flate.
`pdf-inspector` limits what it loads and a page's content, but it takes bytes
and no limit and inflates fonts, CMaps and forms without one. So
[`src/ingest/expansion.rs`](src/ingest/expansion.rs) measures a file before
either parser reads it: `identify` here and `convert` in render, which also
catches a document identified by an earlier build. Past a limit, the file is
refused and is a `record:pdf_paths:<path>` problem saying why; the rest of the
tree syncs. It is measured again each scan, like any file that did not identify.

| what is measured | limit | why that much |
|---|---|---|
| one stream, decoded | 1 GiB | A stream is held once or twice while it is read. Page content and fonts are far smaller. |
| every stream together | 64 GiB | Bounds the time measuring takes. |
| the objects in the object streams a loader expands | 64,000,000 | About 70 bytes each in memory, and two bytes of stream can be one. |
| an object stream's index | no entry overlaps the next | Otherwise a loader reads what they share again for each entry: 2 KiB of file, one 1 MiB array, gigabytes held. |

Measured on a sync (`rss_peak_bytes`): the most a file inside these limits made
a step hold was 2.1 GiB for a stream just under 1 GiB, and 3.6 GiB for 50 million
objects.

The measuring itself costs little: one layer of Flate is counted as it comes out
and never held, so a real scan's big image costs what it did when nothing inflated
it. Every other chain of filters is held once, up to the limit.

**A Flate image past 1 GiB is refused too**, though neither parser would ever
inflate it. Its dictionary calling it an image proves nothing: a stream that says
`/Subtype /Image` and is named as a font's program gets inflated as one. Such an
image is a colour sheet about 80 cm square at 600 dpi, with no text to index unless
the document has another page; a JPEG of any size costs nothing, since nothing
inflates it.

**An object stream past 8 MiB is left out, not refused**, as `pdf-inspector`
leaves it out: a tagged PDF's structure tree can reach hundreds of MB. Its
`content_blake3` is then `None`, since a hash over part of a document could agree
with a different one. This bound is `pdf-inspector`'s and it does not export it;
`the_bound_on_object_streams_is_the_one_pdf_inspector_loads_with` fails if it moves.

An object the cross-reference table lists and the loader cannot parse is measured
from its raw bytes, because `pdf-inspector` may repair it and read it after all
(`an_object_the_loader_drops_is_measured_from_its_bytes`).

### What this does not stop

The check has to load the file, so what `lopdf` does while loading is out of its
reach, and `lopdf` has no setting for these:

- **A chain of cross-reference streams.** Each `/Prev` adds its entries to one
  table, and `max_decompressed_size` caps each stream, not the sum. Measured: a
  63 KiB file held 6.2 GiB, a 250 KiB one 24 GiB.
- **An encrypted document's object streams**, which `lopdf` expands before any
  filter runs. Read from its source, not staged.
- **A file that is simply large.** Objects in the open cost tens of times what
  they take on disk, up to `max_bytes`.

Stopping the first two takes a cap in `lopdf`, or reading each new PDF in a child
process that can be killed, which would also catch whatever nobody has thought of.

## Why no OCR yet

A spike over a 21-PDF corpus (born-digital papers and forms, browser
print-to-PDF, and scans in seven scripts) produced the numbers behind
this decision:

- **Classification is reliable.** 21/21 correctly sorted; born-digital
  files came back `text_based` at confidence 1.0, scans at 0.95. That is
  what makes "record it and skip it" a safe default — nothing is
  silently dropped, and the work list for a future OCR pass is exactly
  `SELECT … WHERE needs_ocr = 1`.
- **Conversion is effectively free.** 277 pages in ~3.4 s, single
  threaded (~6 ms/page).
- **OCR's failure mode is silent, not loud.** With PP-OCRv6 Small (50
  languages: Latin + Chinese + Japanese), supported scripts round-trip
  at 100% / 99.7% character similarity against born-digital ground
  truth. Unsupported ones do not merely fail: Cyrillic scored **0.2%**
  similarity while reporting `ocr_confidence` **0.88** and
  `hosted_recommended: false` on every page, and Devanagari hallucinated
  CJK glyphs at 0.70 confidence. The engine's own quality signals do not
  catch it.

So OCR is deferred rather than half-built, and `PdfConfig::ocr = true`
is **rejected at load time** instead of being silently ignored — a
config that asks for OCR should fail loudly, not quietly index nothing.

When an engine does land, two guards belong with it, neither of which
the engine provides:

1. **A supported-script allowlist**, checked before routing a page.
2. **A letter-ratio floor** on the output. Across the spike corpus, good
   OCR ran 88–96% letters (of non-whitespace characters) and garbage ran
   0.4% and 26%; a floor around 60% separates them with enormous margin,
   and catches what `ocr_confidence` misses.

The seam for that work is `RENDER_VERSION` in
`../pdf_render/src/render/convert.rs`: bumping it re-renders every
document with no migration.

### `needs_ocr` is a work list, not a verdict on the document

`needs_ocr = 1` means *some* page of a document is unreadable. It does
not mean the document is skipped: selecting `WHERE needs_ocr = 0` would
render no `Mixed` document at all, since every one has an unreadable
page by definition.

What renders is decided per page:

```sql
WHERE has_encoding_issues = 0 AND page_count > ocr_page_count
```

— at least one page carries text we can extract. The pages we could not
read stay counted in `ocr_page_count`, and the render step writes a
`*Page N — no extractable text …*` note in their place, so the gap is
visible to whoever opens the document rather than only to whoever
queries the store. Those notes get no `grid_rows` row: the sentence is
identical on every unreadable page, so rows would add nothing to the
grid while costing a qmd embedding each.

`has_encoding_issues` is the one all-or-nothing case, and deliberately
so. A page whose fonts do not decode produces mojibake, which *looks*
like text — it would be indexed, searched, and shown as if it meant
something. An absent page is an honest gap; a garbled one is a lie, so
one such page suppresses the whole document — the opposite trade from
scanned pages, because the two failures are not comparable.

Note that the column is populated from the detector's per-page reasons
(`suspected_garbled_text`: an Identity-H font with no `ToUnicode`, or a
Type3-only page), **not** from pdf-inspector's own
`has_encoding_issues`. That field is always `false` for us: it is
computed from extracted markdown, and the download step runs
detect-only. Undecodable *fonts* are what we can see without paying for
a conversion; genuinely garbled *text* from a decodable font is not
caught here.

## What the metadata is actually worth

Measured over a 20-document real corpus (arXiv papers, IRS forms,
browser print-to-PDF, UN translations):

| Column | Populated | Notes |
|---|---|---|
| `title` | 15/20 | Usually good; a few are producer boilerplate. |
| `pdf_id_permanent` | 13/20 | Trailer `/ID[0]`. |
| `author` | 10/20 | See the caveat below. |
| `xmp_document_id` | 3/20 | The reason lineage is a hint, not a key. |
| `content_blake3` | every parseable file | Computed by us — see below. |

The last row is the one to reach for. `content_blake3` is a hash over
the document's *content* — every object reachable from the catalog,
with the Info dictionary, the XMP packet and the trailer `/ID` left
out — so retitling a PDF or letting a tool regenerate its `/ID` moves
`blake3` while `content_blake3` holds. Unlike the producer-supplied
columns above it is present for every file we can parse and cannot be
duplicated by `cp`, which makes it the column that actually answers
"every revision of this document".

It is still a hint. A writer that renumbers objects (Acrobat
"Save As", `qpdf --linearize`, Ghostscript) changes it even though
nothing visual moved, so it splits where it ideally would have merged.
That direction is deliberate — a false split costs a duplicate row,
where a false merge would hide a document — and it is why the primary
key stays `blake3`. `src/ingest/content_hash.rs` has the full account of
what survives and what does not.

**`author` is populated more often than it is meaningful.** Of the 10
values found: two were real author lists (arXiv papers), five were the
same producer username repeated across unrelated UN documents, and two
were IRS internal routing codes (`W:CAR:MP:FP`). We store what the file
says and do not try to filter the junk — the only way to tell a routing
code from a surname is a heuristic that will eventually discard a real
name, which is the same trade we declined for print-header stripping.
Treat the column as a hint, and expect to see noise in the grid.

Multi-author lists are stored in full but collapse to
`First Author et al.` in `grid_rows.author` — a 14-author paper produced
a 165-character string, which fits `VARCHAR(255)` only by luck and is
unreadable as a grid cell either way. The full value stays in
`pdf_documents.author` and in the markdown frontmatter.

## Known limitations

- **Browser print chrome is only partly removed.** `pdf_render`'s `render::convert`
  strips running heads/feet that repeat on their own line, but the
  extractor fuses roughly 80% of them into a body line instead
  (measured: 40 of 48 surviving instances across 4 print-to-PDF
  documents). Those stay in the text. See that module's docs for why
  a regex-based fix was rejected.
- **Floated layout scrambles reading order.** A Wikipedia infobox or a
  right-floated figure caption can interleave into the adjacent
  paragraph, because the extractor groups by Y-coordinate. Affects
  print-to-PDF far more than born-digital papers.
- **Dense forms convert poorly.** A fillable grid (a tax form) has no
  prose reading order to recover; the output is a scramble of field
  labels.
- **One garbled page costs the whole document.** `has_encoding_issues`
  is document-level, so a 200-page report with a single
  Identity-H-without-`ToUnicode` page renders nothing. Fixing that means
  recording *which* pages are garbled, not just how many — the render
  step would then drop those pages the way it notes scanned ones. Not
  done because the failure is usually document-wide anyway: font
  encoding is a property of the font, and a document generally uses one
  set throughout.

## `source_url` is absolute, and that has consequences

`grid_rows.source_url` holds an absolute `file://` URL, because that is
what the UI needs to reveal a document in the platform file manager.
Two things follow, both deliberate:

- **PDF grid rows are machine-specific.** The backend index is a derived
  artifact — rebuilt from the per-source render stores by
  `grid_index` — so this does not corrupt anything shared. But it does
  mean two machines indexing the same corpus produce different rows for
  the same document.
- **Moving the corpus rewrites its rows.** The path is in every grid
  row's `source_url`, so a changed path changes the row. That is
  arguably correct (the URL really did change) and cheap at ~6 ms/page,
  but it is worth knowing before relocating a large tree.

The same property makes the rows differ between machines, which is why
`fixture_db_snapshot` (in `unified_document_index`'s tests) normalizes
`source_url` through `stable_source_url`.

## Orphaned documents

`pdf_paths` is truncated and rebuilt every scan whose walk read the
whole tree, so a deleted file disappears on its own. `pdf_documents` is **not** truncated — it is
keyed on content, which has no notion of "no longer present," and
dropping it would lose when the document was first seen
(`pdf_documents_bookkeeping.fetched_at_utc`) and force a re-convert of
every document whose path merely moved.

Instead, after a clean walk, a document no `pdf_paths` row names is
deleted with its bookkeeping (`RawDb::prune_unnamed`,
`a_document_no_path_names_goes`). The rows stay in earlier commits, but
HEAD stops recording that the document was once here.

## Inspecting a scan

```sh
bazelisk build //third-party/doltlite:doltlite
dl=bazel-bin/third-party/doltlite/doltlite
db=<root>/pdfs/ingest/entities.doltlite_db

# How much of the corpus is out of reach without OCR?
$dl -readonly $db "SELECT pdf_type, needs_ocr, COUNT(*) FROM pdf_documents
         GROUP BY pdf_type, needs_ocr;"

# Pages, not documents: what an OCR engine would actually have to read,
# and how much we are already getting out of the same files.
$dl -readonly $db "SELECT SUM(ocr_page_count) unreadable,
                SUM(page_count - ocr_page_count) readable
           FROM pdf_documents;"

# Documents that render nothing at all, and why.
$dl -readonly $db "SELECT pdf_type, has_encoding_issues, COUNT(*) FROM pdf_documents
          WHERE has_encoding_issues = 1 OR page_count <= ocr_page_count
          GROUP BY pdf_type, has_encoding_issues;"

# Duplicates: one document, many locations.
$dl -readonly $db "SELECT blake3, COUNT(*) c, GROUP_CONCAT(id) FROM pdf_paths
         GROUP BY blake3 HAVING c > 1;"

# Ship of Theseus: every revision of one conceptual document.
$dl -readonly $db "SELECT blake3, title, doc_modified_at
           FROM pdf_documents
          WHERE content_blake3 = (SELECT content_blake3 FROM pdf_documents
                                   WHERE blake3 = '…')
          ORDER BY doc_modified_at;"

# Which documents in the corpus are metadata-only variants of each other?
$dl -readonly $db "SELECT content_blake3, COUNT(*) c, GROUP_CONCAT(title) FROM pdf_documents
         GROUP BY content_blake3 HAVING c > 1;"

# The producer-supplied lineage, when the file happens to carry it.
$dl -readonly $db "SELECT blake3, title, doc_modified_at, xmp_instance_id
           FROM pdf_documents
          WHERE xmp_document_id = 'uuid:…' ORDER BY doc_modified_at;"
```

## Fixtures

`holodeck/scanned_blueprint.pdf` and
`holodeck/scanned_blueprint_retitled.pdf` are the `content_blake3`
pair: identical page content, different `/Title` and different trailer
`/ID`. They are built on the *scanned* document on purpose — it is the
one fixture that never renders, so the pair costs the qmd indexer
nothing. A text-document pair would add a page to embed on every full
fixture build.

`tests/fixtures/pdf_tng/` is generated by
[`//tests/fixtures/make_pdf_fixtures.py`](/tests/fixtures/make_pdf_fixtures.py)
— hand-built PDF bytes rather than library output, so the files stay
reviewable and byte-deterministic (their blake3s are the provider's
primary keys, so drift there would churn every golden). The generator
lives under `tests/fixtures/` rather than beside this provider because
that is a Python lint root and a provider directory is not — same
reason `make_lightroom_catalog.py` sits there. Regenerate with:

```sh
uv run python tests/fixtures/make_pdf_fixtures.py
```

`engineering/hull_survey.pdf` is the `Mixed` case: one text page and
one image-only page, pinned by `a_mixed_document_renders_its_readable_pages`.
It costs one embedded page, the same as any one-page fixture: its
second page renders as a note, not as a row.

The corpus deliberately includes a byte-identical duplicate pair, a
same-`DocumentID` revision, a metadata-free document, a mixed
text-and-scan document, an image-only page, a truncated file, and a
non-PDF — one per behavior the e2e test asserts.
