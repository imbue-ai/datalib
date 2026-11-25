# Contacts: a simpler UI

*Proposal (2026-11-24, revised 2026-11-25 after Thad's notes on it);
nothing here is built. What the screens do was seen by using the TNG
demo. The numbers are counts from copies of real libraries and from one
real address book. If this is agreed, it replaces
[`contact_linking.md`](contact_linking.md)'s "Where links are made" and
most of [`contact_editing.md`](contact_editing.md).*

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
2. **Everyone else**, a tab beside it: the people the sources know whom
   you have not added, with the same columns and sorts. Select rows,
   *Add to contacts*. Nothing on it is a contact until you say so.
   Senders you or an agent marked *not real* are left out, with a switch
   to show them.
3. **A person's page**, one page whether or not they are a contact:

   ```
   ◯  Jean-Luc Picard                        not in your contacts
      ☑ picard@enterprise.starfleet     tng_contacts, slack, 4 more
      ☑ jlpicard@enterprise.starfleet   tng_contacts
      ☑ +1 202 555 0101                 tng_contacts, 2 more
      ☑ U_PICARD                        slack
      ☑ Jean-Luc Picard                 facebook
      [ Add to contacts ]   or add to… ▾        Everything with them

   ▸ tng_contacts (seen here)   Captain, Starfleet
   ▸ slack                      also called Captain Picard · 4 items
   ```

   Once added, the ticks go, and each handle's menu has *Not theirs* and
   *Copy*; a pencil edits the name and the note. A contact with no
   handle has a page too, which it does not today. If you already have a
   contact of that name, *Add to Jean-Luc Picard* comes first.
4. **An address book's own page** is that source's record. Double-click
   its heading for the person's page, with every handle on the card
   ticked. In the address book's list, select rows and *Add to contacts*
   to bring over many at once.
5. **Suggestions** replace Review: a banner on Contacts, there only when
   an agent's suggestions wait. "12 suggestions" opens them as cards
   ("These two are Will Riker, because …"), each with *Yes* and *No*,
   and *Yes to all* for one run of an agent.

## Shared handles

Robert and Marie Picard share `vineyard@labarre.earth`. Both can be
contacts, and both have it.

- **Adding a handle another contact has asks**: "This is on Robert
  Picard. *Both* or *Move it here*."
- **A message's own header** shows the name that message gave, as it
  does today, so most of Marie's mail says Marie.
- **Anywhere else its chip shows both names**, "Robert Picard · Marie
  Picard", drawn as an untitled conversation's people are now. Each name
  opens its own page.
- **Search cannot tell them apart.** *Everything with Marie* finds
  Robert's mail from that address too, and her page says the address is
  shared.
- **An agent may not share one.** A suggestion for a handle that another
  contact has is skipped, as today.

The store's `handles` table is keyed by handle alone; it becomes
(handle, contact), with a rung. `POST /resolve` answers a list. This is
also what group contacts were for ("a shared address belongs to a group
contact"), which were never built past the table.

Neither the address book nor the library measured holds a handle that
two differently named records share, so the TNG fixture gets the two
Picards.

## Numbers without a country code

Today `(202) 555-0101` is kept as text and reaches nobody, rather than
being given a country by guess. Instead, a source that holds numbers
typed by hand (an address book, an SMS backup) gets one setting: **the
country its numbers are in when they do not say**. Set, its render reads
them as that country's, so the card joins the same handle as the texts.
Unset, nothing changes. A number that is not valid there stays text.

That needs each country's rules for dialling prefixes (a leading 0 is
dropped in France and kept in Italy), which is a library's job: a port
of Google's libphonenumber, if its license passes `deny.toml`. Setting
it renders that one source again.

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
| fields, drafts, copying from a source | go: the page shows what each source says, and the name and note save as you type |
| "no longer works" dates | go |
| group contacts and members | go: a shared handle needs no group |
| a photo of your own | goes; a source's photo is drawn |

A table goes only where its root has no rows in it, and the rung that
drops one refuses otherwise and says how to export it.

## How many, and when last

Frequency and recency come from the grid's rows, so a person's count is
what `from:` them finds. Counting every author from
`grid_rows_by_author` takes 24 ms warm for 1,211 authors. The last date
needs the stamp added to that index: without it the same read took 29 s.

Today each document's record of a person carries a count of its own,
`seen`. The two counts agree for 62 of 74 people in the fixture; the
other 12 have more grid rows, for a reason not yet looked into, which
comes first. Dropping `seen` altogether is worth doing but nothing here
needs it. It also weighs which name a person goes by, so that one forged
message renames nobody, and that would have to move to the grid's rows.

## Order of work

Each step stands alone; the first two change no stored data.

1. **Clicks, place and words.** A click does nothing but select, the box
   and the "+" go, Contacts is pinned, the page loses its data-model
   sentences.
2. **Adding the whole person**: the ticked handles, adding many rows from
   an address book, a page for a contact with no handle.
3. **Shared handles**, with the store's rung.
4. **The list**: the three sorts, and *Everyone else*.
5. **Numbers without a country code.**
6. **Suggestions.**
7. **What goes, goes**, with the store's rung, the routes and
   [`contacts_app.md`](../../user/contacts_app.md).

## To decide

1. **Does a double-click on someone not in your contacts add them?**
   Proposed: no. It opens their page with *Add to contacts* focused, so
   double-click then Enter adds them. A double-click that added would
   put a spammer you only wanted to look at in your contacts, and
   opening a card never writes (`AGENTS.md`, "Card source is data").
2. **Is an agent's contact yours before you say Yes?** Today it is listed
   and draws on chips at once. Proposed: no, since that fills your
   contacts for you. The cost: 202 of the 210 contacts above turn into
   suggestions, and their chips go grey, until *Yes to all*.
3. **Is *Everyone else* wanted**, or is adding from chips and address
   books enough? Proposed: wanted; sorted by frequency it is where the
   fifty people you hear from most get added.
4. **Do fields and photos of your own go?** Proposed: yes, as nobody has
   used either, knowing that writing contacts back to CardDAV
   ([`contact_editing.md`](contact_editing.md)) would need fields again.
5. **One country for the library, or one per source?** Proposed: per
   source, so that changing it renders only that source.
