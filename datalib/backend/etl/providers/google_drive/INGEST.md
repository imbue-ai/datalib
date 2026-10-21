# `google_drive` — how the download works

A backup of a Google account's Docs, Sheets and Slides. The store keeps
each file in Google's own native form at every version a run saw. Beside
it are the file's comments, its revision list and the images it shows,
and, when the config asks, an export LibreOffice can open (`exports`,
`revision_exports`, both off by default). There is no render step yet. Why it is
shaped this way, what Google offers and what is still to come:
[`docs/dev/plans/google_drive.md`](../../../../../docs/dev/plans/google_drive.md).

## Where it comes from

Four Google APIs, each signed by its own latchkey built-in service, all
signed in as the same account:

| host | service | what for |
|---|---|---|
| `www.googleapis.com/drive/v3` | `google-drive` (any of the four also signs `files`) | the listing, revisions, comments, exports |
| `docs.googleapis.com` | `google-docs` | a Doc's native JSON |
| `sheets.googleapis.com` | `google-sheets` | a Sheet's native JSON |
| `slides.googleapis.com` | `google-slides` | a deck's native JSON |

Image links in the native JSON (`docs.google.com/*-images-rt/…`) are
signed by Google for about 30 minutes and go out as plain `curl`
(`api::req_image`). Google signs them afresh on every read, so they are
taken out of the JSON that is stored (`shape::take_image_links`). The
image bytes, which do not change, are stored by where each link was.

A refusal from one service (latchkey has no sign-in for it, the token
lacks a scope, or the API is off for the OAuth project) ends that kind
only. The others still run, and the refused kind is a `phase:` problem
naming the service to sign in to again. Each kind therefore drains on
its own.

## What one run does

1. **List.** `files.list` over the configured kinds, not trashed, shared
   drives included, 1,000 a page (`api::list_url`). With `file_ids` set,
   one `files.get` per named file instead; a missing one is a config
   problem (`file_ids:<id>`).
2. **Owe.** A file is owed when `files_bookkeeping.held_version` is not
   the `version` the listing names (`owed::owed`). Drive's `version`
   goes up on every change to the file.
3. **Capture.** Up to four files are fetched at once
   (`Captures::capture`). Each fetch reads:
   - the revision list, paged, unless the account may only comment
     (`capabilities.canReadRevisions = false`), when Drive lists none.
     It is listed twice, and every revision either listing named is
     kept: Drive's first listing after a quiet spell can leave out the
     oldest revision. A file whose two listings disagreed is logged and
     counted in `revision_listings_disagreed`;
   - the comments, paged, with `includeDeleted=true`;
   - the native JSON, gzipped, as Google sent it less its image links,
     which go in the capture's sidecar (`volatile_payload`);
   - every image link in it, once per distinct URL;
   - one export per format `exports` names for the file's kind: a list
     for every kind, `[]` for none, or a table per kind.

   A capture is written whole, in one transaction, or not at all. If any
   request fails, the file stays owed with the error on its sidecar and
   a `files:<id>` problem. An export Google declines as too large
   (`exportSizeLimitExceeded`, over 10 MB) is the one exception: the
   capture is kept, its export part has no bytes, and a warning on the
   file says which format was declined.
4. **Old revisions.** For every Doc and Sheet in this run's listing,
   each stored revision with an export link for a format
   `revision_exports` names is owed that export, until it is held
   (`revision_exports`, held at no version, since a revision's content
   does not change). Each export is two requests:
   - the revision's `exportLinks` URL on `docs.google.com`, which
     latchkey signs (≥ 3.19.2) and Drive answers with a `307`;
   - the `Location` it names, a signed `googleusercontent.com` download,
     fetched plain so the credential never follows it to another host
     (`api::get_revision_export`).

   A `404` means Drive no longer has that revision's content: the edge
   is kept with no bytes and a warning, and not asked for again. A
   deck's revisions are never fetched: its export link ignores
   `revision=` and exports the deck as it is now. Measured on one deck,
   revision 1 and the newest exported identical ODP, PPTX and PDF.
   Docs and Sheets honour it: three revisions of a Doc and three of a
   Sheet each came back different.

Each run leaves a row in `sync_runs`: its config, its summary (`listed`, `owed`, `captured`, …) and what it changed in each table.

**A second run** with nothing edited costs the listing alone: one request
per 1,000 files. Measured on 2026-10-21, on 4 real files (a Doc, a
Sheet, a deck and a Doc shared with the account): the first run made 4
captures with 4 exports and 17 images in 58 s, and the second run owed
nothing.

**Why nothing is re-fetched for a version already held.** An export is
not byte-stable: two exports of one version differ in their zip
timestamps. A second capture of an unchanged file would store a second
3 MB copy of the same thing. The native JSON, less its image links,
is the same on every read, but it is only fetched with the rest.

## The store

`entities.doltlite_db`, beside the CAS `blobs.sqlite`. Every key is
Drive's:

| table | key | holds |
|---|---|---|
| `files` | file id | the listing entry, as of the newest capture. `permissionIds`, `permissions`, `owners`, `parents` and `spaces` are sorted (`shape::order_file_entry`); `thumbnailLink` and every `photoLink` go to the sidecar's `volatile_payload` |
| `revisions` | `{file}#{revision}` | each revision Drive listed: number, time, author, its `exportLinks` |
| `comments` | `{file}#{comment}` | each thread with its replies inline, deleted ones marked `deleted` |
| `captures` | `{file}#{version}` | one row per version captured: the listing's `modifiedTime`, the native JSON's own `revisionId` (Docs and Slides), and the newest Drive revision listed |
| `capture_parts` | `{capture}#{part}` | a CAS edge per part: `native.json.gz`, `export.<ext>`, `image:<JSON pointer>` |
| `revision_exports` | `{file}#{revision}#export.<ext>` | a CAS edge per old revision's export, NULL `blake3` for one Drive no longer has |

Every table has its `_bookkeeping` sidecar. `files_bookkeeping.held_version`
is the one held version in the store.

**What is never deleted.**
- **Files.** A file that leaves the listing (trashed, unshared, deleted)
  keeps everything it had.
- **Revisions and comments** are upserted on each capture, so one Google
  later merges away or drops stays.
- **Captures** are append-only.

The history of a file is its `captures` rows, never this store's commit
log.

**Why the native JSON is in the CAS** and not a JSONB payload:
- doltlite stores a large value whole, so a one-cell edit to a 5 MB
  Sheet payload grew the store by 5.1 MB in a measurement;
- gzipped, the same Sheet is about 44 KB;
- the gzip header carries no time, so an unchanged file packs to the
  same blob, and the CAS stores it once.

**What it costs.** Measured on the 4 files above:
- the CAS held 24 MB: 14.6 MB of ODF exports, 8 MB of images and
  1.6 MB of native JSON;
- `entities.doltlite_db` was 0.4 MB.

The exports are big because Google embeds the document's fonts in each.

## What it does not do

- **No native JSON before the first sync.** Google's native APIs read
  only the current version, so a version from before the first sync is
  kept only as its export, and a deck's not at all.
- **No delta listing.** Drive's `changes` API is not used; the whole
  listing is read every run.
- **Only native files.** Other Drive files (PDFs, images, uploaded
  Office files) are not listed.
- **No render, and no index.** Nothing reaches the grid or search yet.
- **No Add source form.** A config entry is written by hand
  (`docs/user/config_examples/all_sources.toml`).
- **A capture can be newer than its version.** It is keyed by the
  `version` the listing named. If the file was edited while it was being
  fetched, the content is newer, and the next listing captures it again
  under its new version.

## Inspecting a store

Stock `sqlite3` cannot open a `.doltlite_db`; use the doltlite shell
(`bazelisk build //third-party/doltlite:doltlite`), or turn the store
into plain SQLite:
`datalib-doltlite -readonly entities.doltlite_db .dump | sqlite3 out.sqlite`.

```sh
DL=bazel-bin/third-party/doltlite/doltlite
# Each file's versions, newest first, and what each capture holds
$DL -readonly entities.doltlite_db "
  SELECT c.file_id, c.version, c.modified_time,
         (SELECT group_concat(p.part, ' ') FROM capture_parts p
           WHERE p.capture_id = c.id AND p.part NOT LIKE 'image:%') AS parts
  FROM captures c ORDER BY c.file_id, CAST(c.version AS INTEGER) DESC"
# Files by kind and name
$DL -readonly entities.doltlite_db "
  SELECT payload->>'\$.mimeType', payload->>'\$.name' FROM files ORDER BY 1, 2"
# Open comment threads
$DL -readonly entities.doltlite_db "
  SELECT file_id, payload->>'\$.content' FROM comments
  WHERE payload->>'\$.resolved' = 0 AND payload->>'\$.deleted' = 0"
# Pull one version's native JSON out of the CAS
sqlite3 blobs.sqlite "SELECT writefile('native.json.gz', bytes) FROM cas_objects
  WHERE blake3 = '<blake3 from capture_parts>'" && gunzip native.json.gz
```

The hermetic tests are `//datalib/backend/etl/providers/google_drive:google_drive_tests`,
on a TNG tape (`tests/google_drive_tests/tape.rs`). The live check runs
against the signed-in account:
`GOOGLE_DRIVE_TEST_FILE_IDS=<id>,<id> DATALIB_RUNTIME_DIR=<staged runtime> bazel run //datalib/backend/etl/providers/google_drive:google_drive_live`
(`scripts/stage_runtime.sh` stages one).
