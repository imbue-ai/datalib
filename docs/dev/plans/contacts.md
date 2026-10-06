# Contacts: one person (or household, or list) across every source

*Proposal (2026-10-01), partly built: a first slice of phases 1 and 2
(see "Order of work") landed with the PR that added this banner. The
facts about the tree it starts from were read on 2026-10-01 and are
cited by path; check them before relying on one.*

The same person shows up in a mirror under many identifiers: an email
address, a phone number in WhatsApp and Signal and Messages, a Slack
user id, a LinkedIn profile, eventually a face tag in Lightroom.
Nothing in datalib says these are one person. This plan adds a
**contact** — datalib's own record of a person or a group of people —
which a person links identifiers to by hand, and which every view of
the data then shows: a chip with the contact's name and photo wherever
a rendered document names one of its identifiers.

It is also the first time a person edits state in a store through the
UI, and the first time live content takes part in the grid and in
search. Both are written so the next feature of either kind can follow
them.

## Words

| word | means |
|---|---|
| **handle** | one identifier in one namespace, normalized: `email:riker@enterprise.org`, `tel:+12025550123`, `slack:T01/U02`. Upstream data; a render finds it. |
| **contact** | datalib's record of a person (`kind = person`) or of several people who share handles (`kind = group`: "Mom & Dad", a mailing list). A person creates it. |
| **link** | a row saying a handle belongs to a contact. A person makes it. |
| **`DatalibContact`** | a person as one source describes them — the handles it ties together, the names it shows, a photo and details. A source's is read from raw rows at render time; the contacts app's is the one a person made. See [One type for a person](#one-type-for-a-person-datalibcontact). |
| **address-book card** | a record the `contacts` *provider* mirrors from CardDAV or a `.vcf` — upstream data, like a Slack profile. Not a contact. See [The code name](#the-code-name). |

## The handle

A handle is `<kind>:<value>`, made by one pure function per kind in a
small crate with no dependencies on the rest of the tree, so render
crates and the applet can both link it.

The choice that matters most: **where a native id *is* an email
address or a phone number, the handle is `email:` or `tel:`, not a
per-app kind.** A WhatsApp JID is a phone number; a Signal recipient
has an E.164 number; an iMessage handle is one or the other. So one
link (`tel:+12025550123` → Riker) covers WhatsApp, Signal, Messages and
SMS at once. Only ids that are opaque by nature get a kind of their
own.

| kind | value | from |
|---|---|---|
| `email` | lowercased address | email From/To/Cc, Google Chat, contacts cards, iMessage |
| `tel` | E.164 | WhatsApp JIDs, Signal e164, Messages, SMS, contacts cards |
| `slack` | `{team_id}/{user_id}` | Slack messages and reactions |
| `beeper` | the Matrix user id | Beeper |
| `linkedin` | the profile URL | LinkedIn |
| `signal-aci` | the ACI | Signal, when a recipient has no e164 |
| `lightroom-face` | the face tag's name | later |

Phone normalization needs a default region for numbers written without
a country code; that is a setting of the applet, read by the render
through the step's params.

## Handles in the render

Today a sender reaches the markdown as a display name only —
`MessageHeader::render` (`etl/render/src/message.rs`) writes
`<span class="msg-author">{name}</span>` with nothing else on it, and a
reaction carries only the reactor's name. `NormalizedChatItem` has an
`author_id`, but `chat-common/src/render.rs` never reads it, and what
providers put there is not consistent: email puts the *mailbox
account*, Signal the backup's local recipient row id, which is not
stable across backups.

The change:

- `NormalizedChatItem::author_id` becomes `author_handles:
  Vec<Handle>`, and `NormalizedReaction` gains `reactor_handles`. Email
  fills it from the From address and gains To/Cc as rendered
  recipients; Signal maps its recipient row to e164 or ACI.
- The one shared helper writes
  `<span class="msg-author" data-handle="email:riker@enterprise.org">Will Riker</span>`.
  The text stays exactly what the source said: it is what qmd indexes
  and what shows wherever nothing resolves. **`data-handle` is
  load-bearing**, like `data-section-uuid`, and goes on the
  sanitizer's allowlist (`chat-common/README.md`).
- contact-common keeps the typed emails and phones the contacts parse
  already has (`contacts_render/src/render/parse.rs`) instead of
  flattening them into `(label, value)` rows, and writes them as
  handle spans too.
- `grid_index` fills a new derived table, `row_handles(uuid, handle,
  role)` with `role` one of `author`, `recipient`, `reactor`,
  `mention`, `self` (a contact document's own handles). The index
  knows handles and never contacts.
- **A mention counts only where the source marks it up**: Slack's
  `<@U02>`, a Notion user mention, a GitHub `@login`. Those carry an
  exact id, so they render as `data-handle` spans and become chips
  too. Addresses found in plain text are left out: quoted replies
  repeat every earlier message, so each would be counted again in
  every reply, and signatures add noise.

This phase changes nothing a person sees, and costs a re-render of
every chat source (a `RENDER_VERSION` bump each).

### What `row_handles` costs

An estimate to check, not a measurement:

- **Size.** About one entry per message (its author), one per reaction,
  plus an email's To and Cc: perhaps two or three per grid row, at
  roughly 100 bytes each and twice that with the handle index. The one
  root `paged_grids.md` measured holds about 17 KB of index per grid
  row (bodies and twelve indexes), so this adds a few percent.
  Doltlite does not compress, so that is the real figure.
- **Writes.** Keyed `(uuid, handle, role)`, a sync's new entries land
  together at the tree's right edge, as its grid rows do
  ([`doltlite.md`](../doltlite.md) § "What a write costs"). The
  `handle` index scatters — a sync bringing messages from 200 people
  touches about 200 of its pages — which is exactly what
  `grid_rows_by_author` already does, one index of the kind grid_rows
  has twelve of.
- **Item rows only.** Messages, reactions and contact documents get
  entries; a chat's document row does not, or a 10,000-message chat
  would repeat every participant. A document's participants are a join
  through its items.
- **No cap on recipients.** To and Cc can run to hundreds, and
  dropping some would make the record lie. One 300-recipient email is
  about 60 KB of entries, in proportion to its body. What grows badly
  is a reply-all thread — forty replies to the same 300 people is
  12,000 entries saying one thing. If the budget below is missed, the
  fix is to store each distinct recipient list once and point each
  email at it (340 entries instead of 12,000), not to drop any.
- **The triage grid's counts are a scan.** If they are slow at a few
  million entries, `grid_index` keeps a `handle_counts` table as it
  goes.

Phase 1 measures it on a copy of a real root — counts and sizes only —
against a budget: the table under 5% of the index, and `grid_index`'s
incremental pass under 10% slower.

## One type for a person: `DatalibContact`

Every source that knows people already describes them in its own way:
Slack's user list (a name, a title, an avatar, often an email), WhatsApp's
address book and the names people give themselves, Signal's recipients
(a number and an ACI), a vCard, a LinkedIn connection, a Facebook
friend. A **`DatalibContact`** is the one type for all of them, and for
the contacts app's own contacts too: who a person is, as one source
describes them.

All of this happens after the raw layer: a `DatalibContact` is built at
render time from raw rows, or by the contacts app from its store, and
everything derived from one is rebuildable.

```rust
struct DatalibContact {
    source_id: String,          // who describes the person: a source, or the contacts app
    key: String,                // that source's own id for them: a Slack user id, a vCard UID, a contact_id
    kind: ContactKind,          // person, group, organization
    names: Vec<String>,         // the names it shows, the one it prefers first
    handles: Vec<ContactHandle>,// each Handle, with its label ("work", "cell") and stopped_working_by
    photo: Option<Photo>,       // bytes, a blob in the source's CAS, or a URL
    org: Option<String>,
    title: Option<String>,
    note: Option<String>,
    details: Vec<(String, String)>, // birthday, address, anything else the source says
    groups: Vec<String>,        // an address book's categories, a group's members
    source_url: Option<String>,
    seen: Option<Seen>,         // how many items they authored in this source, the last one's stamp
}
```

**It lives in a small crate of its own**, depending on `datalib_handle`
and serde and nothing else, so the render crates, the contacts crate,
the applets and the UI's mirror of it share one definition at no cost.

**Many describe the same person, and the source tells them apart.**
Slack's, the address book's and the one you made are three
`DatalibContact`s with three `source_id`s. A chip ranks them: the
contacts app's first, then an address-book card, then a provider's own,
then chat-common's baseline. So "your contact" is not a different type,
only the top-ranked one.

**Who produces them**, each needing less from the provider than the one
before:

- **The contacts app**, from its store: a contact and its handles,
  `stopped_working_by` included.
- **A source about people** — the `contacts` provider's vCards,
  LinkedIn connections, Facebook friends — one per record.
- **A chat provider, for what only it knows**: Slack's users (with the
  profile email as a second handle), WhatsApp's contacts (a linked id
  and its number), Signal's recipients. `NormalizedChat` carries them.
- **chat-common, for free.** It sees every item's `author_handle` beside
  the name the provider resolved for it, so it adds a baseline per
  handle per source — the names it was shown under, how often, last
  seen — for every chat provider with no per-provider code. A
  provider's own and the baseline for the same handle merge into one.

**contact-common is the one place a `DatalibContact` is rendered.** It
turns one into a document — the page a vCard or a contact gets today —
and writes the `contacts` and `contact_handles` rows a render store
gains beside `grid_rows`. What rendering needs beyond who the person is
(the document's uuid, the address book it is filed under, the raw rows
it was built from for incrementality) is passed in beside the
`DatalibContact`, never stored on it; that bookkeeping is what made
`NormalizedContact` a render-side type, and it is retired.

**Not every one becomes a document.** A source about people — an
address book, LinkedIn connections, the contacts app's snapshot —
renders each as a document, so it is searchable. A chat provider writes
only the rows, or every Slack user would be a search result.
`grid_index` loads the rows into the index the way it loads `grid_rows`,
and a row's id is minted like any entity id (`entity_ids.md`), from
`source_id` and `key`.

**Who reads them.**

- **The core resolver.** The `unified_index` applet answers `POST
  /people` with every `DatalibContact` in the index holding each
  asked-for handle, ranked. This works with no contacts app at all.
- **The contacts app**, whose `resolve` answers in the same type. A
  chip gets one ranked list per handle, whoever produced it; it draws
  the first, and the hover card shows the rest ("seen as 'J-L Picard'
  in Gmail, 'Captain' on Slack").
- **Adopting and suggesting.** One with several handles is exactly what
  "adopt card" adopts — "link all four of this card's handles" — and a
  Slack user whose email matches a contact is the first kind of
  suggestion.

It costs one row per person per source plus one per handle — small
beside `row_handles`, which grows with every message.

## A separate app, tightly integrated

Contacts is built as an app of its own that plugs into datalib, not as
part of the core. **The core learns handles and never contacts.** What
the core gains is generic, and the contacts app is its first user:

| the core gains | the contacts app uses it to |
|---|---|
| `data-handle` spans and `row_handles` | know which rows mention a handle |
| a handle resolver the document view and the grid call | draw chips |
| `DatalibContact`: each source's account of a person, in the index | rank its own above them, offer a card's handles to adopt |
| `grid_rows.live_view` | open a contact's row as a live card |
| an ordinary source group | put contacts in search and qmd |

Nothing in the core opens the contacts store. So removing the app is:
take its `[[applets]]` entry and its group out of the config, and
delete `<data_root>/datalib_curated/datalib_contacts/`. Every source, the index
and search keep working; handle spans go back to plain text.

## The app's store

`<data_root>/datalib_curated/datalib_contacts/contacts.doltlite_db`, with the
export and anything else the app keeps beside it.

**It lives apart because it is the one store that cannot be rebuilt.**
Every other store under a root is either derived (render, index, qmd)
from something that can be fetched again, or the machine's own
bookkeeping. This one is a person's work. `datalib_curated/` holds one
directory per app, so an app's state can be managed or deleted on its
own; the next app of this kind (tags, notes, saved queries) gets a
sibling. `feedback` and the remote-media allow rows stay in `system/`:
they belong to the core. `system` is today the only reserved top-level
name (`SYSTEM_DIR`, `dag/src/config.rs`); `usable_group_id` has to
refuse `datalib_curated` too.

**Why doltlite:** every edit is a commit, so the history is an audit
trail ("when did I merge these two?") and undo is a revert; and a
branch is where suggestions go later (below), so nothing automatic
reaches `main` without a person accepting it.

### Tables

| table | key | holds |
|---|---|---|
| `contacts` | `contact_id` | `kind` (`person` / `group`), name, note, photo, `merged_into` (set when merged away), created/updated `_at_utc` + `tz_offset` |
| `handles` | `handle` (`kind:value`, as `datalib_handle` spells it) | `contact_id`, how it was linked (`manual`, `card`, `suggestion`), `linked_at_utc`, `stopped_working_by` |
| `members` | `(group_id, member_id)` | `added_at_utc` |

- **A handle belongs to exactly one contact.** An address two people
  share belongs to a group contact whose members are those people.
  Each handle then still resolves to one thing, which keeps the key,
  the chip and the search join simple.
- **Keys are handles, never `grid_rows.uuid`.** That uuid moves when
  a recipe changes ([`entity_ids.md`](../entity_ids.md) § "What a
  re-key costs"); a handle is the upstream's own identifier.
- **`contact_id` is a random v4.** This is the one place the "an id
  is a pure function of upstream data" rule does not apply, because a
  contact is minted by a person's act, not by a record.
  `entity_ids.md` should say so in a paragraph.
- **A merge keeps the survivor's id** and sets `merged_into` on the
  other, so a link or URL holding the old id still resolves.

### A handle that no longer works

An old phone number or a closed email account still names its owner in
every call log and message from when it worked, so it stays linked; it
just should not be used to reach them any more. That is one column on
the link, **`stopped_working_by`**: null while the handle works, and
otherwise a date by which it had stopped.

- **"By", not "on".** Marking a handle as not working fills in today,
  which is always true when nothing better is known; the person narrows
  it later if they remember. The column never claims more precision
  than the person gave it.
- **A partial date**: `2019`, `2019-06` or `2019-06-14`, as precise as
  the person knows. It is a date a person remembers, not an instant
  anything measured, so it is not an `_at_utc` stamp.

A chip for a handle that no longer works still resolves to its contact
— that is the point — and marks the handle as old on hover. The
contact card lists such handles after the working ones, struck
through, and the vCard export leaves them out.

This does not cover a number that was *reassigned* to someone else:
the key is still the handle alone, so a handle has one owner for all
time. If that case turns up, the key gains a validity range;
`stopped_working_by` is already its end.

### What being irreplaceable requires

- **Nothing but the person deletes it.** Resetting a source or the
  index, or deleting a source, never touches this directory. A link to
  a handle no source mentions any more just resolves nothing. Deleting
  the app's directory is the one way to lose it, and the UI says so
  before it does.
- **Every shape change is a migration rung, never a reset.** The
  "breaking changes are fine" rule in `AGENTS.md` exempts input from a
  person, and this store is all such input.
- **An export** — JSON of all three tables, and vCard for the people —
  so the data is readable without doltlite.

## Who owns it: the `datalib_contacts` applet

A new `datalib-applet datalib_contacts` subcommand (an `[[applets]]` entry) is
the store's one writer, holding the per-file lock for its life. It is
the first applet that writes. The only other reader is the app's own
snapshot step (below), which reads one commit.

| route | does |
|---|---|
| `POST resolve` | `{handles: […]}` → each handle's `{contact_id, label, icon}` or null |
| `GET search?q=` | contacts and unlinked address-book cards, for the typeahead |
| `POST contacts` | create (optionally adopting every handle on an address-book card) |
| `POST link`, `POST unlink` | one handle to or from a contact |
| `POST merge`, `POST members` | merge two contacts; add or remove a group member |
| `GET contact?id=` | one contact, its handles, groups or members, and its history |
| `POST revert` | undo one commit |

Each write is one commit on `datalib_writer`, sealed onto `main`
(`etl/README.md` § "Connection pools").

The document view and the grid call `resolve` at
`/applet/datalib_contacts/…`, the way they call `unified_index`. That
is the handle-resolver seam: the core knows to call it, not what it
returns beyond `{label, icon}`. If no `datalib_contacts` applet is
configured, handle spans are drawn as their plain text and nothing is
offered — the feature is absent, not degraded.

## Chips

The `identity` cell type (`{id, label, icon, detail}`, `cards.md` §
"typed cells") is already a chip in all but name; it becomes one Vue
`IdentityChip` used by grid cells and by documents alike.

- **The icon is a token or a URL.** A token (`slack`, `gmail`) maps to
  a bundled asset as today (`ui/src/config/icons.ts`); a contact's
  photo is a URL the applet serves.
- **In a document**, a `decorateHandles` pass beside
  `decorateRemoteMedia` in `ChatBody.ce.vue` collects every
  `[data-handle]`, asks who they are (the core's `DatalibContact`s and
  the contacts app's, one call each per document), and draws chips. A
  chip draws the top-ranked one: your contact's name and photo, or else
  the best a source gave — an address-book name and photo, a Slack
  avatar — with the handle kind's mark and a quiet "+", since it is not
  yet a contact of yours.
- **In the grid**, the search applet returns each row's author handle
  beside its display name, and the grid resolves the visible rows'
  handles through the same `resolve` call, so authors read as contacts
  too. The search applet never opens the contacts store.

## Editing

Three surfaces, in the order a person meets them:

1. **The popover on an unresolved chip.** A typeahead over contacts and
   unlinked address-book cards (picking a card creates the contact and
   adopts every handle on it), plus "New contact". When a source ties
   the handle to other handles, it offers to link those too. One
   gesture, no dialog.
2. **The contact card**, opened from a resolved chip — a card, not a
   modal (`cards.md`). Handles grouped by kind, each with unlink; groups
   it belongs to, or members if it is a group; "Merge with…"; its
   documents (a `contact:` search); its history, each entry with undo.
3. **A triage grid** of unresolved handles ranked by how often
   `row_handles` names them. Linking the top fifty correspondents
   covers most of a mailbox, and this is where that happens.

**In v1 only a person makes links.** What upstream asserts — this
address-book card holds these three handles, this Slack profile has
this email — feeds the one-click "adopt card" and, later, suggestions,
but never links anything by itself.

## Live content in the grid and in search

A contact is live state, but the grid, search and qmd all read rendered
documents. Rather than teach each of them a second kind of row, a
contact is **materialized** into an ordinary document and an ordinary
grid row, and the row names a **live view** to open instead of its
markdown.

- **A `datalib_contacts` group with a `snapshot` step** (no inputs) opens the
  app's store read-only, reads `main`'s head hash, and reports it as
  its version. An unchanged store moves nothing downstream
  (`dag/README.md` § "Versions: reported by the step").
- **Its `render_markdown` step** opens the store at exactly that hash
  (a detached `<file>@<hash>` open) and writes one document per
  contact: name, handles, groups or members, note. `grid_index` and
  qmd take it from there like any source, so a contact is searchable
  by name and free text.
- **`grid_rows` gains `live_view`**: a component name
  (`datalib_contacts.contact`) and one string argument (the `contact_id`).
  Opening a row that has one opens that card; the markdown stays
  reachable as the snapshot qmd saw. It is a component name and an
  argument, never card source, so nothing in a row is evaluated.
- **After an edit** the UI opens a sync request for `datalib_contacts/snapshot`
  (`POST /api/requests`), so search catches up within a sync while
  chips are live at once.

Why not embed a live view inside a markdown document: the markdown
carries upstream content (an email body is HTML a stranger wrote), and
the sanitizer cannot tell an attribute our renderer wrote from one a
sender did. A field in `grid_rows`, written only by render code, has
no such ambiguity.

This is the general mechanism: any applet whose state should be
searchable gets a snapshot step and rows with a `live_view`.

## Search

`contact:<name or id>` is answered from the index alone, through what
the snapshot rendered. Each contact's document lists its own handles in
`row_handles` (role `self`), and a person's membership in a group is an
edge from the person's document to the group's, labelled `member of`.
So the filter finds the contact's document, takes its `self` handles
and those of every group it has an edge to, and then finds the rows
that mention any of them. Hits through a group carry the group's chip
so it is clear why they matched. Like the rest of search, it is as
fresh as the last sync. The filter is typed by people, so its spelling
is kept stable once shipped.

## Prior art: Thunderbird's global search

Thunderbird's global search index (Gloda, `global-messages-db.sqlite`;
MPL-2.0, so a source of ideas here, never of code) has the same shape:
an `identities(contactID, kind, value)` table indexed on `(kind,
value)`, each identity belonging to one contact; a `messageAttributes`
table with a row per identity per message for From, To, Cc and Bcc;
and the address-book name looked up when a message is shown. The
index is rebuildable and the address book a separate store a person
edits — the split between the index and `datalib_curated/` here.

Two things it does that this plan does not:

- **It creates a contact for every new address**, named from the
  header, and never merges them (its own comments say it meant to). So
  every newsletter sender is a contact and one person with three
  addresses is three. Here only a person creates a contact; an
  unlinked handle stays a handle.
- **It stores derived rows**: `recipients` (To, Cc and Bcc together)
  and `involves` (everyone on the message) beside `to` and `cc`, about
  three rows per recipient. The file is known for reaching hundreds of
  megabytes or more at 50–100k messages, and many people turn global
  search off. Here a row carries one handle in one role, and
  "involves" is a query.

It also stores each identity once and refers to it by integer id —
the interning [`row_handles`' costs](#what-row_handles-costs) leave for
a measurement to ask for.

## Order of work

**Built so far.** Phases 1–2's first slice: `datalib_handle` (`email`,
`tel`, `slack`); `data-handle` on the author span for email (From),
Slack, WhatsApp (a linked id through its number), Messages and Google
Chat/Voice; the `datalib_contacts` crate and applet; chips, the
link/create popover, the hover card and copy. Phase 3's core:
`DatalibContact` in `datalib_contact_schema`; contact-common rendering it
(vCards, LinkedIn, Facebook friends) with `NormalizedContact` retired;
chat-common's baseline; `source_contacts` / `source_contact_handles` in
every render store and the index; `POST /people` on the `unified_index`
applet; chips and the hover card drawing your contact first, then each
source's account, with no contacts app needed for the latter; the
contacts app answering `resolve` as a `DatalibContact`. Providers: a
chat carries the provider's own accounts (`NormalizedChat::contacts`),
merged with the baseline — Slack's profiles first, which tie a Slack
user to an email; Signal's numbers as handles; email's To and Cc as a
recipients line under the header, chipped like the author. Not yet:
WhatsApp's address book as accounts, Signal's ACI, reactions and
mentions, `row_handles`, groups, merge, undo, adopting a card's handles,
the contact card, chips in the grid, and phases 4–6.

1. **Handles end to end, nothing visible.** The handle crate (pure,
   unit-tested), `data-handle` in chat-common, email and contacts, the
   email and Signal fixes, `row_handles`.
2. **Store, applet, chips.** `datalib_curated/datalib_contacts/`, the
   applet and its routes, `IdentityChip`, `decorateHandles`, the
   link/create popover.
3. **`DatalibContact`.** The type in its own small crate; contact-common
   rendering it, with `NormalizedContact` retired; chat-common's
   baseline; Slack, WhatsApp and Signal filling in their own users; the
   index rows and their load; `POST /people`; chips and the hover card
   drawing the ranked list; the contacts app's `resolve` answering in
   the same type.
4. **Managing contacts.** The contact card, merge, unlink and undo,
   groups and members, the triage grid, author chips in the grid,
   adopting a source's handles.
5. **Contacts in search.** The snapshot step, `live_view`, the
   `contact:` filter.
6. **Later.** Suggestions on a branch; Lightroom face tags; a
   distinguished "Me" contact seeded from each source's `account`; a
   validity range on a link, for a handle reassigned to someone else.

## The code name

`contacts` is already a `SourceType` and a `grid_rows.provider` tag,
for the CardDAV/`.vcf` provider. Until that is settled, everything this
plan adds is named **`datalib_contacts`** in code — the applet id, the
group type, the provider tag, the store's file name. The name is
clumsy on purpose, so that it gets changed rather than kept. One
likely end state: the provider becomes `address_book`, keeping
`provider:contacts` as a search alias since people type that filter,
and this feature takes `contacts`. Prose and the UI say "contact"
throughout.

## Open questions

None yet.
