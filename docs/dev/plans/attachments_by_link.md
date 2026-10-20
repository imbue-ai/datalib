# Attachments by link, not by copy

*Proposal (2026-10-20). Nothing here is built.* Render stops copying
attachment bytes into the rendered tree. A page links to the bytes
where they already live, and the `unified_document_index` applet serves
them from there.

## What happens today

Render writes every attachment it links to into a `blobs/` directory
beside the page, and the page links it relatively (`blobs/<hash16>.<ext>`).
Three places write those files:

- `BlobBundle::materialize_to_dir` (`etl/src/blob_cas.rs`), called by
  chat-common (email, Slack, Signal, WhatsApp, Claude, ChatGPT, Beeper,
  SMS, Google Takeout, Facebook, LinkedIn) and by Notion.
- contact-common's `write_photo`, for a contact's photo.

The UI rewrites `blobs/x.png` to the applet's
`/asset/{markdown_uuid}/blobs/x.png` (`ui/src/cards/asset_urls.ts`), and
that route reads the file from disk next to the markdown.

So every attachment is on disk twice: once in the source's CAS
(`<group>/ingest/blobs.sqlite`) and once as a loose file. Email is
worse: its CAS holds only the `.eml`, so the attachment is there
base64-encoded and again decoded in `blobs/`. Most of those copies
belong to old mail and old chats nobody opens.

## The link

Each source has its own stores, so a link names the step whose tree
holds the bytes, using the step's URI with the thing inside it
appended (`datalib_uris.md` §2). Three kinds:

| link | where the bytes are |
|---|---|
| `datalib:step/<step_id>/blob/<blake3>` | the row of `<step_id>/blobs.sqlite` with that hash |
| `datalib:step/<step_id>/eml_part/<eml_blake3>/<part_blake3>` | the MIME part, inside that `.eml` in the CAS, whose decoded bytes have that hash |
| `datalib:step/<step_id>/vcard_photo/<contact_id>/<photo_blake3>` | the `PHOTO` of that vCard row in `<step_id>/entities.doltlite_db`, if its decoded bytes have that hash |

- **`blob`** covers every chat-common source, Notion, and LinkedIn's
  contact photos, which are already in the CAS (the `contact_photos`
  edge).
- **`eml_part`** covers email's attachments and inline images. Render
  already names each part by its content hash (`extract_attachments_from_emls`).
  The server finds the `.eml` in the CAS, parses it, and returns the
  part whose hash matches. That parse runs on every request. Opening old
  mail is rare enough that this beats keeping a copy, and since the
  bytes never change behind a link, the browser caches the answer for
  good.
- **`vcard_photo`** covers CardDAV and `.vcf` contacts, whose photo
  stays inside the vCard by design (the `contacts` ladder dropped
  `contact_photos`). The hash makes the link specific to one photo: once
  the card changes, the old link answers 404 and the next render writes
  a new one.

The type served is the CAS's `content_type` (already corrected from the
bytes at ingest), the part's `Content-Type`, or the vCard's `TYPE`. A
`?name=` parameter can carry the upstream file name for the download's
`Content-Disposition`; the link works without it.

## Serving it

The applet serves the three links at
`/store/<group>/<function>/<kind>/…`, beside `/asset`. It already
links the render side, and `datalib-http` must not link `etl` or a
mail parser.

- **The path is checked, never joined in as typed.** Every CAS is an
  ingest step's, so the function must be `ingest`, and the group must
  be a valid id segment (letters, digits, `.`, `_`, `-`; never `.` or
  `..`). The hash must be 64 hex digits. Anything else is a 404.
- **Opening the CAS needs no pin.** It is plain SQLite holding only
  committed rows, so the route opens it read-only, reads, and closes.
- **The vCard read is a doltlite reader.** It uses a detached read-only
  open (`doltlite_raw::open_reader`), and its statement goes on the
  `doltlite_two_process_test` allowlist before it ships
  (`etl/README.md`).
- **Bytes that can run script are already contained.** The gateway
  sends everything an applet serves with `nosniff`, gives HTML and SVG
  the `DATA_DOCUMENT_CSP` sandbox, and serves script as bytes
  (`proxied_response` in `http/src/lib.rs`).
- **Caching is a follow-up.** A link names its bytes by hash, so an
  answer could be `Cache-Control: immutable`, but the gateway passes on
  only `Content-Type`. That matters most for `eml_part`, where every
  request parses the `.eml`.

## Render

- `BlobBundle` builds links rather than file names: `markdown_link` and
  chat-common's `rel_path` become a `datalib:step/<step_id>/blob/…` URI.
  `materialize_to_dir` and `write_photo` go.
- To render a link, render needs only the hash, the type and the name.
  `load_many` stops selecting `bytes`, and the existence check that
  feeds `blob_missing` asks only whether the hash is in `cas_objects`.
  Email still loads its `.eml`s, because it needs the body.
- contact-common takes a photo link instead of bytes, and so does the
  `photo_url` the index carries for chips.
- The render version moves, so every page is rendered again. When
  render writes a page, it deletes any `blobs/` directory left beside
  it.

## The UI

The sanitizer's hook rewrites a link to the applet route wherever one
appears in a `src` or `href`: a markdown image, a link, or a renderer's
raw `<audio>` or `<video>`. Today only markdown images are rewritten,
so an attachment link or a player relative to `blobs/` resolves
against the app's own URL. The sanitizer's allowlist already admits
`datalib:`, for chips. A step chip's URI is exactly
`datalib:step/<group>/<function>`, so `entityFromUri` and
`Entity::parse` stop claiming anything longer as a step.

## What this gives up

The rendered tree on disk is no longer complete on its own. A `.md`
opened off disk, or read by an agent, links to bytes it cannot follow
without the app. `docs/agent_user.md` should say how to fetch one: the
applet route, or the CAS row by hash. An export that writes the files
out on demand can come later, if someone needs it.

## Docs to change with it

`data_architecture_parse_and_render.md` (the page "beside a `blobs/`
directory"), `etl/README.md` § "Blob CAS" (the paragraph on file
names), `docs/agent_user.md`, and the header comment of `asset_urls.ts`.

## Order

1. The `blob` route and the UI rewrite, then chat-common and Notion
   switched over. This covers most of the bytes. `materialize_to_dir`
   stays for email's parts until step 2.
2. `eml_part`, for email; `materialize_to_dir` goes.
3. `vcard_photo`, for contacts, and LinkedIn's photos (already in its
   CAS) as `blob` links, since both go through contact-common's
   `write_photo`, which goes.

The old `/asset` route stays until nothing writes `blobs/`.
