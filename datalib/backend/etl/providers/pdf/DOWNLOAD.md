# pdf — download

Scans a local directory tree for `*.pdf`, hashes each file, classifies
it, and records what it is. Conversion to markdown is the **render**
step's job; this side never produces text.

This document covers what's load-bearing and provider-specific. For the
framework contracts every provider honors — schema-first, bulk-upsert
chokepoints, commit lifecycle, `--reset-and-redownload` semantics — see
[`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md).
For the row shapes and the identity argument, see
[`src/download/schema_raw.rs`](src/download/schema_raw.rs).

## Relationship to `fsindex`

Both providers scan local trees, and they share the primitives that make
that fast and correct: blake3 leaf hashing and Unison's
`(mtime, size, inode, dev)` rescan cursor, both in
[`datalib_etl::fswalk`](/datalib/backend/etl/src/fswalk.rs). That module
was factored out of fsindex when this provider needed the second copy;
fsindex's `hash.rs` and `stamp.rs` are now thin adapters over it.

They are separate **sources** because they answer different questions:

| | `fsindex` | `pdf` |
|---|---|---|
| Question | "what is in this tree?" | "what documents do I have?" |
| Scale | tens of millions of entries | thousands of documents |
| Keyed on | path (both tables) | content hash; paths hang off it |
| Directories | tree-hashed into a Merkle structure | not modelled |
| Render side | none | markdown + `grid_rows` |
| Per-item cost | one `stat`, sometimes one `read` | a parse and a conversion |

The last row is why the retry story differs. fsindex's `DOWNLOAD.md`
says, correctly for it, "No retry semantics for transient failures — a
`read(2)` either succeeds or it's a real error." Here a document can
fail for reasons that are worth retrying (a file caught mid-write, a
half-synced Dropbox placeholder), so an unidentifiable file is re-read
on the next scan rather than cached as permanently broken. That
behavior is pinned by `rescan_reuses_hashes_and_is_idempotent` in
`tests/pdf_e2e.rs`, which asserts that exactly the corrupt fixture is
re-read on an otherwise-unchanged rescan.

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

The seam for that work is `render::convert::RENDER_VERSION`, which
participates in the render cache key: bumping it re-renders every
affected document with no migration.

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
key stays `blake3`. `download/content_hash.rs` has the full account of
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

- **Browser print chrome is only partly removed.** `render::convert`
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

## `source_url` is absolute, and that has consequences

`grid_rows.source_url` holds an absolute `file://` URL, because that is
what the UI needs to reveal a document in the platform file manager.
Two things follow, both deliberate:

- **PDF grid rows are machine-specific.** The backend index is a derived
  artifact — rebuilt from the `*.grid_rows.json` sidecars by
  `grid_index` — so this does not corrupt anything shared. But it does
  mean two machines indexing the same corpus produce different rows for
  the same document.
- **Moving the corpus re-renders it.** `markdowns.row_set_hash` is a
  hash over the grid rows, so a changed path changes the hash and the
  document re-renders. That is arguably correct (the URL really did
  change) and cheap at ~6 ms/page, but it is worth knowing before
  relocating a large tree.

The same property makes `row_set_hash` unstable across machines for
this provider, which is why `fixture_db_snapshot` redacts it for `pdf`
rows — see `stable_row_set_hash` there for why that costs almost no
coverage. CI caught this after a first fix that normalized only the
*displayed* `source_url` and left the hash derived from the real one.

## Orphaned documents

`pdf_paths` is truncated and rebuilt every scan, so a deleted file
disappears on its own. `pdf_documents` is **not** truncated — it is
keyed on content, which has no notion of "no longer present," and
dropping it would lose `first_seen_at` and force a re-convert of every
document whose path merely moved.

The consequence is that deleting the last copy of a document leaves an
unreferenced `pdf_documents` row. That is deliberate for now: the row is
cheap, it preserves the record that the document was once here, and the
render side ignores it (its join against `pdf_paths` finds nothing).
Reaping them is a `DELETE … WHERE blake3 NOT IN (SELECT blake3 FROM
pdf_paths)` whenever we decide we want it — but note that doing so
discards history a `dolt_diff` would otherwise still show.

## Inspecting a scan

```sh
bazelisk build //third-party/doltlite:doltlite
dl=bazel-bin/third-party/doltlite/doltlite
db=<root>/pdfs/raw/entities.doltlite_db

# How much of the corpus is out of reach without OCR?
$dl $db "SELECT pdf_type, needs_ocr, COUNT(*) FROM pdf_documents
         GROUP BY pdf_type, needs_ocr;"

# Duplicates: one document, many locations.
$dl $db "SELECT blake3, COUNT(*) c, GROUP_CONCAT(id) FROM pdf_paths
         GROUP BY blake3 HAVING c > 1;"

# Ship of Theseus: every revision of one conceptual document.
$dl $db "SELECT blake3, title, doc_modified_at
           FROM pdf_documents
          WHERE content_blake3 = (SELECT content_blake3 FROM pdf_documents
                                   WHERE blake3 = '…')
          ORDER BY doc_modified_at;"

# Which documents in the corpus are metadata-only variants of each other?
$dl $db "SELECT content_blake3, COUNT(*) c, GROUP_CONCAT(title) FROM pdf_documents
         GROUP BY content_blake3 HAVING c > 1;"

# The producer-supplied lineage, when the file happens to carry it.
$dl $db "SELECT blake3, title, doc_modified_at, xmp_instance_id
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

The corpus deliberately includes a byte-identical duplicate pair, a
same-`DocumentID` revision, a metadata-free document, an image-only
page, a truncated file, and a non-PDF — one per behavior the e2e test
asserts.
