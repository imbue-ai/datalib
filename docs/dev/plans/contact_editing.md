# Contact editing: what is still to build

*Proposal (2026-10-08). Nothing here is built. The contacts app's
store, its routes and how chips read it are in
[`../contacts.md`](../contacts.md); which handles are one person, and
how a person says so, is the other plan,
[`contact_linking.md`](contact_linking.md). The doltlite behaviour this
rests on is pinned by tests and written up in
[`../doltlite.md`](../doltlite.md) §"Merging a branch" and §"Reverting
a commit"; check a fact there before relying on it.*

The contacts app holds what a person says about the people in their
mirror. Today it holds a name, a note, a photo and the linked handles,
and the only way to change them is the chip's popover: one operation,
one commit. This plan is the **contact card**, where a person edits a
contact at length, copies fields from what each source says about that
person, and later publishes the result to a CardDAV address book.

It is the first place in datalib where a person edits data rather than
mirrors it, so it also sets the pattern any later editable record
follows: drafts, saving, seeing someone else's change, undo.

## The card shows a person; only your contact is editable

The card is a viewer of everything the mirror says about a person:
each source's record of them (a `NormalizedContact` from `/people`,
which needs no contacts app) and, when the contacts app is configured,
your own contact, the one part that can be edited. Without the app the
card is read-only and still useful: it is what WhatsApp, Slack and the
address book each say about this person.

- **It opens from any person chip**, resolved or not: double-click, as
  for a group or step chip (`docs/dev/chips.md` § "Clicks"). The card
  source names a handle and the source the chip was seen in,
  `personSource(handle, { seenIn })` in `cardSources.ts`, and the card
  asks `people` for the rest. "Everything from them" (`searchQueryFor`)
  becomes a button on the card. A single click still opens the link
  popover: linking or creating stays one gesture where you are reading.
- **What it is about depends on the link.** A handle linked to your
  contact opens that contact's card, whichever of its handles was
  clicked, headed with its name and "your contact"; it lists each of
  its handles with the source records for each. An unlinked handle
  opens a card about the handle itself, headed with the handle and "not
  linked", with *Create contact* where the app is configured.
- **Each source's record stays its own section**, headed by the
  source's mark and name, never merged with another. One `tel:` handle
  can carry WhatsApp's record, the SMS backup's and Signal's, and they
  may even be different people (a household landline). Where they
  disagree the card shows both.
- **The source the chip was seen in comes first**, marked "seen here",
  so a double-click in a WhatsApp chat lands on WhatsApp's record of
  that number with the others below it.

## Words

[`../contacts.md`](../contacts.md) §"Words" holds. Also:

| word | means |
|---|---|
| **draft** | a contact's unsaved edits: a branch of the store holding them as uncommitted rows. |
| **save** | merging a draft into the store's published state, as one commit. |
| **published** | what readers see: the store's `main`, which the applet moves only when it seals. Chips, search and every other card read this. |
| **source value** | what one source's account of the person says a field is (a Slack profile's title, an address-book card's birthday). |

## What a contact holds

Today: `contacts` (name, note), `photos`, and the handles linked to it.
To copy fields from sources the card needs somewhere to put them, so
the store gains a table shaped like vCard's properties:

```
fields(contact_id, field_id, kind, label, value, handle, position,
       copied_from_source, copied_from_handle, copied_at_utc, tz_offset)
```

- `kind` is a closed set, an enum: email, phone, organization, title,
  birthday, address, url, and others as the card needs them.
- `label` is free text a person types ("home", "work").
- `handle` is the value normalized as a handle (`email:…`, `tel:…`)
  where it is one, and empty otherwise. It is how the card matches a
  field to a link; it links nothing.
- `copied_from_*` says which source's account the value came from, or
  is empty for a value the person typed.

### Fields and links are separate

A **field** is what the card says about a person: the lines of their
address-book entry, written out by the export and by CardDAV
publishing. A **link** is who a chip names: a handle tied to one
contact, made only by a deliberate act (the popover, the card's link
button), under the rule that a handle has one holder.

Neither implies the other. A household's landline can be a field on
Riker's card and on Troi's, and link to nobody, or to a group contact
for the household if someone makes one. A Slack id can be linked to a
contact and never appear as a field.

The card shows how each email or phone field stands:

| the field's handle is | the card shows |
|---|---|
| linked to this contact | nothing more |
| not linked | *link it*, one click, never automatic |
| linked to someone else | that contact's chip ("Troi's"), which also catches a typo |

Linking a handle that is not yet a field offers to add it as one;
again one click, never automatic.

**A copied value is a copy.** A later change upstream does not flow in
by itself. The card compares each copied value with the source's
current one and marks a field whose source now says something else
("Slack now says *Commander*"), with one click to take it.

## Copying from sources

Beside the contact's own fields the card shows each linked handle's
source accounts, which `/people` already serves from the index. Each
source value has a copy button, and each account has "copy all". Where
two sources disagree the person picks one; nothing is merged
automatically. This is the "merge data from several sources" step: the
person does it field by field, and the store records where each value
came from.

## Drafts

- **One draft per contact**, a branch `draft/<contact_id>` cut from the
  published commit when the card starts editing. Two windows open on
  the same contact edit the same draft and see each other's autosaves.
- **Autosave is a plain SQL write to the draft branch**, made when
  typing pauses or a field loses focus, and left uncommitted. Doltlite
  keeps a branch's uncommitted rows in the file, past the connection
  that wrote them and past a reset of another branch.
- **Nobody else sees a draft.** Readers read the published commit.
- **Reopening the card resumes the draft**, from any window, after a
  reload or a restart.
- **Discard deletes the branch** (`dolt_branch('-D', …)`), uncommitted
  rows and all.
- **Every draft operation runs in the `datalib_contacts` applet**, the
  store's one writer: cutting the branch, autosaving, the stale check,
  saving, deleting the branch. A process that moves a ref is a writer
  (AGENTS.md § "Doltlite"). The applet changes branch per request with
  `dolt_connect_branch`, which writes nothing, and ends each request
  back on its writer branch.

## Saving

One transaction in the applet, on its writer branch:

1. Commit the draft's uncommitted rows on the draft branch. A merge
   takes a branch's commits, not its uncommitted rows.
2. `BEGIN`, then `dolt_merge('--squash', 'draft/<id>')`. A squash
   lands as one commit with one parent, so the published history has
   one commit per save, and undo works per save.
3. Where the draft and the published state changed the same cell,
   take the draft's side: the person saving is the last writer.
   Doltlite then puts the whole row in conflict, and the table holds
   the published side. For each conflicting row the applet writes the
   cells the draft changed, from the conflict table's `their_<col>`
   (doltlite's name for the merged-in branch, the draft), and deletes
   the conflict rows. Not `dolt_conflicts_resolve('--theirs')`: it
   takes the whole row, and would lose a published change to another
   cell of it. So a rename in the card and a note changed elsewhere
   both survive, and which cells the draft wins is the same pure
   function the card uses below.
4. Seal (`commit_run`), which publishes, and delete the draft branch.

**A person never overwrites a change they have not seen.** The save
request carries the published commit the card last showed. If the
published state has moved since and the move touched this contact, the
applet saves nothing and answers with the new state (the `If-Match` /
`412 Precondition Failed` pattern of HTTP), and the card shows the
change as below. Saving again then knowingly keeps the draft's values.

## Seeing another writer's change

**The push.** `datalib-http` already watches the data root and pushes
payload-free "ask again" frames over one SSE stream
(`http/src/watch.rs`, `ui/src/live.ts`); the grid index's frame fires
only when its head commit moves. The contacts store gets the same: a
`LiveTable` variant (both halves by hand) sent when the published head
moves. On it `people.revalidate()` redraws every chip, and an open card
refetches.

**A card being viewed** redraws in place.

**A card being edited** compares, for each field, three values: what
it was where the draft was cut (`dolt_merge_base`), what the draft has
now (a diff to `'WORKING'`), and what is published now. A pure function
of those three decides each field:

| draft changed it | published changed it | the card |
|---|---|---|
| no | no | unchanged |
| no | yes | shows the published value; saving keeps it |
| yes | no | shows the draft's value |
| yes | yes, to something else | marks it: "changed elsewhere to *X*", with *use theirs* and *keep mine* |

That is the same three-way merge the save does, shown before it
happens. The card cannot pull the published change into the draft
itself: doltlite refuses a merge into a branch with uncommitted rows.

A link made in the popover or by an agent while the card is open
changes `handles`, not the draft, and shows on the card at once.

## Undo

Each save is one commit, so undo is `dolt_revert` of it: a new commit
that puts back what the save changed, leaving later commits alone.
Doltlite refuses it when a later commit changed the same rows, and the
card says so rather than guessing. A contact's history is the commits
that touched its rows. A draft of the store's `history` and `revert`
and their routes is on the branch `claude/contact-card-wip`; its
doltlite facts have landed.

## The export

JSON of every table, and a vCard per person leaving out handles that
stopped working, so what a person wrote is readable without doltlite.
The vCard writer is also what publishing needs.

## Publishing to CardDAV (later)

A contact can be written to a CardDAV address book. CardDAV versions
each card with an ETag and takes `If-Match` on a write, so publishing
uses the same rule as saving: a card changed upstream since we last
read it is refused, re-read, shown as above, and written again. It
needs a writable account, chosen on purpose: the `contacts` provider
only ever reads.

## Routes to add to the applet

| route | does |
|---|---|
| `POST contact/{id}/draft` | cut the draft, or return the one there is |
| `PATCH contact/{id}/draft` | autosave: set fields on the draft |
| `GET contact/{id}/draft` | the base, draft and published values, per field |
| `POST contact/{id}/draft/save` | save, carrying the published commit the card showed |
| `DELETE contact/{id}/draft` | discard |
| `GET contact/{id}/history`, `POST revert` | a contact's saves; undo one |
| `GET export` | the JSON and vCard export |

## Open questions

- **Are links part of the draft?** Proposed: no. A link changes who a
  chip names everywhere, so it stays an immediate operation, as in
  the popover, and the card's draft holds only fields, name, note and
  photo ([Fields and links are separate](#fields-and-links-are-separate)).
- **When does an abandoned draft go?** A list of drafts, and an expiry,
  or neither until drafts pile up.

## Order of work

1. **The facts** (#1062, #1065).
2. **The store**: the `fields` table (a ladder rung), drafts, save,
   history and revert.
3. **The applet's routes and the live frame.**
4. **The card as a viewer**, which needs nothing above: the chip's
   double-click opening it (`onChipDblClick` in `ChatBody.ce.vue`,
   `onDblClick` in `GridCard.ce.vue`, `personSource` in
   `cardSources.ts`), the person column of `docs/dev/chips.md`
   § "Clicks". An e2e spec on the `contacts` project's root.
5. **The card as an editor**: drafts and the three-way marks, copying
   from sources.
6. **The export.**
7. **Publishing to CardDAV.**
