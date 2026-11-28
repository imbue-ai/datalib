# Chips: a link the app resolves, drawn as what it names

A **chip** is how Datalib shows something inline: a person, a source
group, a pipeline step, a document, a date, a label. It has a name, a picture or
a mark, and behind it a stable identifier the app looks up to learn what
that thing is *now*: the contact a person linked, the name the config
gives a group today, whether a step is running. A chip is drawn the same
in a rendered document, a grid cell and the run log, with the same
tooltip, right-click menu, double-click and copied text.

What a document promises any viewer (the link a person is written as,
the `<time>` a date is) is
[showing a rendered document in your app](../user/rendered_documents.md);
this page is how Datalib's own viewer keeps it, and where to start to add
a kind of chip or a place that draws them. What makes a *person* who
they are (handles, each source's record, the contacts app) is
[`contacts.md`](contacts.md).

## Words

| word | means |
|---|---|
| **entity** | what a chip names: a person (by a handle), a group, a step, a document, a node, a date, a label |
| **chip link** | how a chip is written down: a markdown link with a title (below) |
| **entity URI** | the href, which names the entity (see [The link](#the-link)) |
| **resolver** | the one place in the page that asks the server what an entity is now, for one kind of entity |
| **producer** | whoever writes a chip down: a render writing markdown, or a server sending a grid row |

## The link

**A chip is a markdown link whose href the app knows how to resolve.**
In the markdown it is an ordinary link, so any markdown viewer shows a
named link with a tooltip; Datalib recognizes the href and draws a
chip. Nothing about how the chip looks is written down. The link
carries only what the producer knew when it wrote it:

```markdown
## [Will Riker](mailto:riker@enterprise.org "Will Riker <riker@enterprise.org>") <time …>…</time>
# [slack](datalib:group/slack "slack") — storage
| Store | [ingest](datalib:step/slack/ingest "slack/ingest") | …
```

- **text** — the name to show until the chip resolves: what the
  source showed for a person, the id for a group, the function for a
  step.
- **href** — the entity URI.
- **title** — the hover any other viewer shows; Datalib shows a live
  tooltip instead.

The URI for each kind of handle, and the trust rule a viewer keeps, are
in [the document contract](../user/rendered_documents.md#a-person). The
backend writes a handle's URI with `Handle::to_uri`; one of your
contacts is `datalib:contact/<id>` (`datalib_contacts::contact_uri`,
read by `contactFromUri` in `chipLinks.js`), and whoever a contact's
page is about is `datalib:contact_page/<page>`
(`datalib_contact_schema::contact_page_uri`, `contactPageFromUri`); a
group is
`datalib:group/<id>` and a step `datalib:step/<step id>`
(`datalib_columns::Entity::uri`); a document is `datalib:doc/<uuid>`
(`datalib_uri::Name::doc`, `ui/src/uri.ts::writeDoc`), and a node of a
source's tree `datalib:node/<uuid>` (`datalib_uri::Name::node`,
`ui/src/uri.ts::writeNode`). Those three kinds, the library a copied one
carries, their `/name/` addresses and how the desktop app opens one are
in [`datalib/backend/uri/README.md`](../../datalib/backend/uri/README.md).

Two functions in `datalib_etl_render::message` write every chip link
the backend writes, so the escaping lives in one place:
`chip_link(shown, &handle)` for a person, whose title is
`Handle::describe` (`Will Riker <riker@enterprise.org>`), and
`entity_link(text, uri, title)` for anything else. The UI reads and
writes the same URIs in `ui/src/cards/chipLinks.js` (`handleFromUri`,
`uriFromHandle`, `entityFromUri`, `uriFromEntity`); both sides are
tested over the same cases, so they cannot drift apart without a test
failing.

Who writes chips: calendar-common for an event's organizer and
each guest with an address; forge-render-common for a pull or merge
request's author and each comment's; Notion for a comment's author and
a user mentioned in a page; Google Drive for a person chip in a Doc;
Todoist for a task's assignee; chat-common for a message's author,
recipients and reactors; Slack for a `<@U…>` mention in a body, and email for an `@`
mention; contact-common for
the person a contact's page is about, as its heading, and for each
member of a group of contacts that has a handle; the storage report
(`datalib_step/src/introspect.rs`) for its group in the heading and
each store's step in a column.

## Drawing a chip

1. **Marking.** The viewer's markdown-it plugin (`chipLinks.js`, run by
   `renderDocument.ts` and by the render preview) gives an explicit
   link whose href names a handle `class="chip" data-handle="<handle>"`,
   one naming a group or step `class="chip" data-entity="<uri>"`, and
   one naming a contact's page `class="chip" data-contact-page="<page>"`.
   A link `linkify` made from a bare address in running text is left
   alone, so a signature's address stays an address. The plugin signs
   each mark with a value fresh for that render, and the sanitizer
   (`sanitize.ts`) takes `class="chip"` and each of those attributes
   off anything not signed, so a body's own HTML
   cannot claim to be a chip. It admits the `slack:` and `datalib:`
   schemes beside the web, mail and phone. A handle holding a bidi
   control or an invisible character is not one: two handles that look
   alike would draw as one person.
2. **Asking.** The surface asks the resolver for that kind what each
   chip names ([Resolving](#resolving)).
3. **Drawing.** `drawChip(el, look)` in `contacts.ts` draws a chip from
   a `ChipLook`: its text, its aria label, its classes, its tooltip
   (`title`), and its lead — a photo if there is one, else a
   contact's initial in a disc, else a bundled mark for the icon token
   (`email`, `slack`), else a pipeline glyph (`step:ingest`). The lead
   is decoration: hidden from screen readers and not copied. A photo
   the browser cannot draw falls back to what the chip draws without
   one. A contact you made with no photo of its own shows the first
   one its handles' records have. The name is its own element
   (`.chip-text`), and a chip is never wider than what holds it: short
   of room, its name ends in "…" and the tooltip has it whole. In a
   wrapping row, such as a message header, it moves to a line of its
   own before it shrinks.

What goes into the look is each kind's own rule, pure and unit-tested:

- **A person** — `chipLook` and `chipTooltip` in `contacts.ts`: the
  contact's name if a person linked one, else the best-ranked source's
  name, else what the source showed. A message's own author, in its
  header, shows the name that message gave instead of the source's
  (see [Trust](#trust)). A name that spells out an address or another
  handle the person does not hold is never shown; the chip shows its
  own identifier instead. A chip not on a contact shows its kind's mark.
  One that is on a contact of yours is blue, and one that is not is
  grey (`chip.css`). A chip of the contact you marked *This is me*,
  whichever of its handles it is drawn by, carries a small "me" after
  its name (`ChipLook.me`, `.chip-me`): decoration, so it is no part of
  the name, the accessible label's name or a copy. A list of one
  person's handles (`identifierLook`) leaves it off, since every line
  there is theirs. An address several of your contacts share is still
  one chip, blue, named for them all ("Robert Picard & Marie Picard",
  "Robert Picard & 2 others") and led by its kind's mark, since no one
  photo is right; its tooltip lists them, and its double-click opens the
  address's own page. [`contacts.md`](contacts.md) §"In the UI" has the
  ranking.
- **A group or a step** — `entityLook` and `entityTitle` in
  `entities.ts`: the name the config gives it now (a step is named
  under its group: "Work Slack · Ingest"), or the producer's until the
  answer lands; its type's mark or its phase's glyph; and a dot after
  the name for a status worth noticing — running, waiting, queued,
  failed, blocked or interrupted. The words are in the tooltip. Where
  the context already names the group — a step's row under it in the
  Sources table, its section of the group's dashboard — the chip keeps
  the producer's short name ("Download") and the tooltip carries the
  whole one (`keepShown`, the grid's `chips.keepLabels`). Where a
  status is drawn right beside the chip, as in the Dashboard's rows, the
  chip drops its dot (`quiet`).

`chip.css` is the one look. The document frame, the search and
problems grids, the log card, the Sources card, a source's card,
the Dashboard's sections and the render preview each include it in
their own root, since a card's shadow root and the document frame see
no page styles.

## Trust

**A chip shows what its href resolves to, never the link text**;
[the contract](../user/rendered_documents.md#trust-the-href-never-the-text)
says why. So a body's `[Picard](mailto:phisher@romulan.net)` is a chip
for the phisher, under the name the sources know them by, with "Shown
here as “Picard”" in its tooltip, and a link to `datalib:group/slack`
shows that group's real name. Forging a link can only point at a real
entity under its real name, which is what any link already does.

**One exception: a message's own header shows the name that message
gave its author**, as any mail client does, so a forged `From:` shows
on the forged message and on no other. The source's name for the
person is decided across everything they wrote, not by any one message
([`contacts.md`](contacts.md) §"In the UI"). The viewer knows a header
by its place: `renderDocument.ts` renders what is inside each message's
box alone, and `chipLinks.js` marks a link opening
that fragment's first line, an `h2`, with `data-chip-header`. Nothing a
message carries comes before its header, and the sanitizer takes the
attribute off any link the plugin did not sign. A body rendered whole,
as one string rather than as its pieces, has no header chips.

## Resolving

`ui/src/cards/resolver.ts` is the one place the page asks what a chip
names. There is one `Resolver` per kind of entity, and every document
and grid in the page shares it:

- **`lookup(key)`** returns the answer so far and asks if nobody has.
  A surface calls it as it draws. Every question asked while one pass
  of drawing runs goes to the server as **one request**.
- **`ask(keys)`** asks and waits; a document's decorate pass uses it.
- **`subscribe(listener)`** tells a surface which keys' answers
  changed, so it redraws those chips and nothing else.
- **`dropAnswers(keys)`** drops answers after an edit made in this
  page, so every surface draws them again. An answer to a question asked before
  the edit is thrown away when it lands.
- **`revalidate()`** asks again about everything held, keeping each
  old answer drawn until its new one arrives, and tells subscribers
  only about answers that changed or vanished. That is how a chip
  follows something that moves on its own without flickering. A question
  already out may have been answered before whatever prompted this, so
  it is asked once more when it lands; `ask` waits for that answer too.

The two instances:

| | `people` (`contacts.ts`) | `entities` (`entities.ts`) |
|---|---|---|
| keyed by | handle (`email:…`) | entity URI (`datalib:group/…`) |
| asks | the unified grid and search index's `POST /people` and the contacts app's `POST /resolve` | datalib-http's `POST /api/entities` (`manage::post_entities`, from the rows the Manage table reads) |
| answer | each source's record of the person, and the contact a person made | label, icon token, detail, status, and its row of the Sources table (`row`), whose actions are its menu |
| goes stale when | a person edits a contact (create, link, unlink, no longer works): in this page the call drops the answers for the handles it changed (`forget`); from anywhere else — another window, an agent — the contacts app's commit reaches every page as a `curated` frame, on which `people` and `contactsById` revalidate (`movesPeople`), as on a resync | a sync moves a status, or the config changes: `entities` follows the live connection and revalidates on every frame `movesEntities` names, and on a resync |

Why the viewer joins this itself rather than the producer sending
finished chips: a contact a person linked and a step's status are live
state that moves while the row does not. The producer still resolves
what only it can (the name a source showed, the name the config gives a
group). Joining where the state is live is what lets one link made in a
document redraw every grid already open beside it. It is the one join
the viewer does.

## In a grid

A grid draws a chip from an `identity` typed cell,
`{id, label, icon, detail, entity}`:

- **A person**: the cell's `id` is the handle as a URI and its `label`
  the name the source showed. The search grid's Author column is one
  (`author_ref`, built by `columns.rs::author_identity` from
  `grid_rows.author_handle`), and so is its Contact column on a
  contact's own row (`contact_ref`, built by
  `columns.rs::contact_identity` from `grid_rows.contact`, `email` and
  `phone`, else any handle `contact_record_handles` holds for the
  contact). An identity cell whose `id` is no handle is drawn as plain
  text: a group of contacts.
  One whose `id` is a `datalib:contact_page/` URI, a person whose card
  has no handle, is drawn as that page's chip ([below](#a-contacts-page))
  when the grid is given `chips.page`.
  One whose `id` is a `datalib:contact/` URI is drawn as that contact's
  chip when the grid is given `chips.contact` (the contacts app's table,
  for a contact with no handle).
- **Several people**: an `identities` cell holds a list, each drawn as
  above. The search grid's Subject column is one (`subject_ref`, built
  by `columns.rs::subject_identities`): a contact's own row is its
  Contact chip, a conversation named by its people is a chip for each
  of them, and anything else is its conversation's name. Who a
  conversation is named after is the provider's to say
  (`NormalizedChat::named_after`: a DM's other party, an untitled
  group's members); chat-common writes them as `subject` terms on the
  document's row, and the applet reads them beside the row with the name
  this conversation knows each by, most active first. The search list
  titles a result and its preview by the same people
  (`contacts.ts::peopleTitle`): one person's chip, or for several one
  pill with their photos overlapping and each name a chip of its own.
- **A group or a step**: the cell's `entity` is the URI, *beside* a
  bare `id`. The id stays bare because other code keys on it: the
  document view stores a source's "always load images" setting under
  the Source cell's id. The search and problems grids' Source column
  is one (`Sources::identity` sets `entity` for every source the
  config declares).
- **A document**: the cell's `entity` is its `datalib:doc/<uuid>`, its
  `label` the document's title (`markdowns.title`, the heading its page
  opens with, or "Untitled document"), its `icon` and `detail` its
  source's mark and name. The search grid's Document column is one,
  and so is the problems grid's (`columns.rs::document_identity`, the
  titles read beside the rows by `dolt_repo.rs::document_titles`). It
  is drawn by `documentChip.ts::documentCell` whether or not the grid
  resolves chips, since nothing in it needs resolving (§"A document").
- **A date**: a `datetime` cell (the search grid's Touched, Created
  and Modified) is the date chip a document draws (§"A date"), through
  `cellRenderers.ts::renderDateTime`; a value it cannot read stays
  text. Every grid draws its cells again when the status bar's clock
  changes, so the chip moves with it. A column too narrow for the whole
  date shows `2369-04-14 12:05`, then `2369-04-14`
  (`dates.ts::fittingDateCell`, chosen by a container query in
  `chip.css`); the text a copy or a screen reader gets stays whole.
  The log's Time column is the same chip to the millisecond,
  `2026-09-22 14:07:47.190` (`dates.ts::millisDateCell`), which a
  narrow column cuts from the left so the seconds stay.
  An applet's card has none of this code: the `<time datetime>` it
  writes is drawn by the app, which mounts it
  (`appletDates.ts::withDateChips`), as the photo card's Taken is.

`typedColumns.ts` draws such a cell through `chipCell` or `entityCell`
when given its `chips` option, and `GridCard` redraws the Author,
Contact, Subject and Source columns when `people` or `entities` says an
answer changed. A right-click acts on the chip it landed on, so each
chip in a cell of several has its own menu. The
run log draws its Group and Step columns as chips the same way
(`RunLogPanel.ce.vue`), with the chip's menu ahead of the cell's, and
`TableGrid` takes the option too, as the contacts app's table and the
Sources table do: a double-click on a chip there opens its card
(`openCard`) instead of the cell's in-place edit, and its right-click
is still the row's menu. In every grid a single click on a chip selects
its row, as a click anywhere else in the row does; the chip's href is
stopped on the grid's box, after the grid has selected the row, because
SlickGrid selects nothing for a click already `preventDefault`ed. A
group row of an identity column draws its
value as the chip of the group's first row, where the rows are loaded
(`typedColumns.ts`), and the run log's Group and Step group rows draw
theirs too ("Source: Work Slack (12)"). `tableGrid.css` keeps the grid theme's link
colour off a chip in a cell.

In the Sources table a group's and a step's Name cell is its chip, a
step under the row's short name (`sourcesCardModel.ts::decorate`), with
no status dot (the Status column says it) and the row's problem counts
beside it. A double-click on it opens what it names; a group is renamed
from the row's menu (Rename…), the one way a Name cell's editor opens
(`canEditCell`).

## In a card's own template

`EntityChip.ce.vue` draws a group or step chip where no grid or
document does:

- **Cards:** the Dashboard's Sources, Needs you, Library and Latest
  activity sections; the header of a source's card (the group,
  and each step under its short name); the person card's heading for
  each source's record; the time-series card's heading for each
  source; the log-line card's group and step.
- **The app's chrome:** each row of the toolbar's "what is syncing"
  menu, the lists in the toolbar's pills for an upgrade and for freeing
  disk space, and the
  re-render dialog's list.
  The index group, `unified_grid_and_search_index`, is called "Search
  index" there.

It looks the entity up in `entities`, redraws when the answer moves,
and takes its double-click and right-click from `chipActions`. A single
click does nothing and goes no further, so a chip inside a clickable
row (Latest activity) is not a click on the row. The place hands it
`openCard`; a source's card's own chips show their part of it (a step's
section of Sync) instead of opening the card again, and the chrome's go through
`surface.ts::showCard`, which shows the same card (`sameCard`) where
one is already open. A place may add entries ahead of the chip's own
(`extra`): the log-line card's "Keep only this group" and "Exclude
this group" narrow the log beside it.

A card that uses it lists it, `ChipMenu` and `chip.css` in its
`styleSources`. The chrome is outside every card, so a chrome component
puts their styles in the document (`documentStyles.ts::adoptStyles`)
and imports `chip.css`.

**The Library card** (`LibraryCard.ce.vue`) draws its own: its tree's
rows, inside `<vs-tree>`'s shadow root through the tree's `renderLabel`
and `extraCss`, a source as its group's chip and every other node as its
node's; and its pane's title as the shown node's. The tree's `menu`
event and the title's `contextmenu` show `ChipMenu`, and a double-click
on a chip opens its card. `ChipMenu` listens for Escape on the window
in the capture phase, since the tree takes the key for itself.

## In the search field

The search field (`ui/src/search/`) draws the value of a term whose key
names a source, a group or a step (`source:slack`, the log's
`step:slack/ingest`) as that entity's chip, and a person key's value
that is a handle (`from:email:riker@enterprise.org`) as the person's
chip, and one that names a contact (`from:contact:<id>`) as the
contact's, in the text where it was typed; its menu draws the values it
offers the same way. The chip is a CodeMirror widget whose DOM is
`entityCell`'s, `chipCell`'s or `contactCell`'s, resolved through
`entities`, `people` or `contactsById` and redrawn when one answers;
the query text under it is unchanged. The field's host includes `chip.css`.

Its clicks are a text field's: a click selects the chip whole, and a
double-click opens it as text to edit, the one place a double-click
does not open what the chip names. The right-click menu is the field's
(Edit as text, Exclude) followed by `entityMenu`'s or `chipMenu`'s
(`ui/src/search/chipMenu.ts`).

## A contact's page

A card may hold no handle: only a number with no country code, or no way
to reach the person at all. Its person is then named by the card's own
page, in the page's heading, the grid's Contact and Subject cells and
the search list's title. `contactPages.ts` resolves the page
(`POST /people`'s `pages`) and draws the chip grey, under the card's
name, or as your contact's where its number is the only one a source
knows with the same digits and is on that contact. In a document it
never shows the link's own text ([Trust](#trust)). Its double-click
opens the person's page, and its menu copies the name and adds them.

## A document

**A rendered document is a chip by its title**, named by its
`datalib:doc/<uuid>` (`ui/src/cards/documentChip.ts`). Unlike a person,
a group or a step it has **no resolver**: the producer sends the title,
and a title changes only when the document is rendered again, which
changes the index row the producer read it from. So a grid draws it
from the cell alone, and a new search shows the new title.

- **look**: the title, led by its source's mark; "From Work Slack" and
  the URI on hover.
- **double-click**: the document's card. In the search and problems
  grids it opens beside the grid at the row's own section, as clicking
  the row does.
- **right-click**: Open “<title>”; Copy ID (the uuid), the title, or
  Copy title and link, which puts a link on the clipboard —
  `<a href="datalib:doc/<uuid>?library=<id>">Captain's log</a>` for
  whatever pastes rich text, `Captain's log (datalib:doc/<uuid>?library=<id>)`
  as plain text. The link carries the library it is in, and the desktop
  app opens it from any other app
  ([`datalib/backend/uri/README.md`](../../datalib/backend/uri/README.md)). The rest of the title
  bar's menu (`documentMenu.ts`: open at the source, copy as Markdown) needs the
  document itself, which a chip does not hold.
- **drag**: it drags as a link, with no search
  (`chipDragQuery`).

## A node

**Every node of a source's tree is a chip by its name**, named by its
`datalib:node/<uuid>` (`ui/src/cards/nodeChip.ts`): a channel, a label,
a folder, a page, a thread, any `nodes` row
([`grid_rows.md`](grid_rows.md) § "where a row sits"). Like a
document's, it has **no resolver**: whoever draws it read the node's
name and kind from the index, and those change only when its source
renders again. A node that is also a document (a Notion page, a
contact group's card) carries the document's uuid beside it, as
`data-node-doc`.

- **look**: the name; its kind and its URI on hover.
- **double-click**: its node card (`nodeView`), the search held to what
  is directly in it, led by its document when it is one.
- **right-click**: Open “<name>”; Copy ID (the uuid), the name, or Copy
  name and link, a link as a document chip's is.

The Library card draws one for every place in its tree, and as its
pane's title (§"In a card's own template"). Nothing else does yet, and
no render writes a `datalib:node/` link.

## A date

**Every stamp in a document is a chip too.** A render writes one as
`<time datetime="…" title="…">short form</time>`
([the contract](../user/rendered_documents.md#a-time)), and `decorateDates` in `ui/src/cards/dates.ts`
draws each `<time>` whose `datetime` it can read as a chip. Its value is
the whole of what it names, so there is no URI and no resolver; the
`<time>` element is the chip link. `dates.ts` is the one formatter, and
the rules are pure functions over the parsed stamp and the **time view**.

**The clock in the status bar sets the time view** for every date in the
app, chips and every other stamp (`ui/src/config/timeView.ts`, kept per
browser). A click or a right-click on it picks the zone — this
computer's, **as written** (each stamp in the offset its source wrote it
in), UTC, the last few named zones, or any zone found by place or offset —
and how a date is written: `Wed, Apr 15, 2026` or `2026-04-15`, on a 24-
or 12-hour clock. The clock shows the time now in that zone, outlined
when it is not this computer's. Under "as written", a stamp with no
offset of its own reads as this computer does; a day is the same day in
every zone, and so is a stamp in `Z` (or the older `-00:00`), which a
render writes for a time whose source names no clock.
Picking any other zone reads every stamp in that zone, whatever its
offset (`parseStamp` in `dates.ts`, `parseInstant` in `timeView.ts`). Anything that formats a stamp goes through `timeView.ts`, and
anything that draws one into the page itself (a grid, a document, the
search field) draws it again when the view changes. So does a chat's
divider for each day (`decorateChatDays`), which falls where the day
changes in the view's zone.

- **text**: the instant in the view's zone, `Wed, Apr 15, 2026, 12:05`;
  a day, `Wed, Apr 15, 2026`, the same day wherever it is read. The
  render's own text stays for any other viewer.
- **hover**: the instant to the second in the view's zone, the same in
  UTC, the clock the source wrote it in when that is neither, and how
  long ago.
- **copy**: the text with its zone, `Wed, Apr 15, 2026, 12:05 GMT+2`, and
  a plain `<time datetime>` as HTML, so a paste names the same instant.
- **right-click**: copy it as shown or as written (`datetime`);
  everything on its day in the view's zone; everything before it or
  after it. The search reads a bare day as midnight on the machine that
  runs it, so the day is `after:<day> before:<next day>` only while the
  view is this computer's zone, and otherwise its two midnights with
  their offsets (`after:2026-04-16T00:00:00+02:00
  before:2026-04-17T00:00:00+02:00`).
- **double-click**: a search card for its day.

**T0 draws every date from one instant.** "Use as T0" on an instant's
menu sets the time view's `t0`, and from then on every date chip that
names an instant (`dateText`) shows its distance from it, to the
millisecond, earlier ones negative: `0.000`, `+1.250`, `-4:05.000`,
`+2:00:00.000`, `-3d 04:05:06.789`, `+343y 075d 00:23:24.454`
(`timeView.ts::sinceT0`, which drops the units that are zero in front;
a year is a calendar year). A column of them is right-justified
so the units line up. A day stays a day, since it names no instant; the
hover, the copy and a chip's name in a menu stay the date, since a
distance from T0 means nothing pasted elsewhere. The clock shows
`T0=2026-10-26 09:21:57.190` in the view's zone in place of the time now,
with a × beside it and "Clear T0" in its menu; a chip's menu has "Clear
T0" too. "Set T0…" in the clock's menu takes one by hand: a date and
time to the millisecond and the zone it is read in, starting from T0 or
from now (`timeView.ts::instantOfInput`). T0 is kept per browser with
the rest of the view. Every chart's time axis is drawn from T0 too
(`docs/dev/cards.md` §"Charts"), and its crosshair reads the distance.

**A chip short of room ends in "…"** on the side `ChipLook.elide`
names: the right, except a log's stamp and any date from T0, whose
end is what tells two of them apart.

**A span is two dates read as one.** A render writes an event's start
and end as two `<time>`s in a `<span class="time-range">`; where the end
falls on the start's day in the view's zone it shows its clock alone
(`Fri, Sep 18, 2026, 19:00 – 22:00`), and otherwise its own day
(`rangeEndText`); from T0 each end is its own distance. The calendar's "When", its occurrences and its
cancelled dates are written this way, so no date on its page is text.

**A date in a snippet reads the same.** A snippet or a row's preview
is plain text (`datalib_schema::plain_text`), which writes a `<time>`'s
instant as its stamp, and `dates.ts::datesInText` shows each stamp as a
chip's text would. A day keeps the render's words: it reads the same in
every zone.

**A date in the search field is one too.** A value of a key that takes
dates (`after:`, `before:`) that reads as a day or an instant is drawn
as the same chip in the text where it was typed (`queryText.ts::dateWords`).
Its clicks are a field chip's — one selects it, two open it as text — and
it has no menu there.

**Any chip in a document drags as its search.** `chipDragQuery` gives
what a chip stands for in a search bar — everything with a person or a
contact, a group's documents, a date's day — and the document puts it on
the drag as plain text, which the search field takes like anything
typed. A step names no search and drags as a link.

## A label

**Where a message is filed upstream is a chip too**: an email's labels
and mailboxes, which a render writes after the header's time as
`<span class="msg-label">Away team</span>`
(`datalib/backend/etl/chat-common/README.md` §"The message header").
`decorateLabels` in `ui/src/cards/labelChip.ts` marks each one a chip.
Like a date, its name is the whole of what it names, so there is no URI
and no resolver.

- **double-click**, and **drag** into a search bar: everything filed
  under it, `label:"Away team"`.
- **right-click**: copy its name; everything labelled with it.

## Clicks

| | a person | a group | a step |
|---|---|---|---|
| **click** | nothing (in a grid, it selects the row); the href is never followed | the same | the same |
| **double-click** | the person card (`personView`): your contact and each source's record of them, the source the chip was seen in first | the source's card (`sourceView`), on its documents | its group's card on Sync, scrolled to the step's section |
| **right-click** | `chipMenu`: open the person card; compose mail to an email address; copy the identifier, the name or both; everything with them (`with:`); add them to your contacts, which opens their card | `entityChipMenu`: open its dashboard; then its Sources row's entries — Browse, Sync now or Stop, Turn off or on, history, step log, reveal, copy path, Reset; then copy the id, the name or both | the same, for the step's row |
| **hover** | the tooltip: who, the identifier, what each source calls them, their other handles | the tooltip: name and id, its type, its status | the same |
| **copy** | `Will Riker <riker@enterprise.org>` as text, a working `mailto:` link as HTML | `Work Slack (datalib:group/slack)`, and the link | the same |

**A group or step chip's menu is its row of the Sources table.**
`/api/entities` answers each with its `row`, and the menu is built as
the right-click lands by the Sources table's own `rowMenu`
(`chipActions.ts::entityChipMenu`), less what needs the config's text
(Edit settings, Rename, Remove, Compare) and what the chip's own entries
already are. An entry runs through `rowActions` on rows read fresh at
that moment, and says what it did in a toast. Until the answer lands,
the chip offers its own entries alone.

**`ui/src/cards/chipActions.ts` is the one place a chip's clicks are
decided**: which chip a press landed on (`chipElAt`, `chipOf`), what its
double-click opens (`chipCardSource`), its menu (`chipMenuOf`, every
kind's entries in the one order `CHIP_ACTIONS` gives), and what each
entry does (`runChipAction`). A surface only draws the menu — the
floating `ChipMenu.ce.vue` over a document or a card, a grid's context
menu, the search field's tooltip — and hands it a `ChipHost` saying
what it can do there: open a card, search in place. The
search grid narrows its own query for "Everything with" and replaces it
for a group chip's own Browse; everywhere else both open a search
beside the card. The row's Browse opens its browse card, with the
source's columns, from every surface. A contact chip (`data-contact`)
opens that contact's page.

**A click on a chip is safe**: it changes nothing and opens nothing, so a
person can click anywhere in a document or a grid without looking first.
What a chip opens is a double-click away (`openChip`, the one function
every surface calls), and what changes anything is a menu entry or a
button. Opening a person's card this way also says so (`justOpened`),
and the card takes the keyboard, so that Enter on *Add to contacts* adds
them; a card opened by a link or a kept layout leaves the keyboard alone.

A click with a modifier on a person chip opens a web, mail or phone
link outside the app, as on any link; a `slack://` one goes nowhere, and
no click on a `datalib:` link ever leaves the app (`externalLinks.ts`).
The mail app opens only from the menu's Compose entry (`openExternal`).
A double-click opens a card by its card source (`cardSources.ts`, which
carries no components, so naming a card does not load it).

## Adding a kind of chip

A new kind — a channel, a document — is these steps, in this order:

1. **Its URI.** A standard scheme if one exists, else
   `datalib:<kind>/<id>`. Parse and write it on both sides
   (`datalib_columns::Entity` and `chipLinks.js`), tested over the same
   cases, and admit a new scheme in `sanitize.ts`.
2. **Its resolver.** An endpoint that answers a batch of URIs, absent
   for one that names nothing, and a `Resolver` instance over it. Say
   what makes its answers go stale: an edit in this page (`dropAnswers`),
   or something that moves on its own (follow the live connection and
   `revalidate`).
3. **Its rules.** Its look, tooltip, menu and card source as pure
   functions, unit-tested; draw through `drawChip`.
4. **Where it appears.** A producer writes a chip link with
   `entity_link`, or sends an identity cell with `entity`. Each surface
   that draws it decorates, subscribes and binds the menu ids.

A new kind of *handle* is a person and goes through
[`contacts.md`](contacts.md) §"Changing the rules" instead.

## Where it is tested

- The URIs: `uri_unittests` and `tests/uri.test.ts` over the same
  `uri-fixtures.json` (documents, photos and nodes),
  `handle_unittests` (every handle kind's round trip),
  `columns_unittests` (groups and steps), `tests/chip_links.test.ts`
  (the TypeScript mirror over the same cases, and the plugin's marking).
- The resolver: `resolver.test.ts` (one request per pass, kept answers,
  `dropAnswers` and stale answers, revalidating, failures).
- The rules: `contacts.test.ts`, `entities.test.ts`, `documentChip.test.ts`,
  `nodeChip.test.ts`, `dates.test.ts`, and
  `chipActions.test.ts` (one menu order for every kind, and where each
  entry lands).
- The endpoint: `http_tests::entities_answer_a_chip_for_a_group_and_a_step`.
- The writers: `etl_render_unittests` (`chip_link`), the storage
  report's test in `introspect.rs`, chat-common's header and reactor
  tests.
- End to end: `problems-grid.spec.ts` (a problem's Document chip),
  `chip-menu.spec.ts` (a person in a document),
  `grid-chips.spec.ts` (the Author and Source cells, the storage
  report), `contacts.spec.ts` (two handles linked to one contact,
  followed by an open grid), and `data-sources-sync.spec.ts` (a run
  log's chips, and a group chip following a failed sync without a
  reload), and `library.spec.ts` (the Library card's chips).

## Not built

A system event as a chip ("Worf joined the channel"), which needs each
provider to carry the handle on its system events; a search for
everything beneath a node at any depth (a node chip's "Everything in":
`at:` is what is directly in it); a place's chip in the search field,
where `at:` shows its id; a node
chip anywhere but the Library card; a document chip in a rendered page (a `datalib:doc/` link in
markdown is not marked as one yet, and would need a resolver for its
title) or in the search field; a search when a document chip is
dragged (`datalib_document_id:` could name it); and the title bar's
whole menu on a document chip.
