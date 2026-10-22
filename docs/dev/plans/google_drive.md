# Google Docs, Sheets and Slides as a source, with their history

**Status: proposal (2026-10-21), with phases 1 and 2 built, and phase 4
built for Docs.** The `google_drive`
provider mirrors a Google account's Docs, Sheets and Slides into a raw
store. The store keeps Google's own native representation of each file at
every version a run saw, plus the comments and the revision list, and
OpenDocument exports when the config asks for them.
[`providers/google_drive/INGEST.md`](../../../datalib/backend/etl/providers/google_drive/INGEST.md)
is the reference for what is built. This page is why it is shaped that way,
and what is still to come. The facts under "What Google offers" were
measured on 2026-10-21 against one Workspace account holding 381 native
files (297 Docs, 43 Slides, 41 Sheets).

## What the backup is for

A backup copy of each file, good enough for two things later:

1. **Putting it back into Google**, as a new file that looks like the old
   one.
2. **Opening it without Google**, in LibreOffice or OpenOffice, ideally
   with its edit history and its comments.

Neither of those is built yet. The raw store is shaped so that both can be
built from it without asking Google again.

## What Google offers

**The listing.** `drive/v3/files` with `fields=files(*)` returns each
file's full metadata, up to 1,000 per page. Every file has a `version`, a
number that goes up on every change to the file. Some fields change
between two reads even when nothing was edited:

- `thumbnailLink` is a freshly signed URL on every read (100 of 100 files);
- `permissionIds`, and probably `permissions` and `owners`, come back in a
  different order;
- `photoLink`, a person's avatar URL, changed on 1 file in 100.

**The native representation.** Each kind has its own API, and each returns
the whole file as JSON:

| kind | request | sample size | gzip -9 |
|---|---|---|---|
| Doc | `docs/v1/documents/{id}?includeTabsContent=true&suggestionsViewMode=SUGGESTIONS_INLINE` | 99 KB | 9 KB |
| Slides | `slides/v1/presentations/{id}` | 1.2 MB | 44 KB |
| Sheet | `sheets/v4/spreadsheets/{id}?includeGridData=true` | 6.5 MB | 44 KB |

- **Stable across reads, except the image links.** Two reads of an
  unedited file minutes apart return the same bytes, for all three
  kinds. Two hours apart, every image link had been signed afresh and
  nothing else had changed (one Doc, one deck, 19 links); the images
  themselves came back byte-identical.
- **Big but highly compressible.** The Sheet above has 253 cells with a
  value, yet `effectiveFormat` (the formatting Google computed) is on
  6,158 of its 10,143 cells. Every kind compresses enormously.
- **Revision ids.** Docs and Slides carry a `revisionId`, an opaque token
  that is not the Drive revision number; Sheets carry none.
- **Images** are `contentUri` / `contentUrl` links on `docs.google.com`.
  They are signed, last about 30 minutes, and download with plain `curl`,
  no credential.
- **Head only.** All three APIs read only the current version. There is no
  way to ask for the native form of an older one.

**Comments.** `drive/v3/files/{id}/comments?includeDeleted=true&fields=*`
returns every comment with its replies inline, resolved and deleted ones
included. That is the only complete record: on a doc with 3 comments and
10 replies, the DOCX export held 1 comment and the ODT export 2.

**Revisions.**
- **The list.** `drive/v3/files/{id}/revisions` gives each revision's
  number, time and author. It is empty when the account may only comment
  (`capabilities.canReadRevisions = false`).
- **The list is not stable.** The first listing after a quiet spell can
  leave out a file's oldest revision, and a listing made right after it
  has it. Measured on 2026-10-21 on one deck with 10 revisions:
  - six listings came back with 9, without revision `1`, each the first
    in 2 minutes or more;
  - every listing made a second later had all 10;
  - so did every listing made within about 3 minutes of another;
  - the short and full responses had identical headers.

  A capture therefore lists twice, keeps every revision either listing
  named, and counts the files where they disagreed
  (`revision_listings_disagreed` in its `sync_runs` summary).
- **An old revision's contents** come only from its `exportLinks`, which
  are `docs.google.com/feeds/download/...?revision=N&exportFormat=...`
  URLs.
- **latchkey signs those links from 3.19.2** (imbue-ai/latchkey#181);
  before it, `latchkey curl` answered "No service matches URL" for any
  `docs.google.com` address. A link answers `307` with no body, and the
  `Location` is a signed `googleusercontent.com` download that needs no
  credential.
- **A Sheet's links spell ODS `application/x-vnd.oasis.opendocument.spreadsheet`**,
  where `files.export` takes it without the `x-`.
- **A deck's links ignore `revision=`.** Revision 1 and the newest
  revision of one deck exported identical ODP, PPTX and PDF; so did
  five revisions between them. A Doc's and a Sheet's links honour it.
- **Twice in one run, a deck's signed download answered `400`** with an
  HTML page; the next run fetched both. It did not happen again when
  tried alone or four at once.
- **No Drive API route works instead.** `files/{id}/export` ignores a
  `revision=` parameter and returns the current version, and
  `revisions/{n}?alt=media` is a 404 for a native file.

**Exports.**
- **Formats.** `drive/v3/files/{id}/export?mimeType=...` converts the
  current version to ODF (`.odt`, `.ods`, `.odp`), OOXML (`.docx`,
  `.xlsx`, `.pptx`), PDF, HTML, Markdown (Docs only) and more.
- **What they carry.** ODT and DOCX both carry the open comments, and the
  suggestions as tracked changes (`text:changed-region`, `w:ins`).
- **Embedded fonts.** Both carry the fonts too, so an ODT of a one-line
  doc is 3 MB and a DOCX 1 MB.
- **Not byte-stable.** Two exports of one version differ in their zip
  timestamps and in one id in `settings.xml`.
- **Size limit.** Google caps an export at 10 MB.

**Credentials.** latchkey has four built-in services for this. All four
match `drive/v3/files`, and each signs its own API host too:

| service | scope it asks for | also signs |
|---|---|---|
| `google-drive` | `drive` | `drive/`, `upload/drive/` |
| `google-docs` | `documents`, `drive.readonly` | `docs.googleapis.com` |
| `google-sheets` | `spreadsheets`, `drive.readonly` | `sheets.googleapis.com` |
| `google-slides` | `presentations`, `drive.readonly` | `slides.googleapis.com` |

So the native JSON needs all four sign-ins.

## How the shape follows

- **The native JSON is the primary record.** It is what Google itself
  holds, the only form with every suggestion, named range, chart and
  layout, and the closest thing to a faithful republish. Exports are
  derived copies kept alongside.
- **The native JSON goes in the CAS, gzipped, not in a doltlite row.**
  This is a deliberate exception to "the payload is JSONB in the entity
  table".
  - *Measured:* doltlite stores a large value whole. A 5 MB JSONB
    payload grew the file by 5.1 MB when one cell changed, and by 1.5 KB
    when it changed back to bytes it had seen.
  - *What it would cost:* a Sheet edited all day and synced hourly would
    then cost 5 MB per sync in doltlite, against about 44 KB gzipped in
    the CAS.
  - *Why the CAS can hold it:* with its image links taken out (into the
    capture's sidecar, as volatile fields go), the JSON of an unedited
    file is the same on every read. gzip with a zero mtime is
    deterministic, so an unchanged file maps to an unchanged blob.
- **Each version a run captures is its own row** (`captures`), never just
  a new commit of one row. It does not lean on doltlite history to hold
  the versions, so the history survives whatever is done to the commit
  log.
- **Nothing is re-exported for a version already held.** Exports are not
  byte-stable, so a second export of an unchanged file would add 3 MB of
  identical-looking bytes. `files.version` is the held version; a file is
  owed only when its `version` moves.
- **Comments and revisions are rows of their own**, keyed by the file and
  their own id. They are upserted on each capture and never deleted, so a
  revision Google later merges away, or a comment it later drops, stays
  in the backup.

## Getting to the two goals

**Republishing to Google.**
- **Upload the ODF or OOXML export** with conversion: `files.create` with
  the native `mimeType` and the export as the body. Google's own importer
  rebuilds the file, the tracked changes as suggestions and the open
  comments as comments. That is the high-fidelity route available today.
- **Re-add what the export lost from the `comments` table** with
  `comments.create`: the resolved threads, the replies. They will be
  authored by whoever republishes, so the text has to carry the original
  author and time.
- **Replay the native JSON** through `documents.batchUpdate` /
  `spreadsheets.batchUpdate` / `presentations.batchUpdate`. That is
  possible, and the most faithful for Sheets, where `CellData` goes
  straight back through `updateCells`. But it is a translator per kind,
  and a large one for Docs. It is worth doing for Sheets first.
- **Revision history cannot be recreated.** Google offers no way to write
  it.

**Opening it in LibreOffice, with history.**
- **One package for every version.** ODF packages can hold earlier
  versions of themselves: LibreOffice's File > Versions stores each one
  under `Versions/` in the package, listed in `VersionList.xml`. An
  `export` step could assemble one `.odt` / `.ods` / `.odp` per file whose
  current content is the newest capture and whose versions are the
  earlier ones, each with its author and time from the revision list.
- **Comments the export dropped** can go in as `office:annotation`s.
  Where Google's anchor (`kix.…`) cannot be placed, they go at the end.
- **Where it belongs.** That step reads the raw store and nothing else,
  so it is a render-side consumer, not a change to the download.

## Making ODF from the native JSON ourselves

**Nothing does it off the shelf** (searched 2026-10-21). The nearest
tools stop short:

- **Docs JSON to Markdown.** Converters exist (`gconv`,
  `parse-google-docs-json`, `docs-markdown`), but they keep the text and
  the structure, not the formatting, suggestions or comments.
- **ODF packaging.** `odf-kit` (TypeScript, Apache-2.0) builds ODT from
  HTML, Markdown or its own model, so it could do the packaging, but it
  does not read Google's JSON.
- **LibreOffice headless** converts between office formats, but it does
  not read Google's JSON either.

So a translator is ours to write, one per kind. It would read the raw
store and nothing else, so it belongs on the render side. And it can make
ODF for any version ever captured, which Drive's export cannot once an
old revision is gone.

**Sheets: closest to one-to-one.**
- **What carries straight over.** A `CellData` has the value as entered
  (`userEnteredValue`, its formula included), its format, and the value
  Google computed. Merges, column widths and frozen panes are on the
  sheet.
- **A writer exists.** The Rust `spreadsheet-ods` crate (MIT or
  Apache-2.0, 1.0.4) writes ODS.
- **What does not.** Formulas need rewriting into OpenFormula
  (`=SUM(A1:A3)` becomes `of:=SUM([.A1:.A3])`). Functions only Sheets
  has (`GOOGLEFINANCE`, `IMPORTRANGE`, `QUERY`) have no equivalent, and
  would keep Google's computed value as the cell's cached result.

**Docs: medium.**
- **The content.** Paragraphs, styled runs, lists, tables, inline
  images, headers, footers and footnotes map onto ODF text, written as
  XML directly; no Rust ODT writer worth depending on turned up.
- **Suggestions** become `text:tracked-changes`.
- **Comments** come from the `comments` table as `office:annotation`s.
  Their anchors are opaque (`kix.…`), so they are placed by the comment's
  `quotedFileContent` where that text is found, and at the end otherwise.

**Slides: hardest, so last.** Every element is positioned in EMUs on a
page that inherits from a layout and a master, and the ODP drawing model
differs in the details.

**Exports are off by default.** The native JSON is the backup, and a
renderer for it is coming, so Drive's exports are something a config
asks for. They can be set per kind. ODS is small (0.5 MB for a Sheet
measured), while ODT and ODP carry embedded fonts (3–7 MB measured).
Once a translator for a kind exists, that kind's export becomes a
cross-check at most.

## Phases

1. **Built.** One sign-in per service.
   - *Listing:* every native file in each kind, unless `file_ids` names
     some.
   - *Capture:* for each file whose `version` moved, its metadata, its
     revision list, its comments, its native JSON (gzipped, in the CAS),
     each image the JSON points at, and any exports the config asks
     for (none by default, settable per kind).
   - *Testing:* playback-tested against TNG fixtures, and against the
     live account by hand.
2. **Built: old revisions.** latchkey signs `docs.google.com`'s export
   links from 3.19.2 (imbue-ai/latchkey#181). Every listed revision of
   a Doc or a Sheet is owed an export of its own when
   `revision_exports` names a format (none by default), so the history
   from before the first sync comes back in export form. Native JSON for those versions does not exist
   anywhere, so the native history starts at the first capture, and a
   deck's links ignore the revision, so a deck's starts there too.
3. **Delta listing.** Drive's `changes` API, with its `startPageToken`,
   in place of the full listing each run. The full listing is 1 request
   per 1,000 files, so this matters only for a large Drive.
4. **A markdown render.** Built for Docs
   ([`google_drive_render/TRANSLATE.md`](../../../datalib/backend/etl/providers/google_drive_render/TRANSLATE.md));
   Sheets and Slides are still to do. As first planned: the first render, because it is what the rest
   of datalib reads. A `google_drive_render` crate and a
   `render_markdown` step would make each file one document, from its
   newest capture's native JSON:
   - *Docs:* headings, lists, tables and images as markdown;
   - *Sheets:* each sheet as a table;
   - *Slides:* each slide's text.

   Its comment threads would be sections of their own, with the
   `m-{uuid}` wrappers feedback anchors to. That puts the files in the
   grid and in search like any other source. Deferred for now: phase 1 is
   the backup.
5. **ODF from the native JSON**, Sheets first (above), then the
   ODF-with-versions export step built on it.
6. **A republish command**, export upload first, Sheets replay second.
7. **The wizard.** One catalog entry has one `credentialService`, and
   this source needs four. Either latchkey grows one service covering all
   four scopes, or the catalog learns several.

Not planned: other Drive files (PDFs, images, uploaded Office files).
Their bytes are a plain `alt=media` download with a real
`headRevisionId` and `md5Checksum`, so they would be a natural next
kind, but they are not what this source is for yet.
