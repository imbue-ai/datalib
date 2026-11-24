# People: a simpler contacts UI

*Proposal (2026-11-24); nothing here is built. What the screens do was
seen that day by using the TNG demo. The numbers are counts from clones
of one real library's contacts store and grid index. If this is agreed,
it replaces [`contact_linking.md`](contact_linking.md)'s "Where links are
made" and most of [`contact_editing.md`](contact_editing.md).*

## The problem

Someone opens a library to ask "who is Picard, and what do I have from
him?" What they meet:

- **His address-book card is not a contact.** Its page offers *New
  contact "Jean-Luc Picard"*. The demo indexes nine address-book
  entries, and *Your contacts* starts empty.
- **A contact starts with one handle.** Making one from his card links
  the address clicked and none of the others on it. *Everything with*
  that contact finds 2 rows; his other address finds 22, his number 14.
- **A click on a person does not open the person.** It does nothing in
  some places and opens *Unlink* and *No longer works* in others,
  the contacts list included. Opening takes a double-click or a menu.
- **The card speaks the data model**: "1 source holds its handles",
  "Linked, not a field: … Add as a field", "Link its 2 other handles
  to …", "Copy to …".
- **The list is at** Sources → Applets → `datalib_contacts`.
- **Review shows eleven controls** over an empty table.

Seven ideas are on screen (handle, contact, source's record, link,
field, label, draft) where the person has one.

## What is used

In that library, after a month:

| thing | rows | by hand |
|---|---|---|
| contacts | 210 | 8 |
| links | 496 | 12 |
| `real` labels | 751 | 0 |
| any other label | 0 | |
| fields, photos, group members, "this is me", turned-down links, "no longer works" dates | 0 | |

An agent made the rest, and nobody has reviewed any of it.

## The idea

**There are people, and each has addresses.** An address is anything a
source knows someone by: an email, a number, a Slack account. The
screens use those two words, and "source". `handle` and `link` stay in the code.

- **Everyone the library knows is a person**, with nothing to create
  first. Today's *contact* becomes what you have said about a person:
  a name, a note, and which addresses are theirs.
- **Addresses one source's record ties together are one person**, with
  nobody confirming it: a card's two emails and a number, a Slack user
  and the email on their profile. This reverses "no source links
  anything by itself" (`contact_linking.md`).
- **A person's word wins.** "Not the same person" on an address splits
  it off for good; "Same person as…" joins two. Both are today's links
  and `not_linked` rows.
- **Nothing re-renders.** This still sits over the view.

### Does the rule work?

| | fixture | real library |
|---|---|---|
| addresses (less `me:` and `ai_model:`) | 77 | 1,182 |
| people by the rule | 61 | 1,010 |
| most addresses in one person | 5 (Picard, all his) | 2 |
| contacts with two or more known addresses, already whole by the rule | | 73 of 145 |
| people the rule makes out of two different contacts' addresses | | 2 |

Every join in the fixture is right. The real library has few
address-book cards, so one risk is unmeasured: **a shared address chains
people together**, as a household's landline on two cards would. So an
address that two records of one source hold, with no name in common,
joins nothing, and shows on both people, marked shared. Neither library
has one, so step 3 adds one to the fixture. And the rule never joins two
people you have contacts for; it asks.

## The screens

1. **People**, a tab in the sidebar: one list of everyone, the most
   recently heard from first, searchable by name or address. Columns: the
   person, their addresses, sources, how many items, the last one.
   People only an automated sender made are hidden by default, which is
   what `real` is for.
2. **A person.** Name, photo and addresses at the top, each with its
   source's mark; *Everything with them*; then one line per source, which
   opens to what that source says. A pencil edits the name and the note. An
   address's menu has *Not the same person* and *Copy*.
3. **A chip.** A click opens the person, everywhere. There is no "+" and no
   popover. The right-click menu stays.
4. **An address book's own page** is that source's record, and its chip
   opens the person. It offers nothing to create.
5. **Review** is a banner on People, there only when an agent's
   proposal waits: "12 suggestions", each a card with *Yes* and *No*.

## What goes, what stays

| | proposal |
|---|---|
| contacts, links, `not_linked` | stay, as what you said |
| agents propose, a person decides | stays; the review shrinks to *Yes* and *No* |
| `real` | stays, shown as *Hide this sender*; other labels go |
| *This is me* | stays, on the person's menu |
| merging | stays, as *Same person as…* |
| fields, drafts, copying from a source | go: a source already says these, and an edit to the name and note saves as you type |
| "no longer works" dates | go |
| group contacts and members | go: never built past the table |
| a photo of your own | goes; a source's photo is drawn |

A table goes only where its root has no rows in it, and the rung that drops
one refuses otherwise and says how to export it. Every table going is
empty in the library measured; other roots are not known.

## What the data needs

- **Who is one person** is worked out at the end of each index pass,
  where the handles are already read whole (`write_people`), into one
  table of address and person. A person's id there is not stored
  anywhere else; one you have said something about keeps its contact id.
- **How much, and when last, come from the grid's rows**, so a person's
  count is what a search for them finds. Today each document's record
  of a person carries its own count, `seen`, which is why one person had
  31,303 copies (#1999). Counting every author from `grid_rows_by_author`
  takes 24 ms warm for 1,211 authors. *When last* needs the stamp in that
  index: without it the same read took 29 s. The two counts agree for 62 of
  74 people in the fixture; the other 12 have more grid rows, for a reason
  not yet looked into.
- **`seen` then goes** from the records and the renders, and a
  document's row in the index says only who is in it.
- **One forged message must still not rename anyone.** A name is
  weighed today by the items written under it, from `seen`. The grid has the
  same thing: how many rows each author name has.

## Order of work

Each step stands alone, and the first two change no stored data.

1. **Clicks, place and words.** A click opens the person; People is in
   the sidebar; the card loses its data-model sentences.
2. **Everyone is listed.** People reads the index as well as the
   contacts app. The count and last date come from the grid.
3. **Addresses join by themselves**, with *Not the same person*.
4. **The review banner.**
5. **What goes, goes**, with the store's rung, the routes
   and [`contacts_app.md`](../../user/contacts_app.md).
6. **`seen` goes.** It renders every source again, once.

## To decide

- **Does an agent's unreviewed link count as yours?** Today it draws on
  chips at once. Here: yes, as now, with *Not the same person* one click
  away.
- **Is a photo of your own worth keeping?** Nobody has set one.
- **What does *Everything with them* search for** when there is no
  contact? Proposed: `with:` one address, and the search follows the
  same table to the rest, as it does for `with:contact:<id>`.
- **Without the contacts app**, does grouping still work? Proposed: yes,
  since it comes from the sources; only editing needs the app.
