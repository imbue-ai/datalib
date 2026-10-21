# Google Docs, Sheets and Slides as a source, with their history

**Status: proposal (2026-10-21), with phase 1 built.** The `google_drive`
provider mirrors a Google account's Docs, Sheets and Slides into a raw
store. The store keeps Google's own native representation of each file at
every version a run saw, plus the comments, the revision list and an
OpenDocument export.
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

- **Byte-identical across reads.** Two reads of an unedited file return
  the same bytes, for all three kinds.
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
- **An old revision's contents** come only from its `exportLinks`, which
  are `docs.google.com/feeds/download/...?revision=N&exportFormat=...`
  URLs.
- **latchkey refuses those links**: `latchkey curl` answers "No service
  matches URL" for any `docs.google.com` address.
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
  - *Why the CAS can hold it:* the JSON is byte-stable, and gzip with a
    zero mtime is deterministic, so an unchanged file maps to an
    unchanged blob.
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

**Until those exist**, Drive's export is the only copy LibreOffice can
open, so `exports` defaults to `odf`. It can be set per kind. ODS is
small (0.5 MB for a Sheet measured), while ODT and ODP carry embedded
fonts (3–7 MB measured), so keeping the Sheets' exports and dropping the
rest is a reasonable trade. Once a translator for a kind exists, that
kind's export becomes a cross-check rather than the backup.

## Phases

1. **Built.** One sign-in per service.
   - *Listing:* every native file in each kind, unless `file_ids` names
     some.
   - *Capture:* for each file whose `version` moved, its metadata, its
     revision list, its comments, its native JSON (gzipped, in the CAS),
     each image the JSON points at, and the configured exports
     (`odf` by default, settable per kind, `[]` for none).
   - *Testing:* playback-tested against TNG fixtures, and against the
     live account by hand.
2. **Old revisions.** Teach latchkey's Google services to sign
   `docs.google.com`'s export URLs (`/feeds/download/`,
   `/{document,spreadsheets,presentation}/export`), upstream in latchkey.
   Then every listed revision is owed an export of its own, and the
   history before the first sync comes back in export form. Native JSON
   for those versions does not exist anywhere, so the native history
   starts at the first capture.
3. **Delta listing.** Drive's `changes` API, with its `startPageToken`,
   in place of the full listing each run. The full listing is 1 request
   per 1,000 files, so this matters only for a large Drive.
4. **ODF from the native JSON**, Sheets first (above), then the
   ODF-with-versions export step built on it.
5. **A republish command**, export upload first, Sheets replay second.
6. **The wizard.** One catalog entry has one `credentialService`, and
   this source needs four. Either latchkey grows one service covering all
   four scopes, or the catalog learns several.

Not planned: other Drive files (PDFs, images, uploaded Office files).
Their bytes are a plain `alt=media` download with a real
`headRevisionId` and `md5Checksum`, so they would be a natural next
kind, but they are not what this source is for yet.
