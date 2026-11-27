# Contacts: a simpler UI

*Plan (2026-11-24, revised 2026-11-25 after Thad's notes and answers).
Steps 1 to 5 of §"Order of work" are built, and
[`../contacts.md`](../contacts.md) says how they work; the rest is not. "The problem" is as it was before. What the screens do was seen by using the TNG
demo. The numbers are counts from copies of real libraries and from one
real address book. If this is agreed, it replaces
[`contact_linking.md`](contact_linking.md)'s "Where links are made".
[`contact_editing.md`](contact_editing.md) (editing a contact, and later
publishing it) still stands, but for "Fields and links are separate"
(§"Editing a contact").*

## The problem

Someone opens a library to ask "who is Picard, and what do I have from
him?" What they meet:

- **Making a contact from his card takes one of its handles.** His
  address-book card lists two addresses and a number. *Create contact*
  takes the address that was opened and leaves the other two.
  *Everything with* the new contact finds 2 rows; his other address
  finds 22, his number 14. His card itself still shows a grey
  "Jean-Luc Picard +" afterwards, as if he were nobody, because the
  card's heading goes by the other address.
- **A click can change something.** A single click on a person in a
  document, on a person's page or in the contacts list opens a box with
  *Unlink* and *No longer works*. In a search grid the same click only
  selects the row.
- **The page speaks the data model**: "6 sources hold its handles",
  "Linked, not a field: … Add as a field", "Link its 2 other handles
  to …", "Copy to …".
- **The contacts list is at** Sources → Applets → `datalib_contacts`.
- **Two people cannot share an address.** A couple with one mailbox
  cannot both be contacts with it: the second is refused.
- **A number without its country code is not a handle**, so it cannot
  go on a contact. In a real address book of 91 cards, that is 7 of the
  13 numbers.
- **Review is a wall of controls.** It is where a person agrees or
  disagrees with what an agent suggested. With nothing suggested it
  still shows a filter box, two checkboxes, two menus, four buttons and
  a label box, over an empty table.

## The idea

**Contacts are the people you put there.** The list starts empty and
nothing fills it: not a source, not an address book, not an index pass.
Everyone else in the library is a handle, under the name a source gives
it, as now.

What changes is that putting someone there is one act and gets all of
them, and that taking something back is remembered:

- **Add to contacts takes every handle we know is theirs.** From an
  address-book card, all it lists. From a chip in a message, that handle
  and every other handle on any record that holds it: a Slack profile's
  email, the number on a card. They are shown ticked, and you untick
  what is wrong before adding.
- **A handle can be on several contacts** (§"Shared handles").
- **"Not theirs" is remembered.** Taking a handle off a contact records
  that, and neither Datalib nor an agent offers it for that contact
  again. Adding it back clears the record. "Merge into…" joins two
  contacts that are one person. Today's `not_linked` rows already hold
  the first; it would now cover a handle you added yourself too.
- **A contact is yours to edit**, and the edits are meant to be published
  back to an address book one day (§"Editing a contact").
- **Nothing re-renders.** Contacts still sit over the view.

How good are the offers? In the one real library with many contacts, 145
contacts had two or more handles the sources know. The offer would have
been complete for 73 of them, and twice it held another contact's
handle. (Counted for the first draft's rule, which also followed a
record's handles on to further records; no one there has more than two,
so the two rules agree.) A handle already on another contact is offered
**unticked**, saying whose it is.

### Words

| on screen | means | today |
|---|---|---|
| **contact** | a person (or an organization) you added | contact, "your contact" |
| **handle** | an address, a number, a Slack account: anything a source knows someone by | handle |
| **add**, **not theirs** | put a handle on a contact, take it off | link, unlink |
| **detail** | a line you can edit: a title, a birthday, a street address | field |
| **unfiled** | someone the sources know who is not in your contacts | unlinked |
| **what Slack says** | a source's own record of them | source's record, "source holds its handles" |

"Link" and "field" are not on any screen. `link` stays in the code.

## Clicks

| | a person, anywhere |
|---|---|
| **click** | selects the row in a grid, and nothing else anywhere. The box a click opens today goes. |
| **double-click** | opens their page. |
| **right-click** | *Open*, *Add to contacts…* (or *Add to a contact…*, to pick one you have), *Everything with*, copy. The same menu in grids, where adding is not offered today. |

The "+" on a chip goes: it looks like a button and is not one. A contact
is still blue and everyone else grey.

## The screens

1. **Contacts**, pinned in the sidebar under Sources: your contacts, with
   photo, name, handles, how many items, and the last one. Sorted by
   **recency**, **frequency** or **alphabetically**; searchable by name,
   handle or note. Empty, it says how to add one. A window that already
   keeps its own tabs is given the new pin once, as the first three
   were.
2. **Unfiled**, a tab beside it: the people the sources know whom
   you have not added, with the same columns and sorts, the most frequent
   first. Select rows, *Add to contacts*. Nothing on it is a contact until
   you say so. Senders you or an agent marked *not real* are left out,
   with a switch to show them.
3. **A person's page**, one page whether or not they are a contact:

   ```
   ◯  Jean-Luc Picard                                     unfiled
      ☑ picard@enterprise.starfleet     tng_contacts, slack, 4 more
      ☑ jlpicard@enterprise.starfleet   tng_contacts
      ☑ +1 202 555 0101                 tng_contacts, 2 more
      ☑ U_PICARD                        slack
      ☑ Jean-Luc Picard                 facebook
      [ Add to contacts ]   or add to… ▾        Everything with them
        ↑ focused: double-click, then Enter, adds them

   ▸ tng_contacts (seen here)   Captain, Starfleet
   ▸ slack                      also called Captain Picard · 4 items
   ```

   Once added, the ticks go, and each handle's menu has *Not theirs*,
   *No longer works* and *Copy*; *Edit* opens the form. A contact with no
   handle has a page too, which it does not today. If you already have a
   contact of that name, *Add to Jean-Luc Picard* comes first.
4. **An address book's own page** is that source's record. Double-click
   its heading for the person's page, with every handle on the card
   ticked. In the address book's list, select rows and *Add to contacts*
   to bring over many at once.
5. **Suggestions** replace Review (§"Agents suggest"): a banner on
   Contacts, there only when an agent's suggestions wait. "12
   suggestions" opens them as cards ("These two are Will Riker, because
   …"), each with *Yes* and *No*, and *Yes to all* for one run of an
   agent.

## Agents suggest

**What an agent proposed is never drawn as what you said.** Today an
agent's contact is in your list, and blue on every chip, before you have
looked at it. Instead:

- **Until you say Yes it is a suggestion.** It is not in Contacts and not
  in `with:contact:<id>`. Its chips stay grey, and the person's page says
  "claude suggests these 3 are one person, because …", with *Yes* and
  *No*.
- **Once you say Yes it is yours**, and the page can still say who
  suggested it. The rows already hold this: `agent`, `reason`, and
  `reviewed_at_utc`, empty until a person looks. No shape changes.
- **No is remembered**, as *Not theirs* is.
- **A suggestion holds nothing.** Adding a handle an agent only suggested
  for someone else asks nothing, and takes it out of that suggestion.

In the library below, 202 of 210 contacts become suggestions, and their
chips go grey, until *Yes to all*.

## Editing a contact

The editor stays as it is built: *Edit* opens a form for the name, the
note, and details each with a label; it saves itself as a draft until
*Save* or *Discard*; *Set photo…* sets a picture. Two things on it
change, because it shows the person one thing as two:

- **An address or a number is one line.** Today it is two: a *field*,
  which is a line of text, and a *link*, which makes its messages
  theirs. So the page says "Linked, not a field: … Add as a field" and,
  of the other kind, "Link it". They were kept apart so that a
  household's number could be on two contacts; a shared handle does that
  now. So a handle on the contact is a line in the form, with a label,
  and an address or number typed into the form is added as a handle on
  *Save*, asking if another contact has it. One that is no handle (no
  country code, no match) stays text.
- **Copying is one button per value**, *Use*, beside what each source
  says, in place of a *Copy to Jean-Luc Picard* section under each.

Adding someone brings over the name and the handles with their labels
("work", "cell"). It leaves a card's other details on the card, until you
*Use* or change one.

**Publishing back** stays later, and what it needs is kept: which source
and which of its records a value came from (`copied_from_source` and
`copied_from_key`) is now also written for the handles added from a
card, so a contact knows its card. It publishes a contact's name, note,
photo, details, and those handles a card can hold: an address or a
number, not a Slack account. An agent's suggestion is never published,
since it is not a contact.

## Shared handles

Robert and Marie Picard share `vineyard@labarre.earth`. Both can be
contacts, and both have it.

- **Adding a handle another contact has asks**: "This is on Robert
  Picard. *Both* or *Move it here*."
- **Its chip is still one chip.** A chip names a handle, and this one has
  two names: "Robert Picard & Marie Picard", or "Robert Picard & 2
  others". It is blue, led by the mark for its kind (no one photo is
  right), and its tooltip lists everyone who has it.
- **Double-click opens the address's own page**, with Robert and Marie on
  it as two chips where an unfiled handle has *Add to contacts*. A handle
  one contact has opens that contact, as ever.
- **Its menu's *Everything with* searches the address.** Marie's own page
  searches all her handles, so it finds Robert's mail from this one too,
  and says the address is shared.
- **A message's own header** shows the name that message gave, as today,
  so most of Marie's mail says Marie. Nowhere else tries to guess which
  of them wrote.
- **An agent may not share one.** A suggestion for a handle that another
  contact has is skipped, as today.

The store's `handles` table is keyed by handle alone; it becomes
(handle, contact), with a rung, and *No longer works* is then on one
person's use of it. `POST /resolve` answers a list, and `Who.mine`
becomes one, possibly empty; a chip's look, tooltip and menu read the
list, so nothing is drawn by a second path. This is also what group
contacts were for ("a shared address belongs to a group contact"), which
were never built past the table.

Neither the address book nor the library measured holds a handle that
two differently named records share, so the TNG fixture gets the two
Picards.

## Numbers without a country code

Today `(202) 555-0101` on a card is kept as text and reaches nobody,
rather than being given a country by guess. Nothing is set anywhere to
change that; **the library says which number it is.** When you add
someone, a number like that is compared with every full number a source
knows, by its digits less any leading zero. `+1 202 555 0101` ends in
the same digits, so it is offered as theirs, ticked, with the card's
spelling beside it. With two matches, in two countries, both are
offered unticked. With none, the number stays text: no source has a
message from it, so a handle would reach nothing.

In the real address book, each of the 7 numbers matched exactly one of
the 19 full numbers two real libraries know, and no two of the 19 could
be confused. That is a small sample.

The match also runs the other way: the page for `+1 202 555 0101` shows
the card under "same digits", so a card with nothing but that number is
found at all. For that the index keeps such a number's digits beside the
handles, which is a rebuild of the index and no render; and a card with
no handle gets a chip naming the card itself, where its heading is plain
text today.

## What goes, what stays

What a month's use left in the one real library with many contacts:

| thing | rows | by hand |
|---|---|---|
| contacts | 210 | 8 |
| links | 496 | 12 |
| `real` labels | 751 | 0 |
| any other label | 0 | |
| fields, photos, group members, "this is me", turned-down links, "no longer works" dates | 0 | |

An agent made the rest, and nobody has reviewed any of it.

| | proposal |
|---|---|
| contacts, links, `not_linked` | stay |
| agents suggest, a person decides | stays, as *Yes* and *No* |
| `real` | stays, shown as *Hide this sender*; other labels go |
| *This is me* | stays, on the page's menu |
| merging | stays, from the list and now from the page |
| details, drafts, copying from a source | stay (§"Editing a contact") |
| "no longer works" dates | stay, in a handle's menu: publishing leaves out an old number |
| group contacts and members | go: a shared handle needs no group |
| a photo of your own | stays |

A table that goes (`members`) goes only where its root has no rows in
it, and the rung that drops one refuses otherwise and says how to export
it.

## How many, and when last

Frequency and recency come from the grid's rows, so a person's count is
what `from:` them finds. Counting every author from
`grid_rows_by_author` takes 24 ms warm for 1,211 authors. The last date
needs the stamp added to that index: without it the same read took 29 s.

Each document's record of a person carried a count of its own, `seen`,
which counted messages and left out reactions, edit versions, comments,
and everything from a source whose records counted nothing; it
disagreed with `from:` for 27 of the fixture's 119 records. It is gone: the
grid's rows are the one count (built,
[`../contacts.md`](../contacts.md) §"How much someone wrote"), and the
author index carries the stamp. A name is still weighed per document,
so one forged message renames nobody.

## Order of work

Each step stands alone; the first two change no stored data.

1. **Clicks, place and words.** Built (#2010). A click does nothing but
   select, the box and the "+" go, Contacts is pinned, the page loses its
   data-model sentences, and *Not theirs* is remembered.
2. **Adding the whole person.** Built: the ticked handles, numbers
   without a country code, adding many rows from an address book, a page
   for a contact with no handle and for a card with none. A handle
   another contact has cannot be ticked yet (step 3), and the search
   finds a number however it is written, which was not in this plan.
3. **Shared handles**, with the store's rung, then **one line per
   address** in the editor: built. A handle added from a card keeps its
   label and which card it came from; the columns are new, so the open
   adds them and no rung was needed. A handle's label and its × change
   it at once, as a link always has, and Discard does not undo them;
   only what is typed waits for Save. *Use* beside each value replaced
   *Use all*. Adding many rows from an address book takes a card whose
   handle is on a contact for that contact only when the names agree:
   Marie's card, with the address Robert has, is skipped and named, and
   her page is where the person says they share it.
4. **The list**: the three sorts, and *Unfiled*. Built. One row of
   *Unfiled* is a handle, not a person: a person's Discord account and
   their number are two rows unless a source's record ties them, and
   adding them together makes one contact only when they share a name.
5. **Suggestions.** Built. An agent's link nobody said Yes to is left
   out of every read of the person's contacts; no shape changed. The
   suggestions open from a banner on Contacts and on the person's page.
   *Review* went with its tab: *Hide this sender* on *Unfiled*'s
   right-click is where a person says a sender is not real. An agent's
   unconfirmed *not real* still hides a sender from *Unfiled*, as §"The
   screens" says.
6. **What goes, goes**, with the store's rung, the routes and
   [`contacts_app.md`](../../user/contacts_app.md).

## Decided

- A double-click on someone not in your contacts opens their page with
  *Add to contacts* focused; it adds nobody.
- What an agent proposed is kept apart from what a person said, and is
  not a contact until a Yes.
- *Unfiled* is wanted, sorted by frequency and recency.
- No country is set anywhere for phone numbers.
- Editing a contact by hand stays, toward publishing it back to a source.
- An address on a contact is one line, both written there and counted as
  theirs. (Reverses October's "fields and links are separate". Given up:
  an address written on a contact whose messages are not theirs, such as
  a switchboard's. If that comes up, it is one option on that line.)
- A chip that several contacts share is one chip with their names joined.
