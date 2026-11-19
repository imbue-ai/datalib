# markdown — render

Each row of `markdown_files` becomes one document with one grid row
(`markdown.Note`, `is_document`). Nothing is read from disk: the text is
in the raw store ([`../markdown/INGEST.md`](../markdown/INGEST.md)).

## The document

- **Its id** is minted from the path alone (`IdNamespace::Markdown`,
  entity kind `note`, no stamp), so an edit keeps it and a rename makes
  a new note, as the store does.
- **Its title** is the file name without `.md` or `.markdown`, the way
  Obsidian names a note.
- **Its front matter** carries `source_path`, the path under the folder.
  That is the fact search's page head reads as `Path:`, so a note is
  found by the folders it is filed in, as a PDF is
  (`datalib/backend/search_index/src/text.rs`, `PAGE_FACTS`). It also
  carries `modified_at`, the file's mtime, and `source_url`, a `file://`
  link built from the scan's root.
- **Properties**: a note that opens with a YAML block (`---` on its
  first line, closed by `---` or `...`) has the block split off and
  shown as a fenced `yaml` code block, so markdown does not read it as
  a rule and a heading. It is not parsed; its words are searchable as
  text. A block that never closes is part of the body, as an editor
  reads it.
- **The body** goes in as its author wrote it, HTML included, in a
  section of its own, with any fence or HTML block it leaves open closed
  at its end (`close_open_blocks`). Each section is rendered alone, so
  nothing in a note reaches its title.

The grid row's text is the title and the whole note, which is what the
keyword search reads. Its date is `modified_at`; there is no
`created_at`, since a file's mtime is not when the note was written.

## Incrementality

A note's bucket is its path, and it reads two rows: its own
`markdown_files` row and the `markdown_scan_meta` row. A run renders the
paths whose rows the diff since the cursor names, and the ones the
driver found stale (a new scan root makes every note stale). A path it
was asked for that has no row any more is declared with nothing, so its
document goes. Pinned by
`an_edit_a_move_and_a_delete_render_only_those_notes` in
`../markdown/tests/markdown_e2e.rs`.

`RENDER_VERSION` is in `src/render/mod.rs`; bump it for a change to what
a note renders to.
