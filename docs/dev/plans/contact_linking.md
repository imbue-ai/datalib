# Contact linking: what is still to build

*Proposal (2026-10-01). Phases 1 to 3 are built, and what the tree
does is [`../contacts.md`](../contacts.md): handles, each source's
record of a person, the contacts app's store and routes, chips. This
plan keeps only what is not built yet. Facts about the tree it cites
were read when each section was written; check one before relying on
it.*

The same person shows up in a mirror under many identifiers. Handles
and the contacts app (built) let a person say which are one person and
see it on every chip. What remains is the rest of linking (merge,
groups, adopting what a source already ties together, the triage
grid), answering "everything about this person" in search, and the
identifiers no source has a handle for yet.

Editing what a contact *holds* — the contact card, its fields, drafts,
saving, undo, the export, writing back to CardDAV — is the other plan,
[`contact_editing.md`](contact_editing.md).

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

## Which rows name a handle: the search terms

Which rows name which handle, and in what role, is the person kinds of
the search terms ([`search_tabs.md`](search_tabs.md) §"The search terms"):
`from`, `to`, `cc`, `bcc`, `participant`, `mention`, `reactor`. That
section has the rules for which rows carry a person and what it
costs. The search terms hold handles and never contacts; the triage grid
counts them, and a `contact:` value is matched against them
([`search_autocomplete.md`](search_autocomplete.md)).

## Linking

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

Routes still to add to the applet:

| route | does |
|---|---|
| `POST merge` | merge two contacts |
| `POST members` | add or remove a group member |
| `GET search?q=` | also unlinked address-book cards, for the typeahead |
| `POST contacts` | also adopting every handle on an address-book card |

### Where links are made

Three surfaces, in the order a person meets them. The first is built
in part; the popover links, creates, unlinks and marks a handle as no
longer working.

1. **The popover on an unresolved chip** also offers unlinked
   address-book cards in its typeahead (picking one creates the
   contact and adopts every handle on it), and, when a source ties the
   handle to other handles (an address-book card, a Slack profile's
   email), offers to link those too. One gesture, no dialog.
2. **The contact card** ([`contact_editing.md`](contact_editing.md)).
   For linking it shows the handles grouped by kind, each with unlink;
   those that stopped working after the working ones, struck through;
   the groups it belongs to, or its members if it is a group; and
   "Merge with…". A link made there is an operation like the
   popover's, not part of the card's draft.
3. **A triage grid** of unresolved handles ranked by how many rows
   name them in the search terms. Linking the top fifty correspondents
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

A contact is searched as a value of a person key, `with:contact:<id>`
or `from:contact:<id>`, expanded into the contact's handles when the
search runs: [`search_autocomplete.md`](search_autocomplete.md)
§"Contacts: expanded when the search runs". It follows a link at once
and needs nothing above. The snapshot is what makes a contact a row of
its own, found by its name or its note.

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
  the search terms say which those are; re-rendering every source on
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

It also stores each identity once and refers to it by integer id, as
the search terms' `vals` table does.

## Order of work

Phases 1 to 3 (handles end to end, the store and its applet and chips,
`NormalizedContact` and the source contacts) are built, but for the two
pieces of phase 1 above: the person kinds in the search terms
(`search_tabs.md` step 4), and mentions outside Slack.

4. **Linking.** Merge, groups and members, the triage grid, adopting
   a source's handles. The contact card, undo and the export are
   [`contact_editing.md`](contact_editing.md)'s.
5. **Contacts as rows.** The snapshot step and `live_view`. The
   `contact:` value in search is
   [`search_autocomplete.md`](search_autocomplete.md)'s.
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
