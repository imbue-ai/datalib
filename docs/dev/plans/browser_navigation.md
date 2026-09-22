# Riding the browser's navigation

*Written 2026-09-22 as a proposal; its four changes are built (the
reference is `cards.md` § "The miller layout and the browser"). Kept
as the record of what was found and why the shape is what it is;
§ "What this does not do" is still open.*

## The question

The three layouts — miller columns, the 2D tree, the tiling manager —
all answer "where does a card a card opens go?", and none of them
answers "how do I get back to where I was, keep this place, or open it
somewhere else?". A browser answers those with history, bookmarks and
tabs, and the app runs inside one (or inside a webview that has the
same history). So: instead of a fourth layout that re-implements a
browser, can the surface we have sit on the browser's own scheme?

Mostly it already does, and the places it doesn't are small.

## What is true today

Read from the tree, not from the docs; each claim names its file.

**The miller stack is a URL.** Every column is one path segment,
`code:size:state` (`ui/src/router/columns.ts`), so a stack is
bookmarkable, shareable, and survives a reload (`url-sync.spec.ts`
proves the reload). The tree and tiling layouts are in memory only;
the layout toggle's own tooltips say so (`views/CardsView.vue`).

**The back button never moves within a stack.** `MillerView.syncUrl`
writes every change with `router.replace`, and nothing else in the
layout pushes. The only pushes in the app are the toolbar's *Data
sources* and *New card* when no card surface is showing
(`ui/src/surface.ts`). So a session of opening and closing columns is
one history entry, and Back leaves the app. The route watcher that
would handle Back (`watch(() => route.path, …)` in `MillerView.vue`)
exists and rebuilds the stack from the URL, remounting every card —
the comment beside it says why that flashes the grid.

**"New tab" works only for the one link that is an `<a>`.** The chrome's
↗ is a real anchor to a one-column stack (`CardControls.aloneHref`), so
cmd-click, middle-click, right-click → *Open in new tab* and drag to the
bookmarks bar all do the right thing, and in the desktop app the shell's
`on_new_window` opens a second window (`tauri/src/main.rs`). Everything
else a card opens goes through `host.openCards(source)`, which is a
function call, not a link: a grid row has no href to cmd-click, and the
document card's edge links carry `href="/#/chat/<uuid>"`
(`DocCard.ce.vue`), a shape no route serves since the column-stack URL
landed — `chatHrefFromClick` deliberately lets a modifier-click through
"so we don't swallow that affordance", and the tab it opens shows the
default grid. Renderer-emitted `/chat/<uuid>` links inside markdown
bodies have the same fate.

**The desktop app has no navigation chrome at all.** No back or forward
key, no address bar, no bookmarks; the shell wires none (grep
`tauri/src/main.rs`). Whatever the browser gives for free, the app
window gives nothing for, today.

**Two things stand in for history and don't compose with it.** The card
chrome's ← → walk the card's *own* source history (a gallery pick, an
agent hand-off, a source edit), kept in `CardControls` for as long as
the card lives. And the miller layout's "open to the right replaces
everything further right" is itself a kind of history — the trailing
columns are where you were. Neither is in the browser's history, so
Back cannot reach either.

## The shape of the answer

A miller stack **is** a page. The URL already says so; the browser just
isn't told when the page changes. Four changes, in the order they pay
off, and none of them is a new layout:

### 1. Push structure, replace state

Split `syncUrl` by what changed. Opening a column, closing one, and
replacing a card's source (`openColumnsAfter`, `closeColumn`,
`setColumnSource`, `commitSource`, `addCard`, `showCard`) are
navigations: `router.push`. A card's state (`setColumnState` — the
grid's query, its selection, its column layout) and a column width are
not: `router.replace`, as now. The rule a person would state: *Back
undoes the last thing that opened or closed something.*

The route watcher then has to **reconcile, not rebuild**: a pure
function from the current slots and the incoming specs to the next
slots, keeping a slot's id where its code matches at the same index,
so Back to a stack that differs only by its last column leaves the
first columns mounted. A slot whose code matches but whose state
differs (the grid's `sel` after Back from a row click) is the open
question: `ShadowCard` re-runs a card only when its source changes,
and a card reads `initialState` once at mount, so the first cut
re-runs it and the grid flashes once. A later cut can hand a card a
restored state without remounting, if the flash matters.

The card chrome's ← → become redundant once every `setSource` is a
history entry; delete them then rather than keep two histories.

### 2. Every in-app navigation is an `<a href>`

Add to `HostCommands` a pure sibling of `openCards`:

```ts
hrefFor(...sources: string[]): string   // the URL openCards would land on
```

so a card can render a real anchor — the grid row's document, the
document card's edges, a Manage row's *Browse* — and intercept only a
plain click (`preventDefault`, then `openCards`) while a modified click,
a middle click, the context menu and a drag stay the browser's. This is
what puts *Open in new tab*, *Copy link*, *Bookmark* and hover-to-see-
where-it-goes on every link for free; the ↗ becomes one anchor among
many. The layout computes the href, since only it knows what "open
from this card" means for the stack.

Fix the two hrefs that exist and lie: `DocCard`'s `/#/chat/<uuid>`
becomes `hrefFor(documentView(...))`, and a `/chat/:uuid` route redirects
to `documentView("<uuid>")` alone, because that shape is written into
rendered markdown in every render store and a redirect costs one line
(the alias rule in `AGENTS.md` § "Breaking changes are fine").

### 3. Tell the browser the page's name

`document.title` is never set (grep `ui/src`). Set it from the stack's
titles — the last column's, or "Search: kraken › Warp core thread" —
so the history menu, a bookmark and a tab strip say what a stack is.
`ctx.setTitle` already gives the layout every title.

### 4. Give the app window the two keys it lacks

`history.back()` and `history.forward()` are what the browser's buttons
call, and the webview keeps the same history; the desktop app only
lacks something to call them. Cmd+[ / Cmd+] (and Alt+←/→ on Linux and
Windows) in the shell, or a ← → pair in the app header shown when
`isDesktopApp()` — two buttons that call the browser are not a browser.
Bookmarks in the app are the URL: *Copy link* (from 2) into wherever
the person keeps them.

## What this does not do

**"Take the whole page somewhere"** needs no mode. A link whose target
is `[documentView(x)]` alone rather than `[…current, documentView(x)]`
is just a different href; once links are real (2), a card can offer
both and the person can put either in a tab. The miller layout's
truncate-to-the-right is already most of the way to a page model:
opening from column *n* discards everything past *n*, which is what a
browser does to forward history when you navigate from the middle.

**The tree and tiling layouts stay off the browser** unless their shape
goes into the URL — the tree is a forest with parent pointers and
encodes as easily as the stack; the tiling split tree less so. Whether
either earns that is a separate call; this plan makes the miller layout
the one that rides the browser, and leaves the toggle where it is.

## What landed, and what the build taught

All four, in one change. Two things the proposal had wrong: the entry
being left needs no separate "replace with its final state" write,
because a card's `setState` already writes as it happens — but those
writes **must be queued**, since the router cancels a navigation another
one overtakes, and a row click is a replace and a push in one tick.
The e2e spec (`browser-history.spec.ts`) fails without the queue and
was run that way once to prove it. And the reconcile keeps a column
only when code *and* state match; a state-only difference remounts,
which the grid's selection never triggers because the selection is
written before the document is pushed.

Still open from § "What this does not do": links from the grid's rows
(a SlickGrid cell is not an anchor), the tree and tiling layouts, and
whether the card chrome's ↗ should become the same `hrefFor` link.
