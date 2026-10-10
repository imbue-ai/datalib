# Contacts: who a handle is

How datalib knows that the `riker@enterprise.org` in a mail, the
`+1 202 555 0101` in a WhatsApp chat and the `U_RIKER` in a Slack
thread are the same person, and draws them as one chip.

Three things to know before the rest:

- **One shape for a person, made at render time.** Every source keeps
  its own raw records as it downloaded them. When it renders, it
  translates what it knows about each person into one shared shape,
  `NormalizedContact`. Nothing is stored in that shape on the download
  side.
- **Your own contacts use the same shape.** A contact you curate in the
  contacts app is answered as a `NormalizedContact` too, so everything
  that reads a source's record of a person reads yours the same way.
- **Linking is after the fact, optional, and sits over the view.** No
  source and no render knows about your contacts. A document names a
  person only by a handle (an email address, a phone number, a Slack
  id). Linking handles to a contact happens later, in the contacts app,
  and changes how a chip is drawn: its name, photo and menu. Documents,
  the index and search are untouched by a link (see
  [Searching for a person](#searching-for-a-person)).

## The same move chat-common makes

Fourteen chat-like sources (email, Slack, WhatsApp, Signal, Messages,
the AI chats and more) each store messages in their own raw shape.
Their render crates don't each write markdown: each translates its raw
rows into one neutral shape, `NormalizedChat`, and
[chat-common](../../datalib/backend/etl/chat-common/README.md) turns
that into the document and its grid rows. A provider's render knows its
source; chat-common knows what a message looks like.

People get the same treatment at the render layer. A Slack profile, an
address-book card, a LinkedIn connection, a WhatsApp address-book
entry, and the bare fact that someone wrote in a chat under some name
all describe a person, each in their source's own shape. Every one of
them is translated into one neutral shape, `NormalizedContact`
(`datalib_contact_schema`): what one source knows about one person, in
terms no source owns. `contact-common` is the counterpart of
chat-common for the sources that are *about* people: it turns a
`NormalizedContact` into a document.

On top of that sit three layers, each of which works without the one
above it:

1. **Handles.** A render writes every person it can identify as a
   normalized identifier, a *handle*, inside the document. The index
   knows handles and never contacts.
2. **Source contacts.** Every source that knows something about a
   person says so as a `NormalizedContact`, and the index keeps those
   rows so it can answer "who holds this handle?" from the sources
   alone.
3. **Contacts.** A person links handles to a contact of their own in
   the contacts app, the one store under a data root that nothing can
   rebuild. Its answer ranks above every source's.

What is still to build: [`plans/contact_linking.md`](plans/contact_linking.md)
(merge, groups, numbers without a country code), [`plans/search_autocomplete.md`](plans/search_autocomplete.md)
(contacts in search) and [`plans/contact_editing.md`](plans/contact_editing.md)
(the contact card: its fields, drafts, saving, undo, the export).
How every chip is written, drawn, resolved and clicked — a person's, a
group's, a step's — is [`chips.md`](chips.md). This page says what the
tree does for a person.

## Words

| word | means |
|---|---|
| **handle** | one identifier in one namespace, normalized: `email:riker@enterprise.org`, `tel:+12025550101`, `slack:T01/U02`, `signal_aci:<uuid>`, `facebook:name/Will Riker`. `datalib_handle` makes them; the UI's `chipLinks.js` mirrors its rules by hand. |
| **render** | the pipeline's render step: it writes a source's documents (markdown files) and grid rows into its render store during a sync, and the index and qmd read them from there. "Renders again" means those stored documents are rewritten. |
| **draw** | what the UI does with a chip when it is shown: it asks who the handle is and paints the name, photo and mark. Nothing is stored. |
| **chip link** | how a document names a person: a markdown link whose href is the handle as a URI, `[Will Riker](mailto:riker@enterprise.org "Will Riker <riker@enterprise.org>")`. The viewer draws it as a chip. |
| **source contact** | a `NormalizedContact` a source wrote: that source's attempt at a neutral, unified record of what it knows about one person, keyed by `source_id` and the source's own `key`. The index stores them in `source_contacts`; the UI holds them as `Who.sourceContacts`. |
| **contact** | a person's own record in the contacts app, also answered as a `NormalizedContact`, with `source_id = "datalib_contacts"`. |
| **link** | a row in the contacts app saying a handle belongs to a contact. Curated by a person; no source or step makes one. |
| **the contacts app** | the `datalib_contacts` applet and its store under `datalib_curated/`. Optional: without it, chips still say what the individual sources know, and offer nothing to link. |

## Handles

[`datalib/backend/handle`](../../datalib/backend/handle/src/lib.rs) is a
crate with no first-party dependencies, so render crates, the index, the
applets and the contacts store all share one definition. A handle is
`<kind>:<value>`. The rule that matters most: **where a native id is an
email address or a phone number, the handle is `email:` or `tel:`, not a
per-app kind**, so one link covers every app that reaches a person that
way. Only an id that is opaque by nature gets a kind of its own.

So WhatsApp, Signal, Messages, SMS backups, Google Voice and an
address-book card all write the same `tel:+12025550101` for one
number. What that costs:

- **The handle does not say which app.** The row it sits on does
  (`source_id`, `provider`), so `from:tel:…` finds a number's
  messages in every app, and a source filter narrows it.
- **A number's state is one state across apps. This is the cost to
  watch.** `stopped_working_by` is on the handle, so a person who keeps
  WhatsApp on a number their carrier has since given away cannot be
  marked "stopped for texts, still works on WhatsApp": marking it
  stopped marks it stopped everywhere. A number reassigned to someone
  else is attributed to its first owner everywhere (§"The contacts
  app"), and sharing one namespace spreads that mistake across every
  app. If either matters, the fix goes on the link (a stop scoped to
  some sources, or a validity range), not into a `tel:` per app, which
  would undo the one link that covers them all.
- **A chip's URI is `tel:`**, which a browser hands to the phone
  dialler, whichever app the message came from.
- **It depends on every source spelling numbers alike.** Only numbers
  written in international form get a `tel:` handle at all (below).
  WhatsApp writes a linked id (`…@lid`) as a number only where the
  backup maps it to one, and Signal writes the account id where it has
  no number, so the same person can arrive under a second handle.

| kind | value | made by | URI (`Handle::to_uri`) |
|---|---|---|---|
| `email` | the address, lowercased | `Handle::email` (takes a `mailto:` too; the domain needs a dot) | `mailto:<address>` |
| `tel` | E.164, `+` and digits | `Handle::tel` (a number written with its `+` and country code, any separators; `Handle::whatsapp_jid` for a `<number>@s.whatsapp.net`) | `tel:<number>` |
| `slack` | `<team_id>/<user_id>` | `Handle::slack` | `slack://user?team=<team>&id=<user>` |
| `signal_aci` | a Signal account id, a lowercase dashed UUID | `Handle::signal_aci` | `datalib:handle/signal_aci/<uuid>` |
| `facebook` | `name/<the name shown>`, its whitespace collapsed; or `deleted/<conversation id>` | `Handle::facebook_name`, `Handle::facebook_deleted` | `datalib:handle/facebook/<value>`, percent-encoded but for its `/`s |

A Facebook export names people and never numbers them, so a
`facebook:name/` handle is a name: two people of one name are one
handle, and nothing in the export can tell them apart. An account
deleted since is written `Facebook user`, or with no name at all, and
gets a handle only in a Messenger conversation that lists it as its one
deleted account: `facebook:deleted/<that conversation's id>`. The same
deleted person in two conversations is two handles; several in one
group are none.

`Handle::parse` reads back exactly what `as_str` wrote and refuses any
other spelling, which is what every store and every wire format goes
through (serde uses it). `Handle::rebuild` is looser: it runs a stored
value through its kind's constructor again, which is how a store brings
its handles along when the rules change. `Handle::from_uri` reads a
chip link's href; `Handle::describe` is the link's title and the text a
chip copies as, `Will Riker <riker@enterprise.org>`,
`Data (slack:T01/U02)`.

### A number without its country code

`Handle::tel` takes only a number written in international form. A
number written the way it is dialled at home, `(202) 555-0101`, is
refused rather than given a country by guess: the same digits are a
different person in another country. The value is kept as the source
wrote it (`ContactHandle::value` with `handle: None`), so nothing is
lost, but nothing links to it either.

That is not a rare case. WhatsApp and Signal always store the
international form, but an address-book card is often typed by hand
without one, and a phone's SMS backup keeps whatever the phone was
given. A card whose numbers
are all national reaches nobody through them. The fix is a default
region that the render applies to such a number, planned in
[`plans/contact_linking.md`](plans/contact_linking.md) §"Handle kinds not made yet".

### Changing the rules

`RULES_VERSION` in the handle crate is bumped whenever a constructor
returns something different for some input: a spelling newly accepted
or refused, a value normalized another way. Two things hang off it:

- **Every source renders again.** The render step puts the version in
  every source's render params as `_handle_rules`
  (`datalib_step/src/render.rs`), so a bump renders everything again the
  way any param change does, and no provider bumps its own
  `RENDER_VERSION` for it.
- **The contacts store respells the links a person made.** Each rules
  change adds a rung to `datalib_contacts::LADDER` that runs
  `rebuild_handles`, and a test fails until it is added. A link the
  new rules cannot read, or whose new spelling another contact holds,
  is kept as written and logged, never dropped.

A change to `tel` or `email` has a second copy to keep in step:
`handleFromUri` in `ui/src/cards/chipLinks.js`. Its `tel` rule today is
looser than Rust's (no ten-digit rule under `+1`, no trunk `(0)`), so
the UI can mark a chip that `Handle::from_uri` would refuse.

**A new kind is not a rules change**: no stored handle reads
differently, so the version stays. It touches these places. The
compiler finds the Rust `match`es and `KIND_ICON` (once the TypeScript
union has the kind); nothing finds the rest:

| where | what |
|---|---|
| `handle/src/lib.rs` | the `HandleKind` variant and its constructor; an arm each in `rebuild`, `to_uri` and `describe`; a branch in `from_uri` unless the URI is `datalib:handle/…`; a row in `every_kind_round_trips_through_its_uri` |
| `contact_schema/src/lib.rs` | `Medium::of_kind` |
| `applets/src/unified_index/columns.rs` | `handle_mark`, the mark the grid's Author chip shows |
| `ui/src/cards/chipLinks.js` | `KINDS`, `handleFromUri`, `uriFromHandle`, and a row in `tests/chip_links.test.ts` |
| `ui/src/cards/contacts.ts` | the `HandleKind` union, `handleKind`, `KIND_ICON`, and `copyText` if `describe` is not `label (handle)` |
| `ui/src/cards/sanitize.ts` | `ALLOWED_URI_REGEXP`, if the URI's scheme is a new one |
| `tests/fixtures/ingested_tng_test.py` | a fixture person reached by the new kind, so the index is seen to know them |

### Who writes a handle

A provider's render decides, at normalize time, which identifier it
has for a person; `NormalizedChatItem::author_handle`,
`Recipient::handle`, `NormalizedChatItem::mentions` and
`NormalizedReaction::reactor_handle` carry it.
What each source has today:

| source | author | more |
|---|---|---|
| email | the From address | To, Cc and Bcc (the sender's copy) as recipients; an `@` or `+` mention, a `mailto:` link in the fresh part of an HTML body |
| Slack | `slack:<team>/<user>` | a `<@U…>` mention in a body; each reaction's user; the profile's email, in the source contact |
| WhatsApp | the sender's number, a linked id (`…@lid`) through `jid_map` | each reaction's sender |
| Signal | the number, else the account id (ACI); a recipient known by PNI alone has none | a mention (a `mentionAci` body range), by number else ACI, written `@Name` in place of its `U+FFFC`; number and ACI together, in the source contact; one known by ACI alone reads as the dashed ACI. The ACI is read by its one path in the stored frame, and one that will not read is a problem on the recipient (`aci`, `CoercionFailed`), not a silent loss |
| Messages | the number or Apple ID address | each tapback's |
| Google Chat and Voice, SMS backup | the address or number; a group MMS, which does not say which number sent it, has none | |
| address books | | a card's numbers and addresses, in the source contact |
| LinkedIn | | a connection's email address, in the source contact |
| Facebook | a Messenger message's sender, by name; a deleted account by its conversation, where it is that conversation's only one | each Messenger reaction's; a friend's name, in the source contact |
| Beeper | none yet: a Matrix user id has no kind | |

The AI chats, calendar, GitHub, GitLab and Notion write no handles.
Where a source renders the owner's side as "Me" (WhatsApp, Signal,
Messages, SMS backup, Google Voice), "Me" has no handle. Where it does
not (email, Slack, Google Chat), the owner's own messages carry their
address or user id like anyone else's.

## In a document

chat-common writes an author with a handle as a chip link in the
message header, and recipients as a line straight under it; the shape,
and why the href is load-bearing, is
[`chat-common/README.md`](../../datalib/backend/etl/chat-common/README.md)
§"The message header". A reactor with a handle is a chip link too, in a
message's reaction list and in the list of reactions to messages not
in the mirror (`render.rs::reactor`). Slack writes a `<@U…>` mention as
a chip link in the body (`slack_render/src/render/mrkdwn.rs`), and as
plain `@Name` inside code, which shows what it holds; an email's
`@` mention is a chip link too (`email_render/src/render/mentions.rs`).
A group of contacts lists its members under its field table, each a
chip link where the member's card has an address or a full number
(contact-common, from `ContactDoc::member_handles`).
Every mention a source marks up is also a `mention` search term on its
message's row. Every chip link
the backend writes is shaped by one function,
`datalib_etl_render::message::chip_link`.

The viewer draws a chip for any explicit link whose href is a handle,
whoever wrote it, so a `mailto:` link in an email's HTML body is drawn
as a chip too. A chip anywhere in a body is safe for one reason: **it
shows who the href resolves to, never the link text**. A link
`[Picard](mailto:phisher@romulan.net)` gets a chip for the phisher
under the name the sources know, with "Shown here as “Picard”" in its
tooltip.

## Source contacts: `NormalizedContact`

[`datalib/backend/contact_schema`](../../datalib/backend/contact_schema/src/lib.rs)
is only the shape: `source_id` and `key` say who describes the person;
then `kind` (`person` or `group`), `names` (the preferred one first),
`handles` (each a `ContactHandle`: its `medium`, the value as written,
its `Handle` if one could be made, a label, and `stopped_working_by`),
`photo` (the image, or a URL only a fetch could follow), `photo_url`
(where the app serves it; see below), `org`, `title`, `note`,
`details`, `groups`, `members`, `source_url`, the record's own stamps,
and `seen` (how much of a chat the person wrote).

Four producers, each needing less of a provider than the one before:

- **The contacts app**, from its store, with `source_id =
  "datalib_contacts"`.
- **A source about people**, through `contact-common`
  ([`etl/contact-common`](../../datalib/backend/etl/contact-common/src/render.rs)):
  an address-book card, a LinkedIn connection, a Facebook friend. Each
  is a document of its own, and carries its source contact.
- **A chat provider, for what only it knows**, in
  `NormalizedChat::contacts`: Slack's profiles (names, title, avatar,
  the email), WhatsApp's address book (`JidNames::contact`), Signal's
  recipients (number and ACI together). These are rows, not documents.
- **chat-common, for free** (`chat-common/src/people.rs`): a baseline
  per handle per document from what the provider showed: each author
  with the names it wrote under, how many items and the last one's
  stamp; each recipient and each reactor under the name shown, having
  written nothing. A provider's own source contact for the same handle
  replaces the baseline and keeps its count (`document_contacts`).

Every `RenderedMarkdown` carries its `contacts`, and the render store
and the index hold them in two tables: `source_contacts`
(`markdown_uuid`, `contact_key`, `source_id`, `name`, `seen_items`,
`last_seen_at`, and the whole record as `contact_json`) and
`source_contact_handles` (`markdown_uuid`, `contact_key`, `handle`;
the index's copy is indexed by handle). `grid_index` loads them the way
it loads `grid_rows`.

**`POST /people` on the `unified_index` applet** takes `{"handles":
[…]}` and answers `{"people": {<handle>: [NormalizedContact, …]}}`: for
each handle, every source contact holding it, one per source and key
(the rows from each document summed), ranked by
`unified_index/src/people.rs`: a source about people first, then the
chat where the person wrote the most. A handle no source mentions is
absent. This works with no contacts app at all.

### Photos

`photo` is what the source gave: the image itself (`Photo::Inline`,
written beside the rendered page as `blobs/<uuid>.<ext>` and never
stored as a row) or a URL nothing fetched (`Photo::Url`, a Slack
avatar). `photo_url` is what a chip draws: a path on the app's own
origin, never another host's, since the app fetches nothing remote
unasked, and only for an image a browser draws: png, jpeg, gif or
webp (`contact_schema::DRAWABLE_PHOTO_TYPES`, the one list, which the
contacts app's photo rule shares). contact-common fills it with
`/applet/unified_index/asset/<markdown_uuid>/blobs/<file>`, which the
index's asset route serves; a photo of another type keeps its file
beside the page and gets no URL. The contacts app fills it with
`/applet/datalib_contacts/photo/<contact_id>` for a photo a person put
on their contact. With no photo, or one the browser fails to load, a
chip linked to a contact draws the person's initial, and any other
chip draws the mark for its handle's kind.

## The contacts app

`<data_root>/datalib_curated/datalib_contacts/contacts.doltlite_db`,
written only by the `datalib_contacts` applet
([`datalib/backend/contacts`](../../datalib/backend/contacts/src/lib.rs)
is the store, `applets/src/datalib_contacts.rs` the routes). It is a
person's work, so the rules that make it irreplaceable:

- **Every write is a commit** on `datalib_writer`, sealed onto `main`
  (`etl/README.md` §"Connection pools"), so the history is an audit
  trail.
- **Nothing resets it.** A reset or removal of a source or the index
  never touches `datalib_curated/`. Every shape change is a rung on
  `LADDER`, never a rebuild (`doltlite_raw::open_curated` refuses a
  shape it cannot reach additively).
- **It is optional.** Take its `[[applets]]` entry out of the config
  and every source, the index and search keep working; chips draw what
  the sources know and offer nothing to link (the gateway's 502
  "no applet" is what `contacts.ts::isAbsent` reads).

| table | key | holds |
|---|---|---|
| `contacts` | `contact_id`, a random v4 (the one id here that is a person's act, not a function of a record) | `kind`, `name`, `note`, `merged_into`, stamps |
| `handles` | `handle`, as `datalib_handle` spells it | `contact_id`, `linked_how`, `linked_at_utc`, `stopped_working_by` |
| `members` | `(group_id, member_id)` | `added_at_utc` |
| `photos` | `contact_id` | `content_type`, `bytes`, `set_at_utc` |
| `fields` | `field_id`, minted by the card | `contact_id`, `kind` (`FieldKind`), `label`, `value`, `handle`, `position`, `copied_from_source` and `_key` |

Each table is a row struct in `contacts/src/schema.rs`, and its DDL
and upsert are derived from it (`#[derive(PortableTable)]`).

Two rules the tables encode: **a handle belongs to exactly one
contact** (linking one someone else holds is refused, never taken
over; a shared address belongs to a group contact), and **keys are
handles, never `grid_rows.uuid`**, which moves when a recipe changes.
`stopped_working_by` is a date *by* which the handle had stopped
working: an old number still names its owner in every message from
when it worked, so the link stays and the chip marks it as old. "By",
not "on": marking a handle fills in today, which is always true when
nothing better is known, and the person narrows it if they remember. It
is a partial date (`2019`, `2019-06`, `2019-06-14`), a date a person
remembers rather than an instant anything measured, so it is not an
`_at_utc` stamp. A number *reassigned* to someone else is not covered:
the key is the handle alone, so a handle has one owner for all time.
Nor is a number that stopped working in some apps and not others: a
stop is on the handle, and `tel:` is one handle across every app
(§"Handles").

The routes, each behind the gateway's secret
([`applets.md`](applets.md)). A refusal the store explains (a handle
someone else holds, a contact with no name, a bad date, not a photo)
is a 409 with the store's words; a handle that does not parse is a
400, and a contact or photo that is not there a 404:

| route | does |
|---|---|
| `POST /resolve` | `{handles}` → `{resolved: {<handle>: NormalizedContact}}`, each handle's contact; a handle nobody holds is absent |
| `GET /search?q=` | contacts whose name contains `q`, for the popover's typeahead |
| `POST /contacts` | create, with `name`, optional `kind`, and the `handles` to link at once |
| `GET /contact/{id}` | one contact, working handles first |
| `POST /link`, `POST /unlink` | one handle to or from a contact |
| `POST /stopped_working` | `{handle, by}`; `by: null` means it works again |
| `POST /rename` | |
| `GET`, `PUT`, `DELETE /photo/{id}` | the photo as bytes; put one (the body, with its `Content-Type`: png, jpeg, gif or webp, at most 4 MB; the route reads up to the gateway's 8 MB so the store's rule is the one that answers); drop it |
| `GET /contact/{id}/edit` | what the card edits, as published: name, note, fields |
| `POST`, `GET /contact/{id}/draft` | open the contact's draft (cut it if there is none) / read it: `{base, mine, published, published_commit}` |
| `PUT`, `DELETE /contact/{id}/draft` | autosave the whole edit onto the draft, uncommitted / discard the draft |
| `POST /contact/{id}/draft/save` | `{seen}`, the published commit the card last showed; `{"outcome": "saved", "commit"}`, or `{"outcome": "stale", "view"}` with nothing saved when the contact moved since |

The config entry is `[[applets]] id = "datalib_contacts"` with
`command = "datalib-applet datalib_contacts"`; the gateway passes the
data root in the environment.

### Fields, and editing a contact through a draft

A **field** is a line of what a contact says about the person (a
number, an address, a title), and a **link** is a handle the contact
holds. Neither implies the other: a household's landline can be a
field on two contacts and linked to nobody. A field's `handle` is its
value as a handle where it is one, so the card can say how the field
stands against the links; it links nothing.

The card edits a contact's name, note and fields through a **draft**
(`contacts/src/drafts.rs`, over `datalib_etl::draft`): a branch
`draft/<contact_id>` where each autosave is an uncommitted write, which
no reader sees. A save publishes it as one commit, the draft winning
only the cells it changed, so a link or a rename made meanwhile
survives. The save names the published commit the card last showed; if
this contact's name, note or fields have moved since, nothing is saved
and the card gets the new state to show. Links and photos change at
once, never through a draft.

## In the UI

Everything is in `datalib/ui/src/cards/`:

- `chipLinks.js` is the markdown-it plugin: an explicit link whose
  href `handleFromUri` reads becomes `<a class="chip" data-handle=…>`
  (and one naming a group or step, `data-entity`; see
  [`chips.md`](chips.md)); a link `linkify` made from a
  bare address in running text is left alone, so a signature's address
  stays an address. Plain JavaScript, so the render preview runs it
  too. It mirrors `to_uri` and `from_uri` over the same test cases.
- `contacts.ts` holds the pure rules, unit-tested in `contacts.test.ts`:
  `chipLook` (what a chip shows, from the contact if there is one,
  else the best-ranked source contact, else what the source showed),
  `chipTooltip` (the chip's title: who, the identifier, each source's
  record and the person's other handles), `chipMenu` (copy the name,
  the identifier or both; find everything from the person; link or
  edit), `copyText` and the copy rewrite. `people` is who each handle
  is, for the whole app: the resolver ([`chips.md`](chips.md)
  §"Resolving") over `/people` and the contacts app's `/resolve`; an
  edit (create, link, unlink, no longer works) forgets the handles it
  touched. An edit made anywhere else — another window, an agent —
  reaches every page as the live stream's `curated` frame, on which
  `people` asks again: the applet holds its store open, so the OS
  reports none of its writes, and instead the gateway nudges
  `datalib-http`'s watch after every write it forwards to an applet;
  the watch compares each `datalib_curated/` store's head and sends the
  frame only when one moved (`http/src/watch.rs`). A draft's autosave
  is no commit, so it sends none. `decorateHandles` draws a document's
  chips and `chipCell` a grid cell's.
- `ChatBody.ce.vue` runs the decorate pass over a document's frame and
  owns the popover
  (`HandlePopover.ce.vue`: link to a contact, create one, unlink, mark
  a handle as no longer working) and the right-click menu
  (`ChipMenu.ce.vue`). `chip.css` is the one look.
- `PersonCard.ce.vue` is the card a person chip's double-click opens
  (`personView`; its rules are `person.ts`): your contact when the
  handle is linked to one, then each source's record of the person, a
  section per source and never merged, the source the chip was seen in
  first. It reads only, but for *Create contact* on an unlinked handle;
  linking stays the popover's.
- The grid's Author column is a chip too: `grid_rows.author_handle`
  (which `from:` finds through the search terms) comes with each
  message's row and each reaction's, and
  `GridCard.ce.vue` draws each Author cell from `people` and redraws
  them when an answer changes. The applet names the mark for a handle's
  kind in `columns.rs::handle_mark`.
- A contact's own row is drawn by who it is about, not by where it is
  filed. Its grid row names the person in `contact` and
  `conversation_name`, puts its first email address and phone number
  in `email` and `phone`, and files the address book (or LinkedIn's or
  Facebook's list) in `channel` alone. The grid's Contact column is a
  chip from those (`columns.rs::contact_identity`: the address, else
  the number, by the rule `contact-common::chip_handle` writes the
  members' chips by), and the search list titles such a row with the
  same chip (`SearchList.ce.vue`). Browsing an address book opens on
  Contact, Phone, Email, Contents (the note first), Touched, Type and
  then Channel (`config/browsePresets.ts`).

A chip ranks what it hears: your contact first, then the source
contacts as `/people` ranked them, then the text the source showed.

## Searching for a person

A person is searched by the role they had on a row, through the search
terms (`docs/dev/plans/search_tabs.md` § "The search terms"), which
hold each row's people by handle:

| key | finds the rows where the person |
|---|---|
| `from:` (and `author:`, `author_handle:`) | wrote it: by handle, or by the name they were shown under |
| `to:`, `cc:`, `bcc:` | was in that header |
| `recipient:` | was in any of them |
| `mention:` | was mentioned, where the source marks a mention up |
| `with:` (and `involves:`) | had any role, took part in the conversation the row is (a document's own row), or is who a contact's card is about |

A value that is a handle (`email:riker@enterprise.org`, or simply
`riker@enterprise.org`, `+12025550101`) matches that handle exactly, and
the search bar draws it as the person's chip; a quoted value matches a
handle or name whole (`from:"Will Riker"`); anything else matches any
handle or name holding it, so `from:riker` finds Riker under every
spelling a source used, and anyone else whose name holds it. A name
also reaches every handle a source ever showed under it, through the
search terms' `names` table, filled from the rows' authors and from
`source_contacts`. A chip's "Everything from <name>" writes
`from:<handle>`. Typing `@` at the start of a word offers people and
writes the pick as `with:`.

A conversation's own row answers `with:` for everyone who took part in
it: chat-common supplies a `participant` term on each document's row
for every author, recipient, mention and reactor of the messages it
holds, by handle, else by the name shown. So `is:document with:riker`
lists the conversations Riker was in, one row each, whatever his role;
without `is:document` the same search also finds each of his messages.

A contact's card answers `with:` and never `from:`: contact-common
supplies an `about` search term for each handle the card holds and each
name it gives, and nobody wrote the card. So `with:riker` finds his card
beside everything he took part in.

[`search_by_example_tests.rs`](../../datalib/backend/applets/src/unified_index/search_by_example_tests.rs)
shows each of these rules on a small cast: how its rows are filed, and
which queries do and do not find them.

**`contact:<id>` is everything from Riker, whatever handle he used.**
Links sit over the view, so the search terms know handles and never
contacts. A `contact:<id>` value on a person key is read from the
contacts store when the search runs (`datalib_contacts::read`, a
detached read, so the applet stays the one writer) and matches each
handle the contact reaches: its own, stopped ones included; those of
the contact it was merged into, or merged into it; and, for a group,
every member's. The search bar offers your contacts first and draws the
value as a contact chip. Making a link changes nothing in the index: no
document renders again, qmd re-indexes nothing, and search follows the
link at once, as chips do. A root without the contacts app refuses
`contact:` by name ([`plans/search_autocomplete.md`](plans/search_autocomplete.md)
§"Contacts: expanded when the search runs").

**Keeping the name out of the markdown is a choice, not a rule.**
Writing a contact's name into each chip link (its text or its title)
would make a document say who a handle is on its own: readable as a
plain file, findable by `grep`, and searchable by qmd under the name
you gave the person. It would cost what the paragraph above saves:
render would read the contacts store, and a link would render again
every document naming the handle and send each back through qmd. The
search terms name exactly those documents, so the cost can stay
proportional to the link. Nothing in the tree rules it out: chips draw
from the live answer whatever the link text says.
[`plans/contact_linking.md`](plans/contact_linking.md) §"Option: the contact's name
in the markdown" keeps it open.

## What renders again when

Which changes make the render step rewrite stored documents. Drawing a
chip in the UI is separate and always uses the latest answer.

| change | what moves |
|---|---|
| the handle rules (`RULES_VERSION`) | every source renders again; the contacts store takes a ladder rung |
| a new handle kind | nothing stored; the places above |
| a provider's handles or source contacts | that provider's `RENDER_VERSION` |
| the header, the recipients line, or what `people.rs` counts | chat-common's `LAYOUT_VERSION`, which renders every chat source again |
| what contact-common writes | the `RENDER_VERSION` of each source that uses it (contacts, linkedin, facebook) |
| a link, an unlink, a handle marked stopped | no document; open chips are drawn again |
| the contacts store's shape | a rung on `LADDER`; never a reset |

## Where it is tested

- `handle`: every constructor's spellings, `parse` against `rebuild`,
  the URI round trip over every kind.
- `contacts`: the store end to end on a real doltlite file, the ladder
  against the current rules, the photo rules.
- `chat-common`: `people.rs` (the baseline, recipients, reactors, the
  provider merge); `render.rs` (the header and recipients line).
- `unified_index`: `people.rs` ranking; the applet's `/people` against
  the fixture index.
- `tests/fixtures/ingested_tng_test.py`: the end-to-end guard, from the
  TNG fixture through the index: Picard known by one address from
  three sources, one number written two ways by three sources as one
  handle, WhatsApp's address book, Signal's number and ACI together, a
  Slack mention as a chip link, two cards' photos as the URLs the index
  serves.
- UI: `contacts.test.ts`, `resolver.test.ts` and
  `tests/chip_links.test.ts`; the render preview golden shows
  unresolved chips. `tests/e2e/contacts.spec.ts` runs on a root with the
  contacts app: it links Riker's Slack and email handles to one contact
  from two documents and checks the documents and an open grid follow.

## Not built

Editing a contact on its card, merge, groups and members, undo, the triage grid of
unresolved handles, a
handle for a number without its country code, a handle that stopped
working in some apps but not others, mentions in Google Chat,
WhatsApp and Messages (each marks them up, in a shape not yet checked
on real data), in Facebook (a tag is `@[<id>:2048:<Name>]` in a post,
not yet written as a handle) and in Beeper (no handle kind for its
users), and a handle for a Beeper (Matrix) user: [`plans/contact_linking.md`](plans/contact_linking.md),
[`plans/contact_editing.md`](plans/contact_editing.md) and
[`plans/search_autocomplete.md`](plans/search_autocomplete.md),
§"Order of work" in each.
