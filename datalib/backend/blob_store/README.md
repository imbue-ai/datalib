# The blob store

A step that keeps attachment bytes keeps them in one file in its own
tree, its **blob store**. The server hands those bytes to a page when
someone opens it, without knowing anything else about the step that
wrote them: not its other tables, not which kind of source it is. What
it does know is this file's shape, and this crate is the one place
that shape is written down. The download side writes through it
(`datalib_etl::blob_cas`), and `datalib-http` reads through it.

## The contract

- **Where:** `<data root>/<group>/<function>/blobs.sqlite`. It is a
  plain SQLite file, not a doltlite one, and only the step whose tree
  it is in writes it.
- **What:** one table, `cas_objects (blake3, byte_len, content_type,
  bytes)` ([`CAS_OBJECTS_DDL`](src/lib.rs)). Each row is filed under
  the blake3 hash of its bytes, in lowercase hex, and never changes
  once written. The same bytes arriving twice are kept once.
- **The address:** `/data/<group>/<function>/blob/<blake3>`
  ([`address`](src/lib.rs)). A page links an attachment by it, and
  `datalib-http` answers it with the row's bytes and type.

A step written by anyone, in any language, that keeps a file of this
shape in its tree has its attachments served with no change to
datalib.

## What the server does with an address

`datalib-http` checks that `<group>` is one of the config's groups,
that `<function>` is a plain directory name, and that the hash is 64
lowercase hex digits, then reads the row. The step itself need not be
in the config: a render-only source's pages link the blob store in an
`ingest` tree no step of the config writes. It never reads a file from
the data root by the path it was given: the root also holds
`system/api-token`. An unknown group, a missing store and a missing
row are all "not found".

A row never changes, so the answer tells the browser to keep it.
Bytes that a browser would run script from if it opened them (HTML,
SVG) are sent with a sandbox policy, and script itself is sent as
plain bytes, so an attachment cannot run in the app's own origin.
