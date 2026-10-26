# The blob store

A step that keeps attachment bytes, or people's photos, keeps them in
one file in its own tree, its **blob store**. The server hands those bytes to a page when
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
  ([`address`](src/lib.rs)). A page links an attachment or a photo by
  it, and `datalib-http` answers it with the row's bytes and type.
- **A part of a stored email:**
  `/data/<group>/<function>/blob/<blake3>/part/<part blake3>`
  ([`part_address`](src/lib.rs)) names one MIME part of the email
  stored under `<blake3>`: the part whose *decoded* bytes (after base64
  or quoted-printable, and for text, as UTF-8) hash to `<part blake3>`.
  The answer is those bytes, typed as the email types the part, with
  `; charset=utf-8` on text. A row is an email when it is stored as
  `message/rfc822`, or stored with no type and reads as a message with
  headers; a row typed as anything else is never taken apart. Only the
  email's own parts are looked at, not those of an email attached to it
  ([`part_of`](src/lib.rs)). So a step that keeps whole emails gets each
  attachment served without storing it twice. Reading MIME is knowledge
  of a format, not of a source.

A step written by anyone, in any language, that keeps a file of this
shape in its tree has its attachments served with no change to
datalib.

## What the server does with an address

`datalib-http` checks that `<group>` is one of the config's groups,
that `<function>` is a plain directory name, that what follows `blob/`
is a hash or a hash, `/part/` and a hash, and that each hash is 64
lowercase hex digits, then reads the row (and the part). The step itself need not be
in the config: a render-only source's pages link the blob store in an
`ingest` tree no step of the config writes. It never reads a file from
the data root by the path it was given: the root also holds
`system/api-token`. An unknown group, a missing store, a missing row, a
row that is not an email and a part the email does not have are all
"not found".

A row never changes, so the answer tells the browser to keep it.
Bytes that a browser would run script from if it opened them (HTML,
SVG) are sent with a sandbox policy, and script itself is sent as
plain bytes, so an attachment cannot run in the app's own origin. For a
part, the part's own type decides this.
