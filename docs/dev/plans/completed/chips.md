# Chips: a link the app can resolve, drawn as the thing it names

**Status: landed (2026-10-06 to 2026-10-08), kept as the record.** A
person, a group and a step are chips in documents, grids and the run
log, resolved live. The reference is [`../../chips.md`](../../chips.md);
for people, [`../../contacts.md`](../../contacts.md). The PRs: #1020
(this plan), #1022, #1026, #1028, #1029, #1034, #1040, #1041, #1043,
#1045, #1060, and the contacts work's #1023, #1024, #1027, #1030. Two
things the design below says that the tree does differently: a group
or step cell carries its URI in `Identity.entity` beside a bare `id`,
not in `id`; and the hover card is a tooltip (#1046). Not built: system
events as chips.

A **chip** is how datalib shows an entity inline: a person, a source
group, a step, later a channel or a document. It has a name, a mark
or a photo, and behind it a stable identifier the app can resolve to
the entity's current state. The same chip is drawn in a rendered
document and in a grid cell, offers the same hover card, the same
right-click menu and the same double-click, and copies the same way.

The one rule everything below follows:

> **A chip is a link whose href the app knows how to resolve.**

In a markdown file it is `[Name](href)`, which every markdown viewer
shows as a link with a name. In datalib the viewer recognizes the href,
asks the resolver for that kind of entity who or what it is now, and
draws a chip. Copied out, it is a link again. Nothing about the chip's
appearance is written into the file: the file carries the identifier
and the name the source showed, and the look is decided when it is
drawn.

## Words

| word | means |
|---|---|
| **entity** | something datalib can name and resolve: a person (by handle), a group, a step. More kinds later. |
| **entity URI** | the href that names one. A standard scheme where one exists, `datalib:` where none does — see [The href](#the-href). |
| **handle** | a person's identifier in one namespace, normalized (`datalib_handle`): `email:…`, `tel:…`, `slack:T/U`. One kind of entity. |
| **resolver** | the endpoint the viewer asks about one kind of entity, in a batch: the contacts applet and the index's `/people` for handles, `datalib-http` for groups and steps. |
| **producer** | whoever writes a chip down: a render crate writing markdown, or a server sending a grid row. |

## The href

Standard scheme where one exists, `datalib:` otherwise:

| entity | href | today's identifier |
|---|---|---|
| a person by email | `mailto:picard@enterprise.starfleet` | handle `email:picard@enterprise.starfleet` |
| a person by phone | `tel:+12025550101` | handle `tel:+12025550101` |
| a Slack user | `slack://user?team=T01&id=U02` (Slack's documented deep link) | handle `slack:T01/U02` |
| a person by a kind with no scheme of its own | `datalib:handle/<kind>/<value>`, e.g. `datalib:handle/signal_aci/<uuid>` | the handle `<kind>:<value>` |
| a group | `datalib:group/slack` | the group id, a directory under the root |
| a step | `datalib:step/slack/ingest` | the step id `<group>/<function>` |

`datalib_handle` gains `Handle::to_uri()` and `Handle::from_uri()`,
tested as a round trip over every kind, and `ui/src/cards/chipLinks.js`
mirrors both in TypeScript over the same cases. A new kind of handle
with a standard scheme adds a row to this table and a parser on each
side; one without is spelled `datalib:handle/<kind>/<value>`, which
`from_uri` already reads through `Handle::rebuild`. A new kind of entity
adds a row, a parser and a resolver.

Why not one `datalib:` scheme for everything (`datalib:email:…`): a
person's link would then be dead in every other app, and a rich paste
into Mail or a GitHub comment would carry a link nobody can click.
Why not the bare handle as the href (`email:…`): `tel:` is a real
scheme and `email:` is not, so the two kinds would behave differently
outside datalib, and each would need admitting through the sanitizer.
The table above costs a small parser per kind and is otherwise free.

## In a document

The author span and the recipients line that `chat-common` writes
today
([`chat-common/README.md`](../../../datalib/backend/etl/chat-common/README.md)
§"The message header") become links:

```markdown
## [Jean-Luc Picard](mailto:picard@enterprise.starfleet "Jean-Luc Picard <picard@enterprise.starfleet>") <time class="msg-ts" datetime="…" title="…">Tue Feb 11th, 2025 at 11:33</time>
<div class="msg-recipients"><span class="msg-recipients-role">To</span> [Will Riker](mailto:riker@enterprise.starfleet "Will Riker <riker@enterprise.starfleet>"); …</div>
```

- **The `##` stays.** qmd cuts chunks at an `h2`; the heading is there
  for it, not for the eye (the README says why).
- **The name is what the source showed**, escaped as every other field
  is. The chip shows the resolved name instead and keeps the source's
  in `data-shown-as` for the hover card's "shown here as".
- **The hover is baked into the link too.** The link's title is the
  static hover every markdown viewer can show: the name the source
  showed and the identifier, in the same form the copy uses
  (`Jean-Luc Picard <picard@enterprise.starfleet>`,
  `Name (+12025550101)`, `slack/ingest` for a step). In datalib the
  decorate pass removes the title and draws the live hover card in its
  place, since that card knows what the file cannot: the contact you
  linked, and what every source calls them.
- **A chip may appear anywhere a renderer has a handle**: a Slack
  mention mid-sentence, a reaction's author, "Worf joined the channel",
  a step named in a run's notes. The header is no longer special; it
  is the first place a renderer writes one. Each provider that has
  handles for mentions or reactions writes them as links in its own
  PR.
- **Plain text stays plain.** A `Plain` body goes through
  `escape_md_block`, so a typed `[x](mailto:y)` is literal. Only
  `Markdown` bodies — an assistant's reply, an email — can carry links,
  and a link there is a link the sender wrote.
- `LAYOUT_VERSION` bumps once; every chat provider re-renders.

**Drawing.** The viewer's own markdown-it (`ui/src/cards/renderDocument.ts`)
gets a plugin (`chipLinks.js`): a `link_open` token whose href parses
as a handle gets `class="chip" data-handle="<handle>"`; when groups and
steps arrive, a `data-entity="<href>"` beside it for the kinds that are
not people. A link `linkify` made from a
bare address is skipped — its token carries `markup: "linkify"` — so a
signature's address stays an address. `sanitize.ts` admits the `slack:`
and `datalib:` schemes; `mailto:` and `tel:` are in DOMPurify's default
list. Then `decorateChips` does what `decorateHandles` does today
(`ui/src/cards/contacts.ts`): collects the chips under the body,
groups them by kind, asks each kind's resolver once, and draws.

**What a chip draws.** The lead, then the name, then the kind's mark
when the lead is not a photo:

| state | lead | name | mark |
|---|---|---|---|
| resolved, with a photo | the photo | the entity's name | — |
| resolved, no photo | the initial in a disc | the entity's name | — |
| unresolved person | — | the best name a source gave, else the shown text | the handle kind's mark, and a quiet "+" when a contacts app is there to link it with |
| resolved but stale | as resolved, dimmed and italic | | |
| group, step | the source's or the phase's mark | the group's name, the step's label | — |

The rules stay pure (`chipLook`, unit-tested); only the decorate pass
touches a DOM. The look is the one in `documentBody.css` today, moved
to a shared `chip.css` so the grid draws the same thing.

**Icons come from the resolver.** `NormalizedContact` keeps its
`photo: Option<Photo>` (an inline image or a remote URL, as the source
had it) and gains `photo_url: Option<String>` beside it: a URL the app
serves, app-relative, which the chip and the hover card lead with. The
contacts applet's `/photo/<contact_id>` for your contact; the index's
asset route for a source's inline photo, written beside the page
when it renders. A remote
`Photo::Url` never becomes a `photo_url`: the app makes no request to a
remote host without the person's say (`sanitize.ts`), so a Slack avatar
stays out until something fetches it into the CAS. A group's or a step's mark is the
`icon` token its row already carries (`Identity.icon`, mapped by
`ui/src/config/icons.ts`). The markdown never names an icon: the look
of a person changes when you link them, and the file was written once.

A chip no resolver answers for is drawn from the markdown alone, in
the unresolved look. That needs no server, so the checked-in render
preview (`ui/tests/goldens/render_preview.html`) shows chips for the
first time — the unresolved ones.

### Trust

#958 trusted a `data-handle` only on the header line, because a body
is HTML a stranger wrote and could place one anywhere. This plan lets
a chip appear anywhere, and is safe for a different reason: **a chip
shows who the href resolves to, never the link text.** A body that
writes `[Picard](mailto:phisher@x)` gets a chip for the phisher under
the name the sources know them by, with "shown here as Picard" on
hover; an unknown address shows as itself. Forging can only point at a
real entity under its real name, which is what any link already does.
`trustedHandleSpans` and its forgery test go; a new test pins that a
chip's label is the resolved name whatever the link said.

People counts are unaffected: `chat-common/src/people.rs` counts
structured authors and recipients, not links, so a signature's address
changes nothing in `/people`.

## In a grid

The `identity` cell type (`{id, label, icon, detail}`,
[`cards.md`](../cards.md) §"typed cells") is a chip in all but name.
It becomes one, two ways. A person's identity (the Author column)
carries the handle as a URI in `id`. A group's or a step's carries it
in a field of its own, **`Identity.entity`** (`datalib:group/slack`),
beside a bare `id`: the document view keys a source's remote-media
setting on the Source cell's bare id, and stored settings would stop
matching if it moved. The identity formatter in
`ui/src/cards/typedColumns.ts` draws either through the same
`drawChip` as a document.

- The `unified_index` applet sets `entity` on the Source cell of any
  source the config declares (`columns.rs::Sources::identity`); the
  search and problems grids draw it as a group chip. `datalib-http`
  answers what each named group or step is now, in a batch:
  `POST /api/entities` with `{entities: [uri]}` gives each one's label
  (a step's under its group's, "Work Slack · Download"), mark,
  detail and status (`manage::post_entities`). `entities` in
  `ui/src/cards/entities.ts` is the resolver over it.
- A group chip's double-click opens the group's sync dashboard and a
  step chip's opens the step's log; the menu copies the name, the id or
  both, opens it, and for a group browses its documents. A status worth
  noticing (running, queued, failed, blocked, interrupted) is a dot
  after the name; the words are on hover.
- The Manage table's Name cell is not a chip: the row is the group or
  step, its double-click renames, and its own menu already does all a
  chip's would.
- The search grid's Author column changes from `text` to `identity`.
  `grid_rows` gains `author_handle`, following the checklist in
  [`grid_rows.md`](../grid_rows.md) §"Adding a column"; the applet
  sends `author_ref: {id: "mailto:…", label: <the author as shown>,
  icon: "email"}` beside `author`, as `source_ref` sits beside
  `source`, so a row without a handle still has its author to draw.
- **The producer sends the id and the label it knows; the viewer
  overlays the entity layer.** For the visible page of rows, the grid
  asks each kind's resolver once — the way `askAboutVisibleRows` in
  `GridCard.ce.vue` already asks per-document questions — and redraws
  the cells that resolved. Linking a handle in a popover redraws every
  open grid and document, since the viewer knows which cells carry it.
  This bends cards.md's "the producer resolves, the viewer presents":
  the producer still resolves what only it can (the source's name for
  the author, the group's configured name), and the layer that moves
  while the row does not — your contacts — is joined where it is live.
  The alternative, the `unified_index` applet reading the contacts
  store itself, would lag the document beside it until a re-query, and
  could never serve Manage, whose producer is `datalib-http`, which by
  design links neither `etl` nor `contacts`. cards.md says so once this
  lands.
- `ui/src/grid/copyRows.ts` copies an identity cell the way a document
  copies a chip (below).

## One resolver

Every surface that draws chips asks one resolver who they are
(`ui/src/cards/resolver.ts`; for people, `people` in
`ui/src/cards/contacts.ts`). A surface asks as it draws; the questions
drawn in one pass go out as one request; answers are kept for the life
of the page. An edit — a link, an unlink, a new contact, "no longer
works" — forgets the handles it touched, and the resolver tells every
subscribed surface, so a grid already open redraws a chip linked in a
document beside it. An answer to a question asked before the edit is
dropped when it lands. Groups and steps have a resolver of their own of
the same shape, `entities`, with `datalib-http` behind it. A group's or
a step's answer also moves on its own, as a sync runs, so `entities`
follows the live connection and revalidates what it holds on every
frame that can move a status or a name: it asks again with the old
answer still drawn, and tells a surface only of answers that changed.

## Copy

A copy keeps the name and the identifier, in both clipboard flavours,
so a paste loses neither (`copyWithHandles` in `contacts.ts` today,
generalized):

| flavour | a person | a group or step |
|---|---|---|
| `text/plain` | `Jean-Luc Picard <picard@enterprise.starfleet>`; `Name (+12025550101)`; `Name (slack:T01/U02)` | `Slack (datalib:group/slack)` |
| `text/html` | `<a href="mailto:…" title="Jean-Luc Picard <picard@…>" data-entity="mailto:…">Jean-Luc Picard</a>` | `<a href="datalib:group/slack" title="slack" …>Slack</a>` |

The lead and the mark are decoration (`user-select: none`) and are not
copied. A paste into Mail or Slack keeps a working link; a paste back
into datalib is a link the plugin chips again.

## Clicks

One pure function, `chipMenu(handle, shownAs, who, canLink)` in
`ui/src/cards/contacts.ts`, feeds every surface. Its entries carry an
*id* and a label, never a handler, the way an `actions` cell does
(`cards.md` § "typed cells"): the document view and a grid cell draw
the same entries and bind each id their own way.

- **Click**: the hover card, and for a person the link/create popover
  (`HandlePopover.ce.vue`) where a contacts app is configured, as
  today. The handler ignores `ev.detail > 1` so a double-click does not
  also open it.
- **Double-click opens the entity's own card**: a person's contact
  card once [`contact_editing.md`](contact_editing.md) lands, and until then
  a search filtered to the handle; a group's row on Sources; a step's
  log. In a grid cell it stops propagation, so it does not also open
  the row.
- **Right-click** opens the menu: copy name; copy identifier; copy as
  `Name <identifier>`; everything from this person (the search); and,
  with a contacts app, link to a contact or edit the link, which opens
  the popover where unlinking and "no longer works" already live. In
  the document the chat body answers the frame's `contextmenu` itself
  when a chip is under the pointer (`ChipMenu.ce.vue`, drawn over the
  frame like the hover card) and the document's own menu never sees
  the click. In a SlickGrid cell it goes through the slot-bank menu of
  `grid/menu.ts`, which already builds entries per click; the entries
  for a chip under the pointer come before the row's.

The document body is drawn in a frame whose policy runs no script
(`docFrame.ts`), as today; the viewer's own code reaches in to
decorate and to listen, and the hover card, popover and menu are drawn
out here over the frame, as `ChatBody.ce.vue` does now.

## Order of work

1. **The link and the plugin.** `Handle::to_uri`/`from_uri` and the TS
   mirror; `chat-common` writes the author and recipients as links;
   `LAYOUT_VERSION`; the markdown-it plugin; the sanitizer's two
   schemes; `chip.css`; `decorateChips` replacing `decorateHandles`;
   the trust test replaced; copy emitting `<a href>`; goldens and
   snapshots updated, the render preview now showing unresolved chips.
   The chat-common README's header section and the sanitizer's
   vocabulary test change in the same PR.
2. **Chips in bodies.** Reactions: `NormalizedReaction::reactor_handle`
   (filled for Slack, WhatsApp and Apple Messages; Beeper has none yet,
   Signal has no reactions), and chat-common writes the reactor as a
   chip link, in a message's reactions and in the orphan list alike, and
   gives a reaction's grid row the reactor's handle. Slack `<@U…>`
   mentions are the Slack render's own mrkdwn conversion
   (`slack_render/src/render/mrkdwn.rs`) through `chip_link`. System
   events as each provider gets a handle for them.
3. **Clicks in the viewer.** `chipMenu`; right-click in `DocCard`;
   double-click; the click handler's `detail` guard.
4. **The grid.** A group in the Source column (`Identity.entity`); `author_handle` and
   the identity Author column; the identity formatter through
   `chipLook`; visible-page resolution; menu and double-click in a
   cell; `copyRows`. cards.md's typed-cells section updated.
5. **Photos.** `photo_url` on `NormalizedContact`; the contacts applet's
   photo route; the index's asset route for source avatars. The UI half
   (the chip and the hover card lead with `photo_url` when it is there)
   is built; the store, the routes and the field are with the contacts
   work.

Steps 1 and 3 are where the pattern is set; 2, 4 and 5 are its
extension to the other places the same entities show.
