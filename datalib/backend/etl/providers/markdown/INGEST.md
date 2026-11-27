# markdown — download

Reads a folder on this machine — an Obsidian vault, a folder of notes —
and stores the text of every `.md` and `.markdown` file under it, one
row per file, keyed by the file's path under the folder. No account and
no credentials: the config is `fswalk.path`. Turning a note into a page
is the render step's job ([`../markdown_render/TRANSLATE.md`](../markdown_render/TRANSLATE.md)).

## What one run does, and what the next costs

The scan is [`file_changes_scan`](../../files/README.md#answering-did-it-change-for-a-file-backed-source)
with a per-path cursor (`ingested_files`, scope `markdown/files`):

- **An unchanged file costs a `stat`.** The host's fingerprint cache
  vouches for its hash, the cursor says this source already stored that
  hash, and nothing is read. A second scan of an unchanged folder
  commits nothing (`a_second_scan_of_an_unchanged_folder_commits_nothing`
  in `tests/markdown_e2e.rs`).
- **A file whose stat moved is read once**, hashed as it is read. If
  the bytes are what the cursor holds (a `touch`, a sync tool rewriting
  the same text) nothing is written. Otherwise its row is written and
  the file stamped, in one transaction.
- **A file that is gone takes its note with it**, in the transaction
  that deletes its row from `ingested_files`. A moved or renamed file
  is a new row at its new path; the old path's row goes once the new
  one is stored. Nothing is deleted after a walk that reported an
  error, or while a file that changed could not be read, since a
  vanished note may be where it went.

Folders whose names start with a dot are never opened (`.*/` is always
the first ignore pattern, `MarkdownConfig::scan_ignores`): `.obsidian/`
holds the editor's state, `.trash/` deleted notes, `.git/` and a sync
tool's `.stversions/` old copies. A note found there would be a stale
second copy of one found outside it. `ignore` adds gitignore-shaped
patterns. A `.gitignore` inside a git repository counts as git would
read it, and so does an `.ignore` file anywhere in the tree.

## The store

| table | key | holds |
|---|---|---|
| `markdown_files` | the path under the folder, slash-separated (`Bridge/Warp core.md`) | `blake3` and `size` of the bytes read, `modified_at` (the file's mtime, ending `Z`: a filesystem stamp names no clock), and `text`, the whole file |
| `markdown_scan_meta` | the source's id | `abs_root`, the folder as the scan resolved it; render builds each note's `file://` link from it |
| `ingested_files` | `(scope, rel_path)` | the cursor: each file's hash when it was last stored |

Both data tables have the usual `_bookkeeping` sidecar.

The note's text is in the store, not read from disk at render time, so
the store's history holds every version a sync read, including notes
since deleted from the folder.

## When part of a scan fails

- **A walk error** (a folder that will not list) is a `listing:files`
  row, and that scan deletes nothing.
- **A file over `max_bytes`** (16 MiB unless set) is not read and is a
  `record:files:<path>` warning; it keeps whatever row an earlier scan
  stored for it.
- **A file that will not open** is `record:files:<path>`; it is not
  stamped, so every scan tries it again, and the scan that reads it
  clears the row.
- **A file that is not UTF-8** is stored with each byte that is not
  replaced by U+FFFD, and a `file:markdown/files:<path>` row saying from
  which byte. The row stays until the file changes.

## What it does not do

- It does not follow Obsidian's links (`[[Note]]`), embeds (`![[x]]`)
  or tags into anything structured: they stay as the text of the note.
- It does not read attachments (images, PDFs) beside the notes; point a
  `media` or `pdf` source at the same folder for those.
- A folder is not yet a container a person can browse by; where a note
  is filed is its `source_path`.

## Inspecting a scan

`datalib-doltlite` reads the store; stock `sqlite3` cannot open a
`.doltlite_db`, but `datalib-doltlite -readonly <store>.doltlite_db .dump
| sqlite3 out.sqlite` turns it into a plain SQLite file.

```sh
datalib-doltlite -readonly <root>/<source>/ingest/entities.doltlite_db \
  "SELECT id, size, modified_at FROM markdown_files ORDER BY modified_at DESC LIMIT 20"
datalib-doltlite -readonly <root>/<source>/ingest/entities.doltlite_db \
  "SELECT scope_key, sample FROM problems"
```
