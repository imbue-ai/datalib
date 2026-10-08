# Contacts: what is still to build

*Proposal (2026-10-01). Phases 1 to 3 are built, and what the tree
does is [`../contacts.md`](../contacts.md): handles, each source's
record of a person, the contacts app's store and routes, chips. This
plan keeps only what is not built yet. Facts about the tree it cites
were read when each section was written; check one before relying on
it.*

The same person shows up in a mirror under many identifiers. Handles
and the contacts app (built) let a person say which are one person and
see it on every chip. What remains is managing contacts as records,
answering "everything about this person" in search, and the identifiers
no source has a handle for yet.

## Words

The reference's words ([`../contacts.md`](../contacts.md) §"Words")
hold here. One more:

| word | means |
|---|---|
| **address-book card** | a record the `contacts` *provider* mirrors from CardDAV or a `.vcf`: upstream data, like a Slack profile, and so a source contact, not a contact. See [The code name](#the-code-name). |

## Handle kinds not made yet

| kind | value | from |
|---|---|---|
| `beeper` | the Matrix user id | Beeper |
| `linkedin` | the profile URL | LinkedIn |
| `lightroom-face` | the face tag's name | later |

A new kind is not a rules change; the reference lists the places
one touches (§"Changing the rules"). Phone numbers written without a
country code have no handle today; giving them one needs a default
region, a setting of the applet that the render reads through the
step's params.

## `row_handles`: which rows mention a handle

`grid_index` fills a derived table, `row_handles(uuid, handle, role)`,
with `role` one of `author`, `recipient`, `reactor`, `mention`, `self`
(a contact document's own handles). It is what the triage grid counts
and what the `contact:` filter joins through. The index still knows
handles and never contacts.

**A mention counts only where the source marks it up**: Slack's
`<@U02>` (already a chip link), a Notion user mention, a GitHub
`@login`. Each carries an exact id. Addresses found in plain text are
left out: quoted replies repeat every earlier message, so each would be
counted again in every reply, and signatures add noise.

### What it costs

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

Measure it first on a copy of a real root — counts and sizes only —
against a budget: the table under 5% of the index, and `grid_index`'s
incremental pass under 10% slower.

## Managing contacts

### The store, what it still needs

The tables are built ([`../contacts.md`](../contacts.md) §"The
contacts app"); these parts of them are not used yet:

- **Merge.** A merge keeps the survivor's id and sets `merged_into` on
  the other, so a link or URL holding the old id still resolves. Search
  already leaves merged-away contacts out.
- **Groups and members.** An address two people share belongs to a
  group contact whose members are those people; `members` holds them.
- **`linked_how`** gains `card` (adopted from an address-book card)
  and `suggestion` (accepted from a suggestion), beside `manual`.
- **An export**: JSON of every table, and vCard for the people,
  leaving out handles that stopped working, so the data is readable
  without doltlite.
- **Undo.** Every edit is a commit, so undo is `dolt_revert` of that
  commit. A draft is on the branch `claude/contact-card-wip`: the
  store's `history` and `revert`, the two routes, and a section of
  `doltlite.md` with tests of what `dolt_revert` refuses (a later edit
  of the same rows, a second undo, an uncommitted change).

Routes still to add to the applet:

| route | does |
|---|---|
| `POST merge` | merge two contacts |
| `POST members` | add or remove a group member |
| `GET contact/{id}/history`, `POST revert` | a contact's commits; undo one |
| `GET export` | the JSON and vCard export |
| `GET search?q=` | also unlinked address-book cards, for the typeahead |
| `POST contacts` | also adopting every handle on an address-book card |

### Editing

Three surfaces, in the order a person meets them. The first is built
in part; the popover links, creates, unlinks and marks a handle as no
longer working.

1. **The popover on an unresolved chip** also offers unlinked
   address-book cards in its typeahead (picking one creates the
   contact and adopts every handle on it), and, when a source ties the
   handle to other handles (an address-book card, a Slack profile's
   email), offers to link those too. One gesture, no dialog.
2. **The contact card**, opened from a resolved chip (the chip's
   double-click, `chips.md` §"Clicks") — a card, not a modal
   (`cards.md`). Handles grouped by kind, each with unlink; those that
   stopped working after the working ones, struck through; groups it
   belongs to, or members if it is a group; "Merge with…"; its
   documents (a `contact:` search); its history, each entry with undo.
3. **A triage grid** of unresolved handles ranked by how often
   `row_handles` names them. Linking the top fifty correspondents
   covers most of a mailbox, and this is where that happens.

**Only a person makes links.** What upstream asserts — this
address-book card holds these three handles, this Slack profile has
this email — feeds the one-click adopt and, later, suggestions, but
never links anything by itself.

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

## Option: the contact's name in the markdown

Today no render reads the contacts store, so a document carries only
the name its source showed, and a link changes no stored document
(the reference's §"Searching for a person"). Writing the contact's
name into the chip link instead would let a document say who a handle
is without the app: `grep` finds Riker's mail under "Riker" whatever
the sender called him, and qmd's free text does too.

What it would take:

- **Render reads the contacts store**, at one pinned commit, the way
  it reads its own raw store, and reports which commit it used.
- **A link renders again only the documents that name its handles.**
  `row_handles` says which those are; re-rendering every source on
  each link would make linking expensive enough to avoid.
- **The chip still draws from the live answer**, so a document
  rendered before the last link never shows a stale name; the stored
  name is for readers outside the app.

Not decided; nothing built so far rules it out.

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
the interning [`row_handles`' costs](#what-it-costs) leave for a
measurement to ask for.

## Order of work

Phases 1 to 3 (handles end to end, the store and its applet and chips,
`NormalizedContact` and the source contacts) are built, but for the two
pieces of phase 1 above: `row_handles`, and mentions outside Slack.

4. **Managing contacts.** The contact card, merge, undo, groups and
   members, the triage grid, adopting a source's handles, the export.
5. **Contacts in search.** The snapshot step, `live_view`, the
   `contact:` filter.
6. **Later.** Suggestions, on a branch of the store, so nothing
   automatic reaches `main` without a person accepting it; Lightroom
   face tags; a distinguished "Me" contact seeded from each source's
   `account`; a validity range on a link, for a handle reassigned to
   someone else (`stopped_working_by` is already its end); a stop
   scoped to some sources, for a number that stopped working for texts
   but still works on WhatsApp (`tel:` is one handle across apps, so
   today a stop applies to all of them).

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
