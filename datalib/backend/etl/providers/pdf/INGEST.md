# pdf — download

Scans a local directory tree for `*.pdf`, hashes each file, classifies
it, and records what it is. Conversion to markdown is the **render**
step's job; this side never produces text.

Row shapes and the identity argument are in
[`src/ingest/schema_raw.rs`](src/ingest/schema_raw.rs); the contracts
every provider honors are in
[`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md).

## Relationship to `fsindex` and `media`

The three providers that scan a local tree, how they share the walk and
why they are separate sources:
[`../media/INGEST.md`](../media/INGEST.md) §"Relationship to `fsindex`
and `pdf`".

One difference matters here. A `read(2)` either succeeds for `fsindex` or
is a real error, but a document can fail for reasons worth retrying (a
file caught mid-write, a half-synced Dropbox placeholder). So a file
that could not be identified is identified again on the next scan
rather than cached as permanently broken, even though the host
fingerprint cache spares re-hashing it. Pinned by
`rescan_reuses_hashes_and_is_idempotent` in `tests/pdf_e2e.rs`: a
rescan hashes nothing and still retries the corrupt fixture.

A scan truncates `pdf_paths` once the walk is done and rebuilds it, so
deletions fall out; content already in `pdf_documents` is not
identified again.

## When part of a scan fails

The scan goes on, and what it could not do is a `problems` row:

- **An entry the walk could not read** (a folder it may not list, a
  file that would not hash, a dangling link) means a path the walk did
  not see may only be one it could not see, so **that scan does not
  truncate `pdf_paths`**: what it saw is upserted over what was there
  and nothing falls out. It leaves a `listing:files` row; the next
  clean walk truncates as usual and clears it
  (`a_walk_with_errors_drops_no_path`).
- **A document that would not identify** is
  `record:pdf_paths:<path>`, with what the parser said. It is retried
  every scan (above), and the scan that identifies it clears the row; a
  scan that cannot see it (under an entry its walk could not read) keeps
  it.
  The fixture's `holodeck/corrupt.pdf` is one, on purpose. No grid row
  carries it: a document that never identified never renders.

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
`fixture_db_snapshot` (in `unified_index`'s tests) normalizes
`source_url` through `stable_source_url`.

## Orphaned documents

`pdf_paths` is truncated and rebuilt every scan whose walk read the
whole tree, so a deleted file disappears on its own. `pdf_documents` is **not** truncated — it is
keyed on content, which has no notion of "no longer present," and
dropping it would lose when the document was first seen
(`pdf_documents_bookkeeping.fetched_at_utc`) and force a re-convert of
every document whose path merely moved.

The consequence is that deleting the last copy of a document leaves an
unreferenced `pdf_documents` row, deliberately: the row is cheap, it preserves the record that the document was once here, and the
render side ignores it (its join against `pdf_paths` finds nothing).
Reaping them is a `DELETE … WHERE blake3 NOT IN (SELECT blake3 FROM
pdf_paths)` whenever we decide we want it. The rows stay in earlier
commits, but HEAD stops recording that the document was once here.

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
