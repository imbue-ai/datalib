# Chips: a link the app resolves, drawn as what it names

A **chip** is how datalib shows something inline: a person, a source
group, a pipeline step. It has a name, a picture or a mark, and behind
it a stable identifier the app looks up to learn what that thing is
*now* — the contact a person linked, the name the config gives a
group today, whether a step is running. The same chip is drawn in a
rendered document, in a grid cell and in the run log, and it behaves
the same in each: the same tooltip, right-click menu and double-click,
and the same text when copied.

Start here to add a new kind of chip or a new place that draws them.
What makes a *person* who they are — handles, each source's record,
the contacts app — is [`contacts.md`](contacts.md); this page is the
machinery every kind shares.

## Words

| word | means |
|---|---|
| **entity** | what a chip names: a person (by a handle), a group, a step |
| **chip link** | how a chip is written down: a markdown link, `[text](href "title")` |
| **entity URI** | the href, which names the entity (see [The link](#the-link)) |
| **resolver** | the one place in the page that asks the server what an entity is now, for one kind of entity |
| **producer** | whoever writes a chip down: a render writing markdown, or a server sending a grid row |

## The link

**A chip is a markdown link whose href the app knows how to resolve.**
In a `.md` file it is an ordinary link, so any markdown viewer shows a
named link with a tooltip; datalib recognizes the href and draws a
chip. Nothing about how the chip looks is written into the file. The
link carries only what the producer knew when it wrote it:

```markdown
## [Will Riker](mailto:riker@enterprise.org "Will Riker <riker@enterprise.org>") <time …>…</time>
# [slack](datalib:group/slack "slack") — storage
| Store | [ingest](datalib:step/slack/ingest "slack/ingest") | …
```

- **text** — the name to show until the chip resolves: what the
  source showed for a person, the id for a group, the function for a
  step.
- **href** — the entity URI.
- **title** — the hover any other viewer shows. Datalib replaces it
  with a live tooltip.

The URIs, a standard scheme where one exists so that a pasted link
still works in another app, and `datalib:` otherwise:

| entity | URI | written by |
|---|---|---|
| a person by email | `mailto:riker@enterprise.org` | `Handle::to_uri` |
| a person by phone | `tel:+12025550101` | `Handle::to_uri` |
| a Slack user | `slack://user?team=T01&id=U02` (Slack's own deep link) | `Handle::to_uri` |
| a person by a handle kind with no scheme | `datalib:handle/<kind>/<value>`, e.g. a Signal account id | `Handle::to_uri` |
| a group | `datalib:group/<id>` | `datalib_columns::Entity::uri` |
| a step | `datalib:step/<group>/<function>` | `datalib_columns::Entity::uri` |

Two functions in `datalib_etl_render::message` write every chip link
the backend writes, so the escaping lives in one place:
`chip_link(shown, &handle)` for a person, whose title is
`Handle::describe` (`Will Riker <riker@enterprise.org>`), and
`entity_link(text, uri, title)` for anything else. The UI reads and
writes the same URIs in `ui/src/cards/chipLinks.js` (`handleFromUri`,
`uriFromHandle`, `entityFromUri`, `uriFromEntity`); both sides are
tested over the same cases, so they cannot drift apart without a test
failing.

Who writes chips today: chat-common for a message's author,
recipients and reactors (`chat-common/README.md` §"The message
header"); Slack for a `<@U…>` mention in a body; the storage report
(`datalib_step/src/introspect.rs`) for its group in the heading and
each store's step in a column.

## Drawing a chip

1. **Marking.** The viewer's markdown-it plugin (`chipLinks.js`, run by
   `renderDocument.ts` and by the render preview) gives an explicit
   link whose href names a handle `class="chip" data-handle="<handle>"`,
   and one naming a group or step `class="chip" data-entity="<uri>"`.
   A link `linkify` made from a bare address in running text is left
   alone, so a signature's address stays an address. The sanitizer
   (`sanitize.ts`) admits the `slack:` and `datalib:` schemes beside
   the standard ones.
2. **Asking.** The surface asks the resolver for that kind what each
   chip names ([Resolving](#resolving)).
3. **Drawing.** `drawChip(el, look)` in `contacts.ts` draws a chip from
   a `ChipLook`: its text, its aria label, its classes, its tooltip
   (`title`), and its lead — a photo if there is one, else a
   contact's initial in a disc, else a bundled mark for the icon token
   (`email`, `slack`), else a pipeline glyph (`step:ingest`). The lead
   is decoration: hidden from screen readers and not copied. A photo
   the browser cannot draw falls back to what the chip draws without
   one.

What goes into the look is each kind's own rule, pure and unit-tested:

- **A person** — `chipLook` and `chipTooltip` in `contacts.ts`: the
  contact's name if a person linked one, else the best-ranked source's
  name, else what the source showed; the kind's mark while it is not
  linked to a contact, and a quiet "+" when a contacts app is there to
  link it. [`contacts.md`](contacts.md)
  §"In the UI" has the ranking.
- **A group or a step** — `entityLook` and `entityTitle` in
  `entities.ts`: the name the config gives it now (a step is named
  under its group: "Work Slack · Ingest"), or the producer's until the
  answer lands; its type's mark or its phase's glyph; and a dot after
  the name for a status worth noticing — running, waiting, queued,
  failed, blocked or interrupted. The words are in the tooltip.

`chip.css` is the one look. The document frame, the search and
problems grids, the log card, the sync dashboard and the render
preview each include it in their own root, since a card's shadow root
and the document frame see no page styles.

## Trust

A chip may appear anywhere in a body, including in a link a stranger
wrote in an email. That is safe for one reason: **a chip shows what
its href resolves to, never the link text.** A body that writes
`[Picard](mailto:phisher@romulan.net)` gets a chip for the phisher,
under the name the sources know them by, with "Shown here as
“Picard”" in its tooltip. A link to `datalib:group/slack` shows that
group's real name. Forging a link can only point at a real entity
under its real name, which is what any link already does.

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
- **`forget(keys)`** drops answers after an edit made in this page, so
  every surface draws them again. An answer to a question asked before
  the edit is thrown away when it lands.
- **`revalidate()`** asks again about everything held, keeping each
  old answer drawn until its new one arrives, and tells subscribers
  only about answers that changed or vanished. That is how a chip
  follows something that moves on its own without flickering.

The two instances:

| | `people` (`contacts.ts`) | `entities` (`entities.ts`) |
|---|---|---|
| keyed by | handle (`email:…`) | entity URI (`datalib:group/…`) |
| asks | the unified index's `POST /people` and the contacts app's `POST /resolve` | datalib-http's `POST /api/entities` (`manage::post_entities`, from the rows the Manage table reads) |
| answer | each source's record of the person, and the contact a person made | label, icon token, detail, status |
| goes stale when | a person edits a contact (create, link, unlink, no longer works): in this page the call forgets the handles it changed; from anywhere else — another window, an agent — the contacts app's commit reaches every page as a `curated` frame, on which `people` and `contactsById` revalidate (`movesPeople`), as on a resync | a sync moves a status, or the config changes: `entities` follows the live connection and revalidates on every frame `movesEntities` names, and on a resync |

Why the viewer joins this itself rather than the producer sending
finished chips: the producer still resolves what only it can (the name
a source showed, the name the config gives a group), but a contact a
person linked and a step's status are live state that moves while the
row does not. Joining them where they are live is what lets one link
made in a document redraw every grid already open beside it.
[`cards.md`](cards.md) §"Typed tables" records this as the one join
the viewer does.

## In a grid

A grid draws a chip from an `identity` typed cell
(`{id, label, icon, detail, entity}`, [`cards.md`](cards.md)
§"Typed tables"):

- **A person**: the cell's `id` is the handle as a URI and its `label`
  the name the source showed. The search grid's Author column is one
  (`author_ref`, built by `columns.rs::author_identity` from
  `grid_rows.author_handle`).
- **A group or a step**: the cell's `entity` is the URI, *beside* a
  bare `id`. The id stays bare because other code keys on it: the
  document view stores a source's "always load images" setting under
  the Source cell's id. The search and problems grids' Source column
  is one (`Sources::identity` sets `entity` for every source the
  config declares).

`typedColumns.ts` draws such a cell through `chipCell` or `entityCell`
when given its `chips` option, and `GridCard` redraws the Author and
Source columns when `people` or `entities` says an answer changed. The
run log draws its Group and Step columns as chips the same way
(`RunLogPanel.ce.vue`).

The Manage table's Name cell is not a chip, on purpose: the row *is*
the group or step, its double-click renames it, and its own menu
already offers everything a chip's would.

## In the search field

The search field (`ui/src/search/`) draws the value of a term whose key
names a source, a group or a step (`source_id:slack`, the log's
`step:slack/ingest`) as that entity's chip, and a person key's value
that is a handle (`from:email:riker@enterprise.org`) as the person's
chip, and one that names a contact (`from:contact:<id>`) as the
contact's, in the text where it was typed; its menu draws the values it
offers the same way. The chip is a CodeMirror widget whose DOM is
`entityCell`'s, `chipCell`'s or `contactCell`'s, resolved through
`entities`, `people` or `contactsById` and redrawn when one answers;
the query text under it is unchanged. The field's host includes `chip.css`.

Its clicks are a text field's: a click selects the chip whole, a
double-click opens it as text to be edited, the one place a
double-click does not open what the chip names. The right-click menu
is the field's (Edit as text, Exclude) followed by `entityMenu`'s or
`chipMenu`'s, so the dashboard, the log or "Everything from" is still a
right-click away; linking a handle stays the popover's
(`ui/src/search/chipMenu.ts`).

## Clicks

| | a person | a group | a step |
|---|---|---|---|
| **click** | the link popover, where a contacts app is configured | nothing; the href is never followed | nothing; the href is never followed |
| **double-click** | the person card (`personView`): your contact and each source's record of them, the source the chip was seen in first | the group's sync dashboard | the step's log |
| **right-click** | `chipMenu`: open the person card; compose mail to an email address; copy the identifier, the name or both; everything from them; link to a contact or edit the link | `entityMenu`: copy the name, the id or both; open its dashboard; browse its documents | `entityMenu`: copy; show its log |
| **hover** | the tooltip: who, the identifier, what each source calls them, their other handles | the tooltip: name and id, its type, its status | the same |
| **copy** | `Will Riker <riker@enterprise.org>` as text, a working `mailto:` link as HTML | `Work Slack (datalib:group/slack)`, and the link | the same |

A menu entry is an *id* the surface binds, never a handler, so a
document and a grid cell draw the same entries and act on them their
own way. A click with a modifier on a person chip is the browser's, as
on any link; no click on a `datalib:` link ever leaves the app. The mail
app opens only from the menu's Compose entry, through `openExternal`. A
double-click opens a card by its card source (`cardSources.ts`, which
carries no components, so naming a card does not load it).

## Adding a kind of chip

A new kind — a channel, a document — is these steps, in this order:

1. **Its URI.** A standard scheme if one exists, else
   `datalib:<kind>/<id>`. Parse and write it on both sides
   (`datalib_columns::Entity` and `chipLinks.js`), tested over the same
   cases, and admit a new scheme in `sanitize.ts`.
2. **Its resolver.** An endpoint that answers a batch of URIs, absent
   for one that names nothing, and a `Resolver` instance over it. Say
   what makes its answers go stale: an edit in this page (`forget`),
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

- The URIs: `handle_unittests` (every handle kind's round trip),
  `columns_unittests` (groups and steps), `tests/chip_links.test.ts`
  (the TypeScript mirror over the same cases, and the plugin's marking).
- The resolver: `resolver.test.ts` (one request per pass, kept answers,
  forget and stale answers, revalidating, failures).
- The rules: `contacts.test.ts`, `entities.test.ts`.
- The endpoint: `http_tests::entities_answer_a_chip_for_a_group_and_a_step`.
- The writers: `etl_render_unittests` (`chip_link`), the storage
  report's test in `introspect.rs`, chat-common's header and reactor
  tests.
- End to end: `chip-menu.spec.ts` (a person in a document),
  `grid-chips.spec.ts` (the Author and Source cells, the storage
  report), `contacts.spec.ts` (two handles linked to one contact,
  followed by an open grid), and `data-sources-sync.spec.ts` (a run
  log's chips, and a group chip following a failed sync without a
  reload).

## Not built

A system event as a chip ("Worf joined the channel"), which needs each
provider to carry the handle on its system events; chips for a channel
or a document; and editing a contact on the person card
([`plans/contact_editing.md`](plans/contact_editing.md)).
