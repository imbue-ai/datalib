# datalib URIs: one grammar for everything datalib names

*Proposal (2026-10-20), out for review; nothing in §2 onward is built.
Once built, this becomes the reference `docs/dev/uris.md`.* Every name
datalib mints for a thing becomes `datalib:<kind>/<id>`, one opaque
grammar with one Rust crate and one TS module behind it, unless a
standard scheme like `mailto:` already names it. Each form grew where
it was first needed, so today there are two syntaxes on one scheme,
four writers in Rust, and a route grammar posing as a name. Attachment
links (`attachments_by_link.md`) are about to add a new family, so this
is the moment to settle the grammar.

Already decided: kinds are `snake_case`, and the attachment `blob`
kind is the prototype for the shared crate.

## 1. What exists today

| form | names | written by | read by |
|---|---|---|---|
| `datalib:group/<id>` | a group of the config | `datalib_columns::Entity::uri` | `Entity::parse`, `entityFromUri` (`chipLinks.js`) |
| `datalib:step/<group>/<function>` | a step | `Entity::uri` | the same |
| `datalib:contact/<id>` | one of your contacts | `datalib_contacts::contact_uri` | `contactFromUri` |
| `datalib:handle/<kind>/<value>` | a person, by a handle kind with no scheme of its own | `Handle::to_uri` | `Handle::from_uri`, `handleFromUri` |
| `mailto:`, `tel:`, `slack://user?…`, a LinkedIn profile URL | a person, by a handle kind with a standard scheme | `Handle::to_uri` | the same |
| `datalib://chat/<uuid>?msg=<section>`, `datalib://search?…`, `datalib://prefs` | a route of the app | `core::deeplink`, `ui/src/router/deeplink.ts` | nothing outside their fixture tests |
| `/chat/<uuid>`, `#/chat/<uuid>`, `/#/chat/<uuid>` | a document, as a path in the app | `grid_rows.entire_chat`, renderers' links | `chatUuidFromHref` (`chatLink.ts`) |
| `blobs/<file>` | an attachment, relative to its page | render | the applet's `/asset` route |

What this costs:

- **Two syntaxes share one scheme.** `datalib:<kind>/…` is opaque;
  `datalib://…` is hierarchical, with a host. A source whose id is
  `chat`, `search` or `prefs` would make `datalib://<id>/…` ambiguous,
  and a URL parser may lowercase a host.
- **The `datalib://` forms name routes, not things.** "Search with
  these parameters" and "the preferences screen" are states of the UI.
  The only one written in practice is the copied 🆔, which names a
  document. The desktop shell registers no handler for the scheme
  (`tauri/README.md`), so pasting one opens nothing.
- **A document has four spellings**: the deep link and the three path
  shapes `chatLink.ts` accepts.
- **The grammar is scattered**: four places in Rust, three TS files.
  Some kinds have test cases shared between the two sides (handles,
  entities, deep links); others have none.

## 2. The grammar

Use a standard scheme when one exists, so that a link pasted into
another app still works: `mailto:`, `tel:`, Slack's `slack://user?…`, a
LinkedIn profile URL. That part is unchanged. Everything else is
`datalib:<kind>/<id>`, in opaque form: no `//`, so no host for a parser
to lowercase.

1. **A URI names a thing, never a route or a file.** What the UI does
   with it (opens a card, draws a chip, fetches bytes) is the reader's
   business. The same URI can be drawn, clicked, copied and pasted.
2. **The kind comes from a closed set**: a strum enum in Rust,
   `snake_case`, like every other enum we store.
3. **Each kind says how many segments its id takes.** A group's id is
   one segment; a step's is two (`<group>/<function>`). Only a leaf
   kind may take the rest of the path: a handle's value keeps its `/`s.
4. **A thing inside another is its container's URI plus
   `/<kind>/<id>`.** The bytes a step's store holds are
   `datalib:step/gmail/ingest/blob/<blake3>`. A reader that knows the
   container but not the inner kind still knows where the thing lives.
5. **A part of a document is a fragment**:
   `datalib:doc/<markdown_uuid>#<section_uuid>`, which is what a
   fragment means everywhere else.
6. **Ids we mint go in raw** (group and function ids, uuids, hashes),
   since all of them are path-safe already. Anything else is
   percent-encoded, as handle values are today.
7. **No query string**, until something needs a parameter that is not
   part of the name.

## 3. The kinds

| kind | URI | names | a click | status |
|---|---|---|---|---|
| `group` | `datalib:group/<id>` | a group | its chip's card | exists |
| `step` | `datalib:step/<group>/<function>` | a step | its chip's card | exists |
| `contact` | `datalib:contact/<id>` | one of your contacts | the person card | exists |
| `handle` | `datalib:handle/<kind>/<value>` | a person, by a handle with no scheme | the person card | exists |
| `doc` | `datalib:doc/<markdown_uuid>[#<section_uuid>]` | a document, or a section of it | opens it | new: replaces `datalib://chat/…` and the `/chat/…` paths |
| `blob` | `datalib:step/<group>/<function>/blob/<blake3>` | bytes in that step's store | the bytes | new: the prototype |
| `eml_part` | `datalib:step/<group>/<function>/eml_part/<eml_blake3>/<part_blake3>` | one MIME part of a stored `.eml` | the bytes | planned |
| `vcard_photo` | `datalib:step/<group>/<function>/vcard_photo/<contact_id>/<blake3>` | the photo inside a stored vCard | the bytes | planned |

**The kinds that name bytes** are served by the
`unified_document_index` applet at `/store/<group>/<function>/<kind>/…`.
The UI rewrites a `src` or `href` to that route in one place, after
sanitizing, so a markdown image, a link, and a renderer's raw `<audio>`
or `<video>` are all treated alike. Every other kind stays a link the
UI handles on click, and no click on a `datalib:` link ever leaves the
app (`chips.md`).

**A `doc` names the document, not where its markdown is kept.** Today
every document is rendered at sync time and stored. The same URI also
fits a document rendered when someone opens it, such as an old email
built from its `.eml` on request rather than kept as markdown. Readers
resolve `datalib:doc/<uuid>` through one route, and that route decides
whether to read a stored render or make one. Rendering on demand needs
the uuid to lead back to the raw record that produces it; §6 asks how.

## 4. Where the code lives

One crate, one module and one fixture file, so the two sides cannot
drift apart:

- **`datalib_uri`, a Rust crate with no dependencies.** An enum with
  one variant per kind, `Display` to write a URI and `parse` to read
  one. `parse` returns `None` for a URI this build does not know.
  `Entity`, `contact_uri`, the `datalib:handle/` arm of
  `Handle::to_uri`, and the render side's link writers all build on it.
  Handles keep their standard schemes in the `handle` crate.
- **`ui/src/uri.ts`, its mirror in TypeScript.** `chipLinks.js`,
  `chatLink.ts` and `asset_urls.ts` read through it.
- **`ui/tests/uri-fixtures.json`, parsed and written by both test
  suites**, as `deeplink-fixtures.json` is now. A kind without a
  fixture case fails a test.

## 5. Moving to it

1. **`blob` ships in this form from the start**, with `datalib_uri`
   holding only that kind. Attachments never ship in another form.
   `eml_part` and `vcard_photo` follow.
2. **`group`, `step`, `contact` and `handle` move onto the crate.**
   Their spellings do not change, so nothing stored moves.
3. **`doc` replaces `datalib://chat/<uuid>?msg=<section>`** for the
   copied 🆔. People may have pasted the old form into their own notes,
   so `parse` keeps reading it as a `doc` (AGENTS.md § "Keep a forward
   path": input from a person keeps an alias). `core::deeplink` and
   `router/deeplink.ts` go, along with `search` and `prefs`, which
   nothing writes.
4. **`grid_rows.entire_chat` becomes `datalib:doc/<uuid>`.** The grid
   index is rebuilt from the render stores, so this costs one
   re-render. `chatLink.ts` accepts the three old path shapes only
   while a stored body still carries them.
5. **When the desktop shell gets a deep-link handler**, it registers the
   `datalib` scheme and opens whatever `parse` reads.

## 6. Open questions

- **`doc` or `object`?** The app's route and the grid column say
  "chat", but the thing is any rendered document: a PDF, a Notion page,
  a Garmin day. `doc` is the working name; `object` would leave room
  for things that are not documents.
- **How does a document rendered on demand lead back to its record?**
  Its `markdown_uuid` is minted from raw ids today, but nothing maps it
  back. Options: keep a small index from uuid to raw record without
  storing the markdown, or give such documents a URI that names the
  record itself (`datalib:step/<group>/<function>/record/<table>/<id>`),
  so the route can render it.
- **Which documents would render on demand?** Old mail is the obvious
  first one: rarely opened, and the bulk of the render tree. Search and
  the grid still need their rows, so a document rendered on demand
  still has its `grid_rows` and search text stored.
- **Does the opaque form work for an OS handler?** macOS and Windows
  dispatch on the scheme alone, so `datalib:doc/…` should open as
  `datalib://` would. To check against `tauri-plugin-deep-link` before
  §5 step 5.
- **Is there anything we name that this grammar cannot spell?**
