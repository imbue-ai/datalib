# Contacts: who a handle is

How Datalib knows that the `riker@enterprise.org` in a mail, the
`+1 202 555 0101` in a WhatsApp chat and the `U_RIKER` in a Slack
thread are the same person, and draws them as one chip.

Three things to know before the rest:

- **One shape for a person, made at render time.** Every source keeps
  its own raw records as it downloaded them. When it renders, it
  translates what it knows about each person into one shared shape,
  `NormalizedContact`. Nothing is stored in that shape on the download
  side.
- **Your own contacts use the same shape.** A contact you curate in the
  contacts app is served as a `NormalizedContact` too, so everything
  that reads a source's record of a person reads yours the same way.
- **Linking is after the fact, optional, and sits over the view.** No
  source and no render knows about your contacts. A document names a
  person only by a handle (an email address, a phone number, a Slack
  id). Linking handles to a contact happens later, in the contacts app,
  and changes how a chip is drawn: its name, photo and menu. Documents,
  the index and search are untouched by a link (see
  [Searching for a person](#searching-for-a-person)).

## The same move chat-common makes

Fifteen chat-like sources (email, Slack, Discord, WhatsApp, Signal, Messages,
the AI chats and more) each translate their raw rows into one neutral
shape, `NormalizedChat`, and chat-common turns that into the document
and its grid rows. People get the same treatment. A Slack profile, an
address-book card, a LinkedIn connection, a WhatsApp address-book entry
and the bare fact that someone wrote in a chat under some name all
describe a person; each is translated into `NormalizedContact`
(`datalib_contact_schema`): what one source knows about one person, in
terms no source owns. `contact-common` is chat-common's counterpart
for the sources that are *about* people: it turns a `NormalizedContact`
into a document.

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

How every chip is written, drawn, resolved and clicked is
[`chips.md`](chips.md); this page is what is particular to a person.
What is still to build is §"Not built" at the end.

## Words

| word | means |
|---|---|
| **handle** | one identifier in one namespace, normalized: `email:riker@enterprise.org`, `tel:+12025550101`, `slack:T01/U02`, `signal_aci:<uuid>`, `facebook:name/Will Riker`. `datalib_handle` makes them; the UI's `chipLinks.js` mirrors its rules by hand. |
| **render** | the pipeline's render step: it writes a source's documents (their markdown) and grid rows into its render store during a sync, and the grid index and the search index read them from there. "Renders again" means those stored documents are rewritten. |
| **draw** | what the UI does with a chip when it is shown: it asks who the handle is and paints the name, photo and mark. Nothing is stored. |
| **chip link** | how a document names a person: a markdown link whose href is the handle as a URI, `[Will Riker](mailto:riker@enterprise.org "Will Riker <riker@enterprise.org>")`. The viewer draws it as a chip. |
| **source contact** | a `NormalizedContact` a source wrote: that source's attempt at a neutral, unified record of what it knows about one person, keyed by `source_id` and the source's own `key`. The index stores them in `source_contacts`; the UI holds them as `Who.sourceContacts`. |
| **contact** | a person's own record in the contacts app, of a person or an organization they added, also served as a `NormalizedContact` (always `kind: person`), with `source_id = "datalib_contacts"`. |
| **link** | a row in the contacts app saying a handle belongs to a contact. Made by a person, or proposed by an agent for a person to confirm; no source or step makes one. |
| **label** | a word on a handle or a contact (`real`, `family`), in or out, with who said so. §"Agents propose, a person decides"; a contact's are in the Library tree (§"The contacts list"). |
| **the contacts app** | the `datalib_contacts` applet and its store under `datalib_curated/`. Optional: without it, chips still say what the individual sources know, and offer nothing to link. |

## Handles

The `datalib_handle` crate (`datalib/backend/handle`) is the one
definition, shared by the renders, the index, the applets and the
contacts store. A handle is `<kind>:<value>`. The rule that matters most: **where a native id is an
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
  watch.** `stopped_working_by` is on the link, one person's use of the
  handle, so a person who keeps
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
| `discord` | an account's id, the digits of a snowflake: one across every server | `Handle::discord` | `datalib:handle/discord/<id>` |
| `signal_aci` | a Signal account id, a lowercase dashed UUID | `Handle::signal_aci` | `datalib:handle/signal_aci/<uuid>` |
| `facebook` | `name/<the name shown>`, its whitespace collapsed; or `deleted/<conversation id>` | `Handle::facebook_name`, `Handle::facebook_deleted` | `datalib:handle/facebook/<value>`, percent-encoded but for its `/`s |
| `google` | `name/<the name shown>`, its whitespace collapsed | `Handle::google_name` | `datalib:handle/google/<value>`, percent-encoded but for its `/`s |
| `linkedin` | the slug of a profile URL (`linkedin.com/in/<slug>`), percent-decoded and lowercased | `Handle::linkedin`, `Handle::linkedin_profile_url` (any `linkedin.com` host; a trailing `/`, query or fragment is dropped) | `https://www.linkedin.com/in/<slug>`, the profile itself |
| `github` | a login, lowercased (an app's ends in `[bot]`) | `Handle::github` | `datalib:handle/github/<login>` |
| `gitlab` | `<host>/<username>`, both lowercased: a self-hosted GitLab is a namespace of its own | `Handle::gitlab` | `datalib:handle/gitlab/<host>/<username>` |
| `notion` | a Notion user id, a lowercase dashed UUID, for a user Notion gives no address for | `Handle::notion_user` | `datalib:handle/notion/<id>` |
| `whatsapp` | the digits of a linked id (`…@lid`) the backup maps to no number | `Handle::whatsapp_lid` | `datalib:handle/whatsapp/<digits>` |
| `me` | `self`: the person whose library this is, wherever a source shows them under a stand-in | `Handle::me` | `datalib:handle/me/self` |
| `ai_model` | a model's id, lowercased (`claude-opus-4-7`) | `Handle::ai_model` | `datalib:handle/ai_model/<id>` |

A Facebook export names people and never numbers them, so a
`facebook:name/` handle is a name: two people of one name are one
handle, and nothing in the export can tell them apart. An account
deleted since is written `Facebook user`, or with no name at all, and
gets a handle only in a Messenger conversation that lists it as its one
deleted account: `facebook:deleted/<that conversation's id>`. The same
deleted person in two conversations is two handles; several in one
group are none. A `google:name/` handle is a name for the same reason: a
comment on a Drive file names its author and nothing else.

No handle holds a character that changes how it shows without showing
itself: Unicode's format characters (`Cf`), such as a bidi control
(U+202E), which reorders the text around it, or a zero-width space
(U+200B), which makes two handles that look alike different handles. A
`facebook` or `google` name may still hold a joiner (U+200C, U+200D),
which some scripts and emoji need between two letters.

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
given. Google sends its own full reading of some numbers
(`canonicalForm`), and one typed without its country code takes its
handle from that where there is one.

**What any other such number may be is matched, never decided.** Nothing
sets a country, for a library or a source. The number's digits, less
what is dialled before a number and is no part of it (`home_digits`: any
leading zero, the 1 before ten digits), are compared with every full
number a source knows, and match one that is those digits with its
country code in front (`Handle::dialled_at_home`, which knows how long
each country code is and nothing else about it). Where that finds one
number, adding the person to contacts takes it; where it finds two, in
two countries, both are offered and neither is ticked. It stays a match:
it makes no handle, names no chip and is in no stored record. Three
places use it:

- **`POST /people`** answers the cards that write a number asked about
  this way (`same_digits`), and the full numbers each such number may be
  (`full_numbers`). The index keeps a record's numbers for it in
  `contact_record_numbers`, read by their last seven digits.
- **Search**: a person key takes a number however it is typed, and
  contact-common writes a card's such numbers as `about` terms, by their
  digits, so `with:+12025550101` finds a card that says `(202) 555-0101`
  ([`searching.md`](../user/searching.md) §"People").
- **The person's page** and adding many people at once (§"In the UI").

A text thread whose other side the phone knew only by such a number
still has no handle for them.

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

Every rule has a second copy to keep in step: `handleFromUri` in
`ui/src/cards/chipLinks.js`, tested over the same cases in
`tests/chip_links.test.ts`.

**A new kind is not a rules change**: no stored handle reads
differently, so the version stays. It touches these places. The
compiler finds the Rust `match`es and `KIND_ICON` (once the TypeScript
union has the kind); nothing finds the rest:

| where | what |
|---|---|
| `handle/src/lib.rs` | the `HandleKind` variant and its constructor; an arm each in `rebuild`, `to_uri` and `describe`; a branch in `from_uri` unless the URI is `datalib:handle/…`; a row in `every_kind_round_trips_through_its_uri` |
| `contact_schema/src/lib.rs` | `Medium::of_kind` |
| `applets/src/unified_grid_and_search_index/columns.rs` | `handle_mark`, the mark the grid's Author chip shows |
| `ui/src/cards/chipLinks.js` | `KINDS`, `handleFromUri`, `uriFromHandle`, and a row in `tests/chip_links.test.ts` |
| `ui/src/cards/contacts.ts` | the `HandleKind` union, `handleKind`, `KIND_ICON`, and `copyText` if `describe` is not `label (handle)` |
| `ui/src/cards/sanitize.ts` | `ALLOWED_URI_REGEXP`, if the URI's scheme is a new one |
| `tests/fixtures/ingested_tng_test.py` | a fixture person reached by the new kind, so the index is seen to know them |

### Who writes a handle

A provider's render decides, at normalize time, which identifier it
has for a person; `NormalizedChatItem::author_handle`,
`Recipient::handle`, `NormalizedChatItem::mentions` and
`NormalizedReaction::reactor_handle` carry it.
What each source writes:

| source | author | more |
|---|---|---|
| email | the From address | To, Cc and Bcc (the sender's copy) as recipients; an `@` or `+` mention, a `mailto:` link in the fresh part of an HTML body |
| Slack | `slack:<team>/<user>` | a `<@U…>` mention in a body; each reaction's user; the profile's email, in the source contact |
| Discord | `discord:<user id>`; a webhook's author has none | a `<@…>` mention in a body; the account's own reactions, since Discord names nobody else's; the profile's names and avatar, in the source contact |
| WhatsApp | the sender's number, a linked id (`…@lid`) through `jid_map`, else the linked id itself (`whatsapp:<digits>`, labelled "WhatsApp user …" and its last four digits when nothing names it) | each reaction's sender |
| Signal | the number, else the account id (ACI); a recipient known by PNI alone has none | a mention (a `mentionAci` body range), by number else ACI, written `@Name` in place of its `U+FFFC`; number and ACI together, in the source contact; one known by ACI alone reads as the dashed ACI. The ACI is read by its one path in the stored frame, and one that will not read is a problem on the recipient (`aci`, `CoercionFailed`), not a silent loss |
| Messages | the number or Apple ID address | each tapback's |
| Google Chat and Voice | the address or number | |
| SMS backup | the number; an MMS by the sender its `addrs` name (`type` 137), a group MMS's member included and the account's own number on one it sent, still shown as "Me"; a call by the other side's number | the number or numbers a text the account sent went to, as recipients; every number the account sent an MMS from (its `addrs` entry of `type` 137), as the account, tied to `me:self` |
| address books | | a card's numbers and addresses, in the source contact |
| LinkedIn | a message's sender, by `SENDER PROFILE URL`; the account's owner, by their primary address, on every post and comment | a connection's profile and email address, in the source contact |
| Google Drive | a comment's author, by name; the account's own comments by its address, where the file names the account | a person chip in a Doc, by address |
| Facebook | a Messenger message's sender, by name; a deleted account by its conversation, where it is that conversation's only one; the account's owner, by name, on every post, album, comment, reaction and edit (a store with no profile knows them only as "Me", and gives none) | each Messenger reaction's; a friend's name, in the source contact |
| Beeper | a Slack user by `slack:<team>/<user>`, read out of the bridge's Matrix id (the owner by the Slack user the room was bridged for); anyone else by a number or address the app lists for them (`participant_identifiers`); a Matrix id alone has no kind | each reaction's; the person's numbers, addresses and cached avatar, in the source contact |
| calendar | an event's organizer, by address | each guest with an address, as a chip on the event's page |
| GitHub, GitLab | a change request's or a comment's author, by login (GitLab's on the host the merge request's address is on) | |
| Notion | a comment's author, and a page's last editor, by the address the users table gives, else by user id | a user mentioned in a page |
| Todoist | a task's adder and a comment's author, by the address the account or its collaborators list gives | a task's assignee, as a chip on its line |
| Baby Connect | whoever logged an entry, by the address the account's users list gives | |

An AI chat's answers (Claude, ChatGPT, Claude Code, Codex) are by their
model, `ai_model:<id>`, where the record names one.
Where a source shows the owner's side under a stand-in (WhatsApp,
Signal, Messages, SMS backup, Google Voice and the AI chats, as
`AuthorKind::Account`), chat-common writes it "Me", by `me:self` where
the source has no handle of its own for them: one chip for the owner
across every source. `me:self` is not among a conversation's
participants, since the owner is in every conversation they wrote in,
so `with:me:self`, like `from:me:self`, finds the messages they wrote. A
phone's backup (WhatsApp, Signal, Messages, SMS backup, Google Voice's
texts and voicemails) records no recipient for a message it received,
so each one someone else sent names `me:self` as its To recipient
(`Recipient::the_account`, not drawn): `to:me` finds it, and it is no
participant either. Where a
source has no stand-in (email, Slack, Google Chat), the owner's own
messages carry their address or user id like anyone else's, and the
source marks its record of the owner as the account
(`NormalizedContact::account`). `from:me` reaches both (§"Searching
for a person").

## In a document

chat-common writes an author with a handle as a chip link in the
message header, and recipients as a line straight under it, leaving
off those the conversation already names (an SMS thread's other side);
the shape is in [the document contract](../user/rendered_documents.md#a-person).
A reactor with a handle is a chip link too, in a
message's reaction list and in the list of reactions to messages not
in the mirror (`render.rs::reactor`). Slack writes a `<@U…>` mention as
a chip link in the body (`slack_render/src/render/mrkdwn.rs`), and as
plain `@Name` inside code, which shows what it holds; an email's
`@` mention is a chip link too (`email_render/src/render/mentions.rs`).
A contact's own page leads with the person as a chip, so adding them is
a double-click away: by a handle of theirs, else by the page itself
(`datalib:contact_page/<page>`, `contact_schema::contact_page_uri`),
for a card with no address and no full number. A group of contacts lists its members, each a chip link where the
member's card has an address or a full number (contact-common, from
`ContactDoc::member_handles`).
Every mention a source marks up is also a `mention` search term on its
message's row. Every chip link
the backend writes is shaped by one function,
`datalib_etl_render::message::chip_link`.

The viewer draws a chip for any explicit link whose href is a handle,
whoever wrote it, so a `mailto:` link in an email's HTML body is drawn
as a chip too. That is safe because a chip shows who the href resolves
to, never the link text ([`chips.md`](chips.md) §"Trust").

## Source contacts: `NormalizedContact`

`datalib_contact_schema` (`datalib/backend/contact_schema`) is only
the shape: `source_id` and `key` say who describes the person;
then `kind` (`person` or `group`), `names` (the preferred one first),
`handles` (each a `ContactHandle`: its `medium`, the value as written,
its `Handle` if one could be made, a label, and `stopped_working_by`),
`photo` (the image in a blob store, or a URL only a fetch could
follow), `photo_url` (where the app serves it; see below), `org`, `title`, `note`,
`details`, `groups`, `members`, `source_url` and the record's own
stamps. How much the person wrote is not on the record: the grid's rows
say it (§"How much someone wrote").

Four producers, each needing less of a provider than the one before:

- **The contacts app**, from its store, with `source_id =
  "datalib_contacts"`.
- **A source about people**, through `contact-common`: an address-book
  card, a LinkedIn connection, a Facebook friend. Each
  is a document of its own, and carries its source contact.
- **A chat provider, for what only it knows**, in
  `NormalizedChat::contacts`: Slack's profiles (names, title, avatar,
  the email), WhatsApp's address book (`JidNames::contact`), Signal's
  recipients (number and ACI together), Beeper's participants (names,
  numbers and addresses, the avatar the app cached). These are rows, not documents.
- **chat-common, for free** (`chat-common/src/people.rs`): a baseline
  per handle per document from what the provider showed: each author
  with the names it wrote under, the most used first; each recipient
  and each reactor under the name shown. A provider's own source
  contact for the same handle replaces the baseline and keeps every
  name it saw (`document_contacts`).

Every `RenderedMarkdown` carries its `contacts`. The render store holds
them in two tables: `source_contacts` (`markdown_uuid`, `contact_key`,
`source_id`, `name`, and the whole record as `contact_json`) and `source_contact_handles` (`markdown_uuid`,
`contact_key`, `handle`).

**The index holds each distinct record once**
(`etl/render/src/grid_index/contact_records.rs`). A person is in as many
documents as they wrote or were named in, and their record is the same
in all of them. So the index keeps the record in `contact_records`
(`record_id`, `source_id`, `contact_key`, `name`, `contact_json`), its
handles in `contact_record_handles` (`record_id`, `handle`), its numbers
with no country code in `contact_record_numbers` (`record_id`,
`digits`), and a row per document in `document_contacts`
(`markdown_uuid`, `record_id`, and `page`: whether the document is a
contact's own page, such as an address book's card). In a library of 309,481 rows there were 2,081 distinct
records. A re-render replaces its document's `document_contacts` rows,
and a record no document carries is deleted once at the end of each
run of `unified_grid_and_search_index` that committed anything, in a
commit of its own (`refresh_people` in `etl/render/src/grid_index.rs`,
which calls `contact_records::sweep`).

These tables are a part of the grid index's file of their own
(`grid_index::Part::Contacts`), with a cursor per source
(`contact_cursors`, written beside the grid's). A build that changes
them empties only them, and the step's next run fills them from each
source's `source_contacts` alone, a page of documents at a time, without
reading or writing the rest of the grid. A build that changes the grid's
own tables empties these with it.

**`POST /people` on the `unified_grid_and_search_index` applet** takes
`{"handles": […], "pages": […]}` and answers `{"people": {<handle>:
[NormalizedContact, …]}, "pages": {<page>: NormalizedContact},
"same_digits": …, "full_numbers": …}` (the last two are §"A number
without its country code"). `pages` are contacts' own pages, by id, each
answered with the contact it is about. `people` is, for
each handle, every source contact holding it, one per source and key,
each with `seen` (`{items, last_at}`) where its handles wrote anything
in that source, ranked by
`unified_grid_and_search_index/src/people.rs`: a source about people (a
record a contact's own page carries) first, then the one where the
person wrote the most. A handle no source mentions is absent. This
works with no contacts app at all.

### How much someone wrote

**One count: the grid's rows a person wrote, which is what `from:`
finds.** A row's author is its `author_handle`, and
`grid_rows_by_author` (`author_handle`, `source_id`, `markdown_uuid`,
`created_at_utc`, `author`) answers how many rows a handle wrote in a
source or a document, and the newest, from the index alone. Every
place that says how much someone wrote reads it: a source contact's
`seen` in `/people` (that source's rows by the record's handles), the
person page's *Seen* line and a chip's "· 4 items", and the order of a
conversation's people (their rows in that conversation), and the
contacts list's *Items* and *Last* (§"The contacts list"). A reaction, a
comment and an edit's version are rows, so they count, as `from:` finds
them. No source counts on its own: a render once kept its own tally of
messages per document, which left out reactions and edits and
disagreed with `from:` for 27 of the 119 records in the TNG fixture.

### Photos

`photo` is what the source gave: an image its blob store holds
(`Photo::Stored`: the step whose store it is, the blake3 of the bytes
and the type the store holds them as) or a URL nothing fetched
(`Photo::Url`, a Slack avatar). Nothing writes a photo beside a page.
`photo_url` is what a chip draws: a path on the app's own origin, never
another host's, since the app fetches nothing remote unasked, and only
for an image a browser draws: png, jpeg, gif or webp
(`contact_schema::DRAWABLE_PHOTO_TYPES`, the one list, which the
contacts app's photo rule shares). For a stored photo it is the photo's
blob-store address, `/data/<group>/<function>/blob/<blake3>`, which
`datalib-http` serves (`datalib/backend/blob_store/README.md`);
`datalib_etl_render::photo` makes it, for contact-common's one person a
page is about and for each of a chat's source contacts. A stored photo
of another type is still shown on its contact's page, at the same
address, and gets no URL.

Where the bytes come from: Beeper's cached avatars are in the source's
blob store already (an avatar counts only when the store holds it as an
image). A vCard carries its photo inside
the card: the contacts download decodes each inline `PHOTO` into the
source's blob store before the card is stored, and the source's
`--prepare` does the same for every card a store already holds. Render
hashes the card's decoded photo and links it. A Google contact's photo
is a link, which the download fetches into the blob store
(`contacts/INGEST.md`). A photo the blob store
lacks leaves the contact with no photo and a `blob_missing` problem on
its page.

For a photo a person put on their own contact, the contacts app answers
`/applet/datalib_contacts/photo/<contact_id>?v=<when it was set>`: a
page draws an image it has already loaded under one URL from memory, so
a changed photo needs a new URL to show without a reload. With no
photo, or one the browser fails to load, a chip linked to a contact
draws the person's initial, and any other chip draws the mark for its
handle's kind.

## The contacts app

`<data_root>/datalib_curated/datalib_contacts/contacts.doltlite_db`,
written only by the `datalib_contacts` applet (the store is
`datalib/backend/contacts`, the routes
`datalib/backend/applets/src/datalib_contacts.rs`). It is a person's
work, and nothing can rebuild it:

- **Every write is a commit** on `datalib_writer`, sealed onto `main`,
  so its commit log is an audit trail.
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
| `contacts` | `contact_id`, a random v4 (the one id here that is a person's act, not a function of a record) | `name`, `note`, `merged_into`, `agent` (the agent that made it, if one did), stamps |
| `handles` | `(handle, contact_id)`, the handle as `datalib_handle` spells it | `linked_how` (`manual` or `agent`), `linked_at_utc`, `stopped_working_by`, `agent`, `reason`, `confidence`, `run`, `reviewed_at_utc`; `label` ("work") and `copied_from_source` and `_key`, the card it was added from |
| `not_linked` | `(handle, contact_id)` | a handle a person took off a contact: `contact_name`, the `agent` and `reason` that proposed it where an agent did, `decided_at_utc`, `contact_rejected` (the agent's whole contact was rejected) |
| `labels` | `(subject, label)`, the subject a handle or `contact:<id>` | `applies` (in or out), `decided_how`, `agent`, `reason`, `confidence`, `run`, `decided_at_utc`, `reviewed_at_utc` |
| `photos` | `contact_id` | `content_type`, `bytes`, `set_at_utc` |
| `fields` | `field_id`, minted by the card | `contact_id`, `kind` (`FieldKind`), `label`, `value`, `handle`, `position`, `copied_from_source` and `_key`: everything a contact says that is not a handle |
| `me` | `contact_id` | `marked_at_utc`: the person said this contact is them |

Each table is a row struct in `contacts/src/schema.rs`, and its DDL
and upsert are derived from it (`#[derive(PortableTable)]`).

Two rules the tables encode: **a handle is linked to a contact by a
person's say-so**, so one another contact has is refused until the
person says whether both have it (an address two people share is
linked to each) or it moves, which takes it off the others as not
theirs; and **keys are handles, never `grid_rows.uuid`**, which moves
when a recipe changes. Rung 3 of `datalib_contacts::LADDER` rekeyed
`handles` from the handle alone to the handle and the contact. Rung 4
made a group an ordinary contact and what it grouped a label: each
member got the group's name as a label of their own, then `members` and
`contacts.kind` went (`groups_become_labels`).
`stopped_working_by` is a date *by* which the handle had stopped
working: an old number still names its owner in every message from
when it worked, so the link stays and the chip marks it as old. "By",
not "on": marking a handle fills in today, which is always true when
nothing better is known, and the person narrows it if they remember. It
is a partial date (`2019`, `2019-06`, `2019-06-14`), a date a person
remembers rather than an instant anything measured, so it is not an
`_at_utc` stamp. A stop is one person's: an address one of two people
stopped using may still be the other's. A number *reassigned* to
someone else is not covered: a link has no dates, so a handle's owners
are its owners for all time. Nor is a number that stopped working in
some apps and not others: a stop is on the link, and `tel:` is one
handle across every app (§"Handles").

The routes, each behind the gateway's secret
([`applets.md`](applets.md)). A refusal the store explains (a handle
someone else holds, a contact with no name, a bad date, not a photo)
is a 409 with the store's words; a handle that does not parse is a
400, and a contact or photo that is not there a 404. `GET /` answers the
list below as JSON, each with its `kind`, and `datalib-applet
datalib_contacts --help` prints it: `read` changes nothing, `propose` is
an agent's way to write, and `person` is the rest, recorded as the
person's own decision whoever called it, since the store cannot tell who
is on the other end of the token. A test checks this table against
the router. [`contacts_app.md`](../user/contacts_app.md) is the same ground
for an agent, as steps:

| route | does |
|---|---|
| `GET /` | every route below, with its method, `kind` and what it does, and how a handle is spelled |
| `GET /health` | `{ok: true}` |
| `POST /resolve` | `{handles}` → `{resolved: {<handle>: [NormalizedContact]}}`, the contacts each handle is on, by name; a handle nobody holds is absent |
| `GET /search?q=` | contacts whose name contains `q`, for the person card's *Add to contacts* |
| `GET /contacts` | every contact, by name, as a typed table (`columns`, `rows`): the contact as its own chip (`datalib:contact/<id>`), never by a handle, which another contact may share; the first address and number; its labels but `real`; how many handles; the note; when it changed. What the contacts app's own table shows |
| `GET /labels` | `{labels: [{label, contacts}]}`: every label your contacts carry but `real`, by label, and how many carry each; what the Library tree's *Contacts* lists |
| `POST /contacts` | create, with `name` and the `handles` to link at once |
| `POST /contacts/add` | `{people: [{name, handles, held?, said?}]}`, in one commit (`add_people.rs`): each a new contact with every handle, unless a contact has one of them; then, when it has their name (case aside), they are that contact, which gets the rest, less any taken off it as not theirs, and when it has another name they are skipped, since it may be an address two people share and only the person can say. `held`, `{<handle>: "share" \| "move"}`, names handles other contacts have that are theirs too or theirs alone; those never make them one of those contacts. `said`, `{<handle>: {label?, copied_from?}}`, is what the card each handle came from said of it, its label and the card as `{source_id, key}`, and the handle keeps both. A person with no handle is already there when a contact has their name. Answers `{created, added_to, already, skipped: [{what, why}]}`; skipped are those with no name, those whose handles are on a contact of another name, and those whose handles are on several contacts |
| `GET /contact/{id}` | one contact, working handles first; a merged-away id answers as its survivor |
| `GET /contact/{id}/labels` | `{labels}`: the contact's labels but `real`, which `POST /label` sets with the subject `contact:<id>`; a merged-away id answers its survivor's |
| `DELETE /contact/{id}` | delete the contact and every contact merged into it: their links, fields, photo, drafts and memberships. Its handles are nobody's afterwards; the commit before holds it all |
| `POST /merge` | `{from, into}`: `into` survives and takes `from`'s links, fields and labels, its photo and note where it has none (two notes are kept one after the other); `from` stays as a row whose `merged_into` names `into`, so a `contact:<id>` saved anywhere still reaches the person, and anything merged into `from` now points at `into` |
| `POST /link`, `POST /unlink` | `{handle, contact_id, held?, label?, copied_from?}` / `{handle, contact_id}`: one handle to or from a contact, as the person's own act; an agent proposes instead. A handle another contact has is refused (409, naming who) unless `held` is `share`, so both have it, or `move`, which unlinks it from the others. `label` and `copied_from` are what the card it came from said; a handle linked already keeps its own. Unlinking is written down in `not_linked`; anyone else who has the handle keeps it |
| `POST /handle_label` | `{handle, contact_id, label}`: the contact's label for one of its handles; `label: null` clears it |
| `POST /stopped_working` | `{handle, contact_id, by}`: by when it stopped working for that contact; `by: null` means it works again |
| `POST /rename` | `{contact_id, name}`, at once, without a draft |
| `POST /me` | `{contact_id, me}`: say the contact is you, or stop; answers `{changed}`. A merged-away id marks its survivor |
| `GET`, `PUT`, `DELETE /photo/{id}` | the photo as bytes; put one (the body, with its `Content-Type`: png, jpeg, gif or webp, at most 4 MB; the route reads up to the gateway's 8 MB so the store's rule is the one that answers), answering its new `photo_url`; drop it |
| `GET /contact/{id}/edit` | what the card edits, as published: name, note, fields |
| `POST`, `GET /contact/{id}/draft` | open the contact's draft (cut it if there is none) / read it: `{base, mine, published, published_commit}` |
| `PUT`, `DELETE /contact/{id}/draft` | autosave the whole edit onto the draft, uncommitted, answering `{handles: {<field_id>: <handle>}}` for each field whose value is an address or number / discard the draft |
| `POST /contact/{id}/draft/save` | `{seen, held?}`, the published commit the card last showed: publishes the draft, and each email and number in its fields becomes a handle of the contact. `{"outcome": "saved", "commit"}`; `{"outcome": "stale", "view"}` when the contact moved since; or `{"outcome": "held", "held": [{handle, by}]}` for a handle other contacts have (`by` their names) that `held`, `{<handle>: "share" \| "move"}`, says nothing of. Only `saved` saved anything |
| `POST /proposals` | an agent's batch, one commit: `{agent, run?, contacts: [{contact_id?, name?, handles, reason?, confidence?, labels}], handles: [{handle, labels}]}`, each label `{label, applies?, reason?, confidence?}`, a confidence 0 to 1. Answers `{contacts_created, linked, labelled, skipped: [{what, why}]}` |
| `GET /review?label=` | the review as a typed table, one row per contact or handle with anything an agent proposed or a label, with a `membership` column for `real` and for `label` if given; `GET /review/items` the same as plain records, for an agent |
| `GET /suggestions` | `{suggestions}`: what agents suggested that waits for the person, least sure first. Each is a new contact (`new_contact`), handles for one of yours (`handles`, beside what it `held` already) or labels, with its `agents`, `runs`, `reasons` and `confidence`, and the `subject` that `/review/confirm` (Yes) and `/review/reject` (No) take. What the contacts card's suggestions show |
| `POST /review/confirm`, `POST /review/reject` | `{subjects}`, each a handle or `contact:<id>`: agree with, or turn down, everything an agent proposed for each |
| `POST /label` | `{subjects, label, applies}`: a person's in (`true`), out (`false`) or unknown (`null`) |
| `GET /not_linked` | every handle a person took off a contact, newest first |

The config entry is `[[applets]] id = "datalib_contacts"` with
`command = "datalib-applet datalib_contacts"`, in the
`unified_grid_and_search_index` group; the gateway passes the data root
in the environment. A new root's starter config has it; an older config
adds it by hand.

### Fields, and editing a contact through a draft

A contact is a list of lines. A **handle** on it (a row of `handles`)
is one line, with the person's label ("work", "cell") and the card it
was copied from, where it was; an address two people share is a line of
each. Every other line is a **field** (a title, a street address, a
birthday, a number written without its country code), which links
nothing. An email or a number typed into the card is a field only
until the save, which makes it a handle (below); a field's `handle` is
its value as a handle meanwhile, so the card can say who else has it.
A field with a `handle` that an older build saved stays a field until
the contact's next save.

The card edits a contact's name, note and fields through a **draft**
(`contacts/src/drafts.rs`, over `datalib_etl::draft`): a branch
`draft/<contact_id>` where each autosave is an uncommitted write, which
no reader sees. A save publishes it as one commit, the draft winning
only the cells it changed, so a link or a rename made meanwhile
survives. The save names the published commit the card last showed; if
this contact's name, note or fields have moved since, nothing is saved
and the card gets the new state to show. In the same commit, after the
merge, each email and number among the fields becomes a handle of the
contact, keeping the field's label and card, and stops being a field
(`draft::Finish`); one already theirs takes the label where it had
none. One another contact has needs the person to say whether both have
it or it moves here: the save without that answer saves nothing and
names who has it. Links and photos change at once, never through a
draft: a handle's label (`POST /handle_label`) and its removal, which is
*Not theirs*, take effect as they are made, and Discard does not undo
them.

On the person card, under *Your contact*, the card lists the contact's
handles as chips with their labels, and `ContactEditor.ce.vue` is the
rest: its fields and note as published, *Edit*, and the photo, which changes at once:
*Set photo…*, *Remove photo*, or a drop on it of an image file or of an
image dragged from the app itself, such as a source's photo on the same
card (`photoDrop` in `contactEdit.ts`). An image dragged from another
site is refused rather than fetched. *Edit* opens the draft (`POST
…/draft`) and shows, in place of the chips, the name, a line per handle
(its kind, its address, a label and ×), the fields (a kind, a value and a free-text label each)
and the note as a form. Each pause in typing, and each time a field
loses focus, autosaves the whole edit (`PUT …/draft`), less any field
still empty; *Save* and *Discard* end it. A card opened on a contact
that has a draft resumes it, in any window, after a reload or a
restart. When it opens the draft, on each `curated` frame, and on a
save refused as stale, the card compares what the draft was cut from,
the form and what is published (`merge3` in `contactEdit.ts`, the same
three-way merge the save makes): a change published elsewhere to
something the draft left alone is taken into the form, and a part both
changed differently is marked "changed elsewhere" with *Use theirs* and
*Keep mine*. After a stale save, saving again keeps what the form says.

The card also sets the contact's lines beside the rest of what it
knows (`contactFields.ts`):

- **A typed address or number another contact has says so.** It shows
  that contact's chip, which also catches a typo, with *Both* and *Move
  it here*; the save sends the answer. The handle comes from the store,
  which answers each field's handle on every autosave, so the card
  never repeats the handle rules.
- **A source's section copies into the contact.** Beside each thing a
  source says that a contact can hold (its name, each address and
  number, title, organization, its details, note) is *Use*, which puts
  it in the draft, opening the draft if it is not open; a value the
  contact already holds, written however, offers none. A copied field
  records the source and record it came from (`copied_from`), and
  typing over it makes it the person's own; a copied address or number
  takes both to its handle at the save.
- **A copy does not follow its source.** Where the source a field was
  copied from now says something else, the field says so ("… now says
  *X*", with *Take it*), or that the source no longer says it.

Adding someone from their page (*Add to contacts*), adding a card's
other handles to your contact, and adding the rows of an address book
each keep the card's label of every handle and the card itself
(`HandleFrom`; `said` on `POST /contacts/add`). A card's other details
stay on the card until you *Use* one.

Not built yet: history and undo, and the export.

### Agents propose, a person decides

An agent may curate the contacts app too: group handles into contacts,
and label people and senders. Nothing it says is final, and the store
writes down where a person agreed or did not, so a later agent can
learn from it (`contacts/src/review.rs`). The label rules themselves
(`contacts/src/labels.rs`) know nothing of contacts: the contacts app
is where the pattern is proved before other parts of Datalib label
things the same way.

- **Who said so is on every row.** A link, a contact and a label each
  name the `agent` that made them, its `reason`, how sure it was
  (`confidence`, 0 to 1) and which of its runs it was (`run`); a
  person's own say is `manual` and names none. `reviewed_at_utc` is when
  a person looked: an agent's link or label without one waits for
  review.
- **Until a person says Yes, an agent's link is a suggestion and
  nobody's.** A link counts when it is `manual` or has a
  `reviewed_at_utc`; one that does not is left out of everything that
  reads the store as the person's contacts: `POST /resolve` (so chips
  stay grey), a contact's handles and the list, the search's
  `contact:<id>` and `me`, the contacts list's counts, and who
  *Unfiled* counts as filed. A contact an agent made with no link that
  counts and one that waits is a suggestion too, and is not listed or
  found by name. `Store::suggestions` (`GET /suggestions`) is what
  waits, one per subject.
- **A suggestion holds nothing.** A person linking a handle that an
  agent only suggested for someone else is not asked about sharing it:
  the suggestion loses it, written down in `not_linked` as if the person
  had said No, and an agent's contact left with no handle goes. Linking
  it to the very contact it was suggested for is a Yes to that link
  (`settle_suggestions` in `contacts/src/lib.rs`). An agent still never
  proposes a handle any contact has, a suggestion included.
- **A label is in, out, or unknown.** A row says in or out
  (`applies`); no row says nobody has decided, which is most of them.
  A label is any word (`labels::normalize_label` writes it lowercase,
  its spaces `-`), and each stands alone: what one says never changes
  another. A person gives a contact theirs on its page. The one agents
  curate is `real` (`REAL`): in when
  the person or sender is not spam, which takes in a machine the person
  wants (a receipt, a notification) as well as the people they deal
  with. An agent's close calls are what its `confidence` is for: a
  person reviews the least sure first.
- **Confirm keeps the agent's row; reject turns it around.** Confirming
  stamps `reviewed_at_utc`. Rejecting unlinks each link the agent made,
  into `not_linked`, and flips each label, so its in becomes the
  person's out; the row stays `manual` and still names the agent, which
  said the opposite. A contact an agent made that is left holding no
  handle goes with its links. Taking a handle off a contact anywhere else
  (*Not theirs*, on the person card) is written down the same way,
  whoever put it there, so no agent puts it back; the person linking it
  again deletes that `not_linked` row.
- **A person's word stands.** An agent's batch skips, and names in its
  answer, a handle someone else holds, a pair in `not_linked`, a label
  a person answered the other way; an
  agent may re-propose over its own rows nobody has reviewed yet. A
  new contact repeating every handle of an agent's contact the person
  rejected whole is skipped too (`contact_rejected`); fewer of them is
  a different claim and may be made.
- **Merges and deletes carry them.** A merge moves the merged-away
  contact's labels and turn-downs to the survivor, the survivor's own
  answer winning where both have one; a delete takes the contact's labels and keeps its
  turn-downs, which name it as it was called.

The person's side is the contacts card's suggestions (§"The contacts
list") and the person's page, which shows each suggestion about them
(`suggestions.ts::suggestionsAbout`) with *Yes* (`/review/confirm`) and
*No* (`/review/reject`). A person says a sender is not real from
*Unfiled*'s right-click, *Hide this sender* (`POST /label`). Labels sit
over the view like links: no document renders again, and the search
does not read them yet.

## In the UI

Everything is in `datalib/ui/src/cards/`:

- `chipLinks.js` is the markdown-it plugin that marks a link whose
  href `handleFromUri` reads as a person's chip ([`chips.md`](chips.md)
  §"Drawing a chip"). Plain JavaScript, so the render preview runs it
  too. It mirrors `to_uri` and `from_uri` over the same test cases.
- `contacts.ts` holds the pure rules, unit-tested in `contacts.test.ts`:
  `chipLook` (what a chip shows, from the contact if there is one,
  else the best-ranked source contact, else what the source showed; an
  address several contacts share is one chip named for them, "Robert
  Picard & Marie Picard" or "Robert Picard & 2 others", led by its
  kind's mark rather than anyone's photo), `chipTooltip` (the chip's
  title: who, everyone who shares it, the identifier, each source's
  record and the person's other handles), `chipMenu` (open the card; copy
  the name, the identifier or both; find everything with the person; add
  them to your contacts), `copyText` and the copy rewrite. `Who.mine`
  is the list of your contacts a handle is on, by name, and the chip,
  its tooltip and its menu all read that list. *Everything with* on a
  shared address searches the address itself, since none of them is
  everyone who used it. `people` is who each handle
  is, for the whole app: the resolver ([`chips.md`](chips.md)
  §"Resolving") over `/people` and the contacts app's `/resolve`; an
  edit made in the page drops the resolver's answers for the handles it
  touched (`forget`). An edit made
  anywhere else (another window, an agent) reaches every page as the
  live stream's `curated` frame, on which `people` asks again. After
  every write it forwards to an applet, the gateway sends
  `datalib-http`'s watch a message on its channel (`watch::Nudge`), and
  the watch sends the frame when a `datalib_curated/` store's head moved (`http/src/watch.rs`); a draft's autosave is no
  commit, so it sends none. `decorateHandles` draws a document's chips
  and `chipCell` a grid cell's.
- `ChatBody.ce.vue` runs the decorate pass over a document's frame and
  owns the right-click menu (`ChipMenu.ce.vue`). `chip.css` is the one
  look.
- `PersonCard.ce.vue` is the card a person chip's double-click opens
  (`personView`; its rules are `person.ts`). It is about a handle, one
  of your contacts by its id, with or without a handle, or whoever a
  card with no handle describes (`Subject`). Its heading is the
  person's chip; then your contact when they are one,
  which the card edits (§"Fields, and editing a contact through a
  draft"), with *This is me* to mark the contact as you (§"Searching
  for a person"); then each source's record that holds one of the card's handles, a
  section per source and never merged, the source the chip was seen in
  first, each saying which address, number or login it was found
  *via* (an id a service minted is left to the record's own chip). Every
  handle on it is a chip showing the handle (`identifierCell`): an
  address, a number or a login as written, and an id a service minted
  (Slack's `T…/U…`, a Discord or WhatsApp number, a Signal or Notion
  id, Facebook's and Google's `name/…`) as the name that service gives
  them, beside its mark, with the id in the tooltip and the copied
  text (`identifierText`). It is pale blue
  when it is on a contact and grey when not, with a chip's double-click
  and right-click. The menu of a handle on your contact also has *Not
  theirs* and *No longer works* (whose date is then a box beside the
  handle), both about this contact alone; of one on nobody's, *Add to
  <your contact>*; and of one someone else has, *Add to <your contact>
  too* or *Move from <them> to <your contact>*
  (`person.ts::handleMenu`). A handle of your contact that others have
  too says *shared with* them. Opened by an address several contacts
  share, the card is the address's own: its heading is the shared chip,
  and each of them is a contact chip where *Add to contacts* would be. A source holding handles that are on nobody
  offers *Add its other handles to <your contact>* (one link per handle;
  one someone else holds is left alone and says whose it is, and one
  taken off this contact as not theirs is not offered). Someone not in
  your contacts is offered whole: **the handles adding them takes are
  listed, ticked** (`person.ts::offers`), the one opened and then every
  other on any record that holds it, one hop and no further, since a
  record is one source's word that its handles are one person and two
  records are not. A handle another contact has is offered unticked,
  saying whose it is; ticked, it asks *Both* (the default) or *Move it
  here*. A number with no country code is offered as the full number it
  may be.
  *Add to contacts* takes the ticked ones under a name, as do the
  contacts that name already finds, the one of exactly that name first.
  Opening the card adds nobody.
- In a search grid, *Add to contacts* on the selected rows adds whoever
  each card is about, every handle of theirs (`contactPages.ts`,
  `POST /contacts/add`), and a toast says what came of each.
- `ContactsCard.ce.vue` (`contactsView()`) is the contacts app's own
  table: every contact, read live from the app's `GET /contacts`, never
  through the index, so it needs no sync and shows an edit made
  anywhere as soon as the `curated` frame arrives. It is a pinned tab of
  a new window, and Browse on the app's Manage row opens it. A contact with no handle is drawn as a contact
  chip (`contactCell`, through `contactsById`), and the one marked as you
  carries a small "me" after its name, as every chip of yours does. Its right-click deletes the rows it aims at, or
  merges every other selected row into the one clicked. Your contacts
  are not in the search grid: nothing renders this store. Its tabs are
  the list and *Unfiled*, and a banner opens an agent's suggestions
  (§"The contacts list").

### The contacts list

The contacts card's list and its *Unfiled* tab say how much each person
wrote, read from the index by two routes of the
`unified_grid_and_search_index` applet
(`applets/src/unified_grid_and_search_index/people_list.rs`). Both count
the grid's rows (§"How much someone wrote", `authors.rs` in
`unified_grid_and_search_index`), and read who is filed from the
contacts store (`ContactsReader::filing`, a detached read):

- **Labels.** A person gives a contact a label on its page, which shows
  the contact's labels, each with an ×, and a box offering the labels
  the others carry (`GET /contact/{id}/labels`, `GET /labels`, `POST
  /label`). The list shows them as *Labels*, and its filter box finds
  them. The Library tree (`docs/dev/cards.md`, `libraryTreeView`) has
  *Contacts* above the sources, each label a node under it, as an
  address book's groups are under it; picking one lists who carries it
  beside the tree (`contactsView({ label })`), and a contact made there
  carries it. `real` is left out of all of these: it says whether a
  sender is spam, and *Unfiled*'s *Hide this sender* sets it.
- **Suggestions.** While an agent's suggestions wait, a banner above
  the list says whose and how many ("tng-curator suggests one change to
  your contacts"); it opens them (`SuggestionsList.ce.vue`), least sure
  first, each a sentence ("These two are **Will Riker**", "This is also
  **Data**"), its handles as grey chips, the agent's reason and run,
  and *Yes* and *No* (`SuggestionItem.ce.vue`). *Yes to all* agrees
  with everything one run of an agent suggested.
- **`GET /people/contacts`** answers `{counts: {<contact_id>: {items,
  last_at}}}`: the rows written by every handle the contact reaches, as
  `from:contact:<id>` reads them (a merged-away contact's too). A contact whose handles wrote nothing is absent. The
  card puts *Items* and *Last* right after the person and sorts by
  *Most recent* (the default), *Most items* or *A–Z*
  (`contactsTable.ts::sortList`); a header click still sorts by any
  column. It fetches the list and the counts together, because the grid
  keeps the layout it drew and puts a later column last. A library with
  no index yet shows the list without them.
- **`GET /people/unfiled`** answers a typed table of everyone the grid's
  rows were written by who is on no contact, the most first: not the
  library's own accounts (`account_handles`), `me` or a model, and no
  sender marked not real (a `real` label out, by a person or an agent)
  unless `?not_real=true`, which marks them. One row per handle, named
  by the name it wrote the most under. *Unfiled* (`UnfiledList.ce.vue`)
  sorts it as the list does; selecting rows and *Add selected to
  contacts*, or *Add to contacts* on the right-click, adds each with
  every handle a source's record of them says is theirs and what that
  record said of each (`contactsTable.ts::peopleToAdd`, through `POST
  /contacts/add`). Rows whose handles meet, or that have one name, case
  aside, are one person. A handle a source ties to nobody else, such as
  a Facebook name, stays unfiled until it is added, even when a contact
  has the same name: the name alone does not say it is theirs.
- The grid's Author column is a chip too: `grid_rows.author_handle`
  (which `from:` finds through the search terms) comes with each
  message's row and each reaction's, and
  `GridCard.ce.vue` draws each Author cell from `people` and redraws
  them when an answer changes. The applet names the mark for a handle's
  kind in `columns.rs::handle_mark`.
- A contact's own row is drawn by who it is about, not by where it is
  filed ([`grid_rows.md`](grid_rows.md) §"`contact`, `email`, `phone`"
  has its columns). The grid's Contact column is a chip
  (`columns.rs::contact_identity`: the address, else the number, by the
  rule `contact-common::chip_handle` writes the members' chips by, else
  any handle the index holds for the contact), and the search list
  titles such a row with the same chip. Browsing an address book opens
  on Subject (the contact's chip), Phone, Email, Contents (the note
  first), Touched, Type and Channel (`config/browsePresets.ts`).

A chip ranks what the resolver returns: your contact first, then the source
contacts as `/people` ranked them, then the text the source showed. A
message's own header is the exception: it shows the name that message
gave ([`chips.md`](chips.md) §"Trust").

**A source's name for a person is not any one message's.** A chat
source's record of a person is one row per document; `/people` merges
them (`unified_grid_and_search_index/src/people.rs::merge_and_rank`) and puts
first the name the most was written under: each document's first name
(its most used, as `chat-common::people::baseline_contacts` orders
them) is credited with the rows the person wrote there, at least one,
and a tie goes to the name that sorts first. So one forged message in a
spam folder cannot rename the sender on the rest of their mail. A name
that spells out an address, a number, a handle's URI or its stored form
(`datalib_handle::Handle::spelled_by`) that the record does not hold
(`"picard@enterprise.org" <q@continuum.test>`) is no name for it
(`NormalizedContact::is_anothers_handle`). The render drops it from
every source's contacts as the document is stored
(`datalib_etl_render::names`), `/people` drops it too, the search's
`names` table takes no row author spelled that way
(`search_terms::names_seen_under`), and the chip refuses one
(`contacts.ts::namesSomeoneElse`), showing the identifier instead.

## Searching for a person

A person is searched by the role they had on a row: `from:`, `to:`,
`mention:`, `with:` and the rest; which key finds which role, and what
a value matches, is [`searching.md`](../user/searching.md) §"People".
The keys read the search terms, which hold each row's people by handle
([`grid_rows.md`](grid_rows.md) §"`supplied_terms`"). A name also
reaches every handle a source ever showed under it, through the search
terms' `names` table, filled from the rows' authors and from
`contact_records`. A chip's "Everything with <name>" writes
`with:contact:<id>` when the handle is linked to one of your contacts,
else `with:<handle>` (`chipActions.ts::everythingWith`). Typing `@` at
the start of a word offers people and writes the pick as `with:`.

What a document's own row answers to:

- **Everyone who took part.** chat-common supplies a `participant` term
  for every author, recipient, mention and reactor of the messages it
  holds, by handle, else by the name shown. Only messages count (not a
  tool call or a system note), and a label a provider shows where it
  has no person (the account's "Me", an "Unknown" sender) names nobody.
  So `is:document with:riker` lists the conversations Riker was in, one
  row each; without `is:document` it also finds each of his messages.
- **Who an untitled conversation is named after.** The provider fills
  `NormalizedChat::named_after` (a DM's other party, an untitled
  group's members) and chat-common writes each as a `subject` term,
  which the Subject cell draws as chips and `with:` reads, so
  `with:riker` finds a DM with him where only you wrote. LinkedIn, Slack
  DMs, Messenger, Signal and WhatsApp one-to-one chats, Messages, Google
  Chat and SMS backups fill it.
- **Who a contact's card is about.** contact-common supplies an `about`
  term for each handle and name on the card, so `with:riker` finds his
  card; `from:` never does, since nobody wrote it.

**`contact:<id>` is everything from Riker, whatever handle he used.**
The search terms know handles and never contacts. A `contact:<id>`
value is read from the contacts store when the search runs (a detached
read, so the applet stays the one writer) and matches each handle the
contact reaches: its own, stopped ones included but not those an
agent suggested and nobody said Yes to; those of a contact
merged with it. So a new link changes
nothing in the index and search follows it at once, as chips do. A root
without the contacts app refuses `contact:` by name.

**`me` is the person whose library this is.** An unquoted `me` on a
person key matches `me:self`, the stand-in the messaging backups write
for the owner; the handles of each source contact marked `account`,
which the grid index keeps in `account_handles`; and every handle of
each of your contacts marked as you (the store's `me` table). These are
read when the search runs and are part of the search's cache key, so
marking a contact, or linking a handle to a marked one, changes the
next search with no re-index. An index from before `account_handles`
refuses `me` by name until its next sync. A source marks its owner by
putting a record of them marked `account` in `NormalizedChat::contacts`
(`account_contact`, or `stand_in_account` for a source that shows its
owner as "Me"). Which sources do, and what `me` cannot find, is
[`searching.md`](../user/searching.md) §"`me`"; change it with them.

In a checkout,
`datalib/backend/applets/src/unified_grid_and_search_index/search_by_example_tests.rs`
shows each of these rules on a small cast.

**Keeping the name out of the markdown is a choice, not a rule.**
Writing a contact's name into each chip link would make a document say
who a handle is on its own, and searchable under the name you gave the
person. It would cost what the links above save: render would read the
contacts store, and a link would render again every document naming
the handle and send each back through the keyword index and the
source's `embed`.

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
| a link, an unlink, a handle marked stopped, a label | no document; open chips are drawn again |
| the contacts store's shape | a rung on `LADDER`; never a reset |

## Where it is tested

- `handle`: every constructor's spellings, `parse` against `rebuild`,
  the URI round trip over every kind.
- `contacts`: the store end to end on a real doltlite file, the ladder
  against the current rules, the photo rules; `review.rs` an agent's
  batch, a person's confirm, reject and in/out/unknown, and a second
  batch leaving the person's word alone.
- `chat-common`: `people.rs` (the baseline, recipients, reactors, the
  provider merge); `render.rs` (the header and recipients line).
- `unified_grid_and_search_index`: `people.rs` ranking; the applet's `/people` against
  the fixture index.
- `tests/fixtures/ingested_tng_test.py`: the end-to-end guard, from the
  TNG fixture through the index: Picard known by one address from
  three sources, one number written two ways by three sources as one
  handle, WhatsApp's address book, Signal's number and ACI together, a
  Slack mention as a chip link, two cards' photos at their blob-store
  addresses.
- UI: `contacts.test.ts`, `resolver.test.ts` and
  `tests/chip_links.test.ts`; the render preview golden shows
  unresolved chips. `tests/e2e/contacts.spec.ts` runs on a root with the
  contacts app: it links Riker's Slack and email handles to one contact
  from two documents and checks the documents and an open grid follow.
- `curated_overlay`: a checked-in contacts store, carried forward by each
  change that moves its shape, laid over the TNG root
  (`:tng_overlay_test`) and over the live bake's data, opened as the
  applet opens it, and asked through the search what each contact, merge
  and `me` reaches
  ([`curated_overlay/README.md`](../../datalib/backend/curated_overlay/README.md)).

## Not built

- merging or deleting from the person card;
- undo;
- the triage grid of unresolved handles;
- labels in search (`label:` and its negation) and on a chip;
- a handle for a text thread known only by a number without its
  country code;
- a handle that stopped working in some apps but not others;
- mentions in Google Chat, WhatsApp and Messages (each marks them up,
  in a shape not yet checked on real data), in Facebook (a tag is
  `@[<id>:2048:<Name>]` in a post, not yet written as a handle) and in
  Beeper (no handle kind for its users);
- a handle for a Beeper (Matrix) user.

The plans for them are `docs/dev/plans/contact_linking.md` and
`contact_editing.md` in the repo.
