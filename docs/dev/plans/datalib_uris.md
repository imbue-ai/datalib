# datalib links: a name for what something is, an address for its bytes

*Mirror of the "Names and addresses" tab of the [datalib links](https://claude.ai/code/artifact/dd31c9e3-8ef9-4185-a85f-0c29a1318117) Claude Doc, which is the source of truth while this plan is under review. Edit the doc and re-mirror this file; don't edit it here. Proposal (2026-10-20); step 1 of §6 landed in PR #1305.*

Copy a message's 🆔 today and paste it into your notes: you get a link like `datalib://chat/1f3a…?msg=9c2e…`, and it opens nothing anywhere. That link is one of ten ways datalib writes a link to something it holds, and the attachments work is about to add more. This doc proposes two forms to replace all of them but card links (§1), before the new ones ship: a `datalib:` **name**, which says what something is, and a `/data/…` **address**, where the server hands out its bytes.

Words used throughout:

- **Data root**: the directory datalib keeps everything in, one subdirectory per group.
- **Group** and **step**: a group is one entry on the Manage screen, usually a data source such as a Gmail account. A step is one job a group runs: `ingest` downloads, `render_markdown` turns what was downloaded into pages. Each step writes under `<data root>/<group>/<step>/`.
- **Store**: the files a step keeps its data in. Each source's `ingest` step keeps attachment bytes in one file, `blobs.sqlite`, each under a hash of its contents, so an attachment that arrives twice is kept once.
- **URI**: a link that names something, like `mailto:riker@enterprise.org`. The part before the first colon is its *scheme*; datalib's own scheme is `datalib:`.
- **Chip**: the small rounded label the app draws for a person, a source or a step inside a page. Double-clicking one opens that thing.

Already decided: kinds are written in `snake_case`, and attachment addresses are built first, to try the design out.

**Status:** step 1 of §6 (the blob-store contract crate, the `/data/` route, and `blob` addresses for chat sources, Notion and Google Drive Docs) landed in PR #1305. Nothing else here is built.

## 1. What exists today

Ten forms, with no one place that defines them:

| Form | Points at | Where you meet it |
| --- | --- | --- |
| `datalib:group/<id>` | a source | a source's chip, e.g. in a storage report |
| `datalib:step/<group>/<function>` | a step | a step's chip, e.g. `gmail/ingest` |
| `datalib:contact/<id>` | one of your contacts | a person chip tied to the contacts app |
| `datalib:handle/<kind>/<value>` | a person, by an id with no standard link (a Signal account, a Facebook name) | a person chip |
| `mailto:`, `tel:`, `slack://user?…`, a LinkedIn profile address | a person, by email, phone, Slack user or LinkedIn member | a person chip; also works pasted into other apps |
| `datalib://chat/<uuid>?msg=<message>` | a page, or a message on it | the link a copied 🆔 becomes |
| `datalib://search?…`, `datalib://prefs` | a search, the preferences screen | nowhere: only tests write them |
| `/chat/<uuid>`, in three spellings | a page | the grid's `entire_chat` column, links between pages |
| a path of card sources, one segment per card | the cards to open, and their state | a link to a layout of cards; any path the app's router does not otherwise match is read as one (`router/index.ts`, `cards.md` §"Card source") |
| `blobs/<file>`, served at `/applet/unified_document_index/asset/<page uuid>/blobs/<file>` | an attachment, as a file beside its page on disk | an image in an email |

Card links are the one form that names a *screen* rather than a thing, and they stay: they are the third kind of link beside names and addresses (§2), and nothing here changes them.

What goes wrong:

- **A copied 🆔 opens nothing.** The desktop app doesn't register `datalib://` with macOS or Windows, and nothing in the app reads one back, so pasting it into a browser, a note or datalib itself does nothing.
- **Two spellings share the `datalib:` scheme, and they can collide.** In `datalib://chat/…` the word after `//` is read as a host name. Had attachments been linked as `datalib://<source>/…`, a source named `chat` would look like a 🆔 link, and a browser may lowercase a host, turning a source `Gmail` into `gmail`.
- **A page is linked four ways**, so code that recognizes a page link has to know all four.
- **Every attachment is on disk twice**: once in its source's store, and again as a file beside each page that shows it. Email is the same: the stored email carries each attachment encoded as text, and render writes it out again decoded.
- **Attachment links and players in chat pages don't open in the app**, from reading the code: only images are pointed at the server, so a `blobs/report.pdf` link resolves against the app's own address. Not yet tried in the app.
- **The format lives in seven places**: four in the Rust backend, three in the UI. Some forms are checked by tests run against both sides; others by nothing.

## 2. The proposal: a name, and an address

Two forms, each with one job:

- **A name says what something is**, and means the same wherever it is pasted: a page, a note, another app. Where a standard form exists, use it (`mailto:`, `tel:`, Slack's `slack://user?…`, a LinkedIn profile address), so the link also works outside datalib. Everything else is `datalib:<kind>/<id>`, never `datalib://`: without the `//`, nothing in it is read as a host name, so nothing gets lowercased and nothing collides with an app route.
- **An address says where the server hands out something's bytes**: a path that starts with `/data/`, which stands for the data root. `/data/gmail/ingest/blob/<hash>` is the attachment with that hash in the store under `<data root>/gmail/ingest/`. A path starting with `/` resolves against whichever server sent the page, so a page uses it as it is: in an image, a download link, an audio player.

**Rules for names:**

1. **A name says what, never which screen.** `datalib:doc/<uuid>` is a page. Whether a click opens a card, a chip is drawn, or a copy produces text is up to whoever displays it.
2. **Every kind is on one fixed list**, written in `snake_case`. A build that meets a kind it doesn't know says so rather than guessing.
3. **Each kind has a fixed number of parts between slashes.** A source's id is one part, a step's is two (`gmail/ingest`). Only the last part of a name may contain `/`: a handle's value can.
4. **A message within a page goes after `#`**: `datalib:doc/<page uuid>#<message uuid>`, the way `#` picks out a place on a web page.
5. **Ids datalib makes are written as they are** (source and step ids, uuids, hashes), since they never contain spaces or slashes. Anything else is percent-encoded, a space becoming `%20`, as handle values already are.

**Rules for addresses:**

1. **Under `/data/`, not at `/`.** The server already answers `/api`, `/applet`, `/agent`, `/metrics`, `/modules`, `/assets` and `/dactal`, and each of those is a legal group id today. More to the point, the app reads every path it does not otherwise route as a card link, so `/data/` has to be routed before that, and no card link may start with `data/`. One prefix keeps both true, and keeps every group id usable after we add routes.
2. **Rooted at the server, never relative to the page.** A page is drawn in a frame built from inline HTML with no address of its own, so a relative link would resolve against the app's address. A page's file can also move, and a page made on request has no file at all.
3. **An address reaches only what the blob-store contract describes (§3), never a file on disk.** The server never reads a file from the root by its path, since the root also holds `system/api-token`, the key to the whole API.
4. **The path after `/data/` follows the tree**: `<group>/<function>/blob/<hash>`. The step comes first, so the address says whose store holds the thing.
5. **What an address returns never changes**, since it names its bytes by their hash. A browser may keep what it fetched for good.

## 3. The kinds

Names, for things you point at:

| Kind | Name | Points at | A double-click opens | Status |
| --- | --- | --- | --- | --- |
| `group` | `datalib:group/<id>` | a source | its sync dashboard (its main card, once built) | exists |
| `step` | `datalib:step/<group>/<function>` | a step | its log | exists |
| `contact` | `datalib:contact/<id>` | one of your contacts that has no handle; a contact with one is drawn as its best handle's chip instead | nothing yet: the person card is keyed by handle, so a contact with none has no card (a handle chip opens the person card, contact included) | exists |
| `handle` | `datalib:handle/<kind>/<value>` | a person, by an id with no standard link | the person card | exists |
| `doc` | `datalib:doc/<page uuid>[#<message uuid>]` | a page, or a message on it | the page | new; replaces `datalib://chat/…` and the `/chat/…` spellings |

Addresses, for bytes the server hands out. Both reach the same place, a step's blob store:

| Address | Returns | Used for | Status |
| --- | --- | --- | --- |
| `/data/<group>/<function>/blob/<hash>` | the stored bytes with that hash | attachments of chat sources, Notion and Google Drive Docs | built, PR #1305 |
| `/data/<group>/<function>/blob/<email hash>/part/<part hash>` | one attachment from inside a stored email | email attachments | planned |
| `/data/<group>/<function>/blob/<hash>`, once the contacts download keeps each photo in its blob store | a contact's photo | contact photos | planned |

**The blob-store contract is the one thing the server knows about a step's data**, a standardized tentacle:

- **Where:** `<data root>/<group>/<function>/blobs.sqlite`, a plain SQLite file that only that step writes.
- **What:** one table, `cas_objects (blake3 PRIMARY KEY, byte_len, content_type, bytes)`. Each row is filed under the hash of its bytes and never changes once written.
- **Inside an email:** a row whose type is `message/rfc822` is an email, and `part/<hash>` picks out the attachment whose bytes have that hash. Taking an email apart follows MIME, the standard every email uses, so this is knowledge of a format, not of any source.

The server knows nothing else: not a source's own tables, not its versioned store, not which kind of source a step is. A custom step that writes a `blobs.sqlite` in this shape has its attachments served with no change to datalib.

**Who answers an address: `datalib-http`, for now.** It reads the blob store directly, through one small crate that defines the contract and that the download side writes through too. AGENTS.md's rule becomes: `datalib-http` reads stores' commit logs and blob stores, and no other rows. §7 says when to revisit this.

**The server picks a handler by kind** in `/data/<group>/<function>/<kind>/…`; `blob` is the only one. How an applet could later answer a kind, and how that shares one pattern with names, is the third tab's §2, "Shared federation".

**A `doc` name points at the page, not the file it is stored in.** Today every page is written when its source syncs, and stored. The same name would also fit a page made only when someone opens it, such as an old email turned into a page from its stored `.eml` at that moment instead of being kept on disk. Readers ask one route for a `doc`, and that route either reads the stored page or makes it. §7 asks what that needs.

## 4. Before, after, and what a person notices

| Change | Before | After | What a person notices |
| --- | --- | --- | --- |
| Attachments | Written again as a file beside every page that shows them | The page points at the copy the source already keeps, at `/data/…` | Less disk used. In the app, nothing, except that attachment links and players in chat pages should start opening. Someone reading a page's `.md` file outside the app sees a `/data/…` path and needs a running datalib to open it. |
| Copied 🆔 | `datalib://chat/<uuid>?msg=<message>` | `datalib:doc/<uuid>#<message>` | The pasted text looks different. It still opens nothing until the desktop app registers the scheme. An old link pasted back is still understood. |
| Links between pages, inside the app | Four spellings | One `doc` name | Nothing, apart from one full re-render of every source after the upgrade. |
| Source, step, contact and person chips | Written in four places in the code | Written in one | Nothing: spelled as today. |
| A script or agent | Opens a page's attachments as files beside it on disk | Fetches them from `/data/…`, sending the API token like any other request | An agent reading the mirror on disk has to ask the server for an attachment. `datalib://search` and `datalib://prefs` go; nothing outside the tests writes them. |

## 5. Where the code lives

One Rust library and one UI module write and read the names; one small crate holds the blob-store contract:

- **`datalib_uri`**, a small Rust library with no dependencies: one variant per kind of name, a way to write it and a way to read it back, plus the `/data/<group>/<function>/` start of an address. Reading a kind it doesn't know returns "unknown", never a guess. Every place that writes a link today goes through it.
- **`ui/src/uri.ts`**, the same in the UI. The code that draws chips and opens pages reads names through it. Addresses need nothing from the UI: the browser fetches them as they are.
- **`ui/tests/uri-fixtures.json`**, a list of example names and addresses that tests on both sides read and write. A kind with no example fails a test.
- **The blob-store contract crate** (name to pick): the table's definition, opening a blob store read-only, fetching a row by hash, and taking a part out of an email. The download side writes through it and `datalib-http` reads through it, so neither defines the table on its own. It depends on SQLite and a mail parser, and on nothing else of datalib's.

## 6. The order of work

1. **Attachment addresses first**: the contract crate, `datalib-http`'s `/data/` route, and `/data/…/blob/<hash>` for chat sources and Notion. Email's `part` addresses follow, then contact photos, once the contacts download also puts each photo in its blob store.
2. **The existing chip kinds move onto the library.** Their spelling doesn't change, so nothing stored changes.
3. **`doc` replaces the copied-🆔 link.** The old `datalib://chat/…` form is still read, since people may have pasted it into their own notes. The code for the old routes goes.
4. **The grid's `entire_chat` column becomes a `doc` name.** The grid is rebuilt from the stored pages, so this costs one re-render. The UI stops accepting the three old spellings once no stored page uses them.
5. **When the desktop app learns to open links from other apps**, it registers `datalib:` and opens whatever the library reads.

## 7. Open questions

- [ ] **When should serving move from `datalib-http` to the search applet?** Moving it is moving one route, since an address never names the server that answers it. Revisit if a new kind of address needs more than the blob-store contract (a source's own tables, its versioned store), or if parsing untrusted email inside the process that holds the API token and runs every sync proves a risk worth a separate process.
- [ ] **`doc` or `object`?** The app calls these pages "chats", but they are any page datalib renders: a PDF, a Notion page, a day of Garmin data. `doc` is the working name; `object` would also cover things that aren't pages.
- [ ] **`/data/` or another prefix?** It needs to be one word no route will ever want for itself.
- [ ] **Should a page have an address too?** For example `/data/<group>/render_markdown/doc/<uuid>` for its markdown. An export could then write each page and its attachments at their addresses, and every link in the exported tree would work offline.
- [ ] **How would a page made on request find what to make it from?** A page's uuid is computed from the ids of the records it shows, but nothing maps a uuid back to them. One way: keep a small table from page uuid to record, without storing the page. An address that names the record itself would need more than the blob-store contract: only the step that wrote a record can read it.
- [ ] **Which pages would be made on request?** Old email is the obvious first: rarely opened, and most of what render writes. Search and the grid still need each page's row and searchable text, so those stay stored.
- [ ] **Can macOS and Windows open a name without `//`?** Both pick the app by the part before the colon, so `datalib:doc/…` should open the same way `datalib://` would. To check against Tauri's deep-link plugin before step 5.
- [ ] **Is there anything datalib names or serves that these two forms can't write?**
