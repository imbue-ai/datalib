# The Library tree holds everything: Manage, folders, links, pins

*Proposal (2026-11-28). Nothing in it is built. What the screens do
now was seen by using the TNG demo (`//datalib:dev_tng`) that day; what
the code does was read from the tree the same day. Thad's answers the
same day are written into the sections they settle; §"Questions" lists
what is still open.*

## The idea

Unix lets one program list, open and link anything because everything
is a file with a path. Datalib can do the same with a **node**:

1. Everything a person deals with has a `datalib:` name, and so a link:
   a source, a step, a place (a channel, a label, a folder upstream), a
   document, a contact, a contact label, and the new things below.
2. Everything sits somewhere in one tree, the Library, and is browsed
   and opened from there.

This plan does four things with that:

1. Every source and step chip, wherever it is drawn, shows its sync
   status and its problem counts.
2. The Sources card's content moves into the tree, as a *Manage* area.
3. A person can make folders in the tree and put links in them, to
   any node.
4. A person can pin any node of the tree to the sidebar.

## What is there now

Seen in the demo:

- **The Library is not in the sidebar.** It is opened from "+ New
  card"; the sidebar's pinned tabs are Dashboard, Search, Sources and
  Contacts. On first open the tree pane stayed blank for about three
  seconds before the tree drew.
- **The tree's top level is *Contacts*, then 24 sources in config
  order.** The groups that are not sources are left out: the two `diff`
  groups, Grid and Search Index, Time Series Index. Under a source are
  its places (a Slack workspace, then its channels).
- **A tree node shows no status and no problem count.** The Sources
  card shows both: `google-takeout` carries a red 1 and an amber 6 beside
  its name, and so does its Import step.
- **The tree's right-click menu is shorter than the Sources row's.**
  Both have Open, Browse this data, Sync now, Turn off, Show commit
  history, Show step log, Copy path, Reset…. Only the Sources row has
  Edit settings…, Rename…, Compare two versions…, Show sync dashboard
  and Remove from config.
- **The Sources card is already a two-level tree**: a group's chevron
  shows its steps (Import, Render markdown, Semantic indexing), each
  with its own status, size and problem counts. Its other columns are
  Actions (browse, sync, chart, on/off), Status, Items, Modal spend and
  Size. Above it: Sync everything, Packages, Add source.
- **The Sources grip reorders `config.toml`**, and refuses an order
  against the data flow (`config_model.md` § "What the Manage screen
  and the wizard make of it"). So the Sources order is the pipeline's,
  not one a person can arrange freely.
- **Edit settings is a modal** over the Sources card.
- **A source's own card** (double-click its chip) has the chip with its
  counts and a status mark in its header, then Documents and Sync tabs.
- **A group's status can hide a step that never ran.** After "Sync now"
  on `slack`, every source row read "Succeeded seconds ago", because the
  index ran and then each source's `embed`. `google-takeout` read
  Succeeded while its Import read "Never run" and held the error. That
  is the documented rule (a step that never ran is passed over while a
  sibling has run), but a green check on a chip would say more than the
  row does.
- **The sidebar is already a tree of cards.** An open tab's menu has
  Move up, Move down, Put in a new container, Rename…, Pin and Close.

I did not see a running state: the TNG sync finished in under a second.

Read in the code:

- **No backend route builds the tree.** `LibraryCard.ce.vue` puts each
  level together from `GET /api/manage/rows`, the grid applet's
  `/sources_with_places` and `/places/{parent}`, and the contacts
  applet's `/labels` (`cards/library.ts`).
- **Nothing a person arranges in the tree is saved**, but which nodes
  are expanded, kept in the card's state in
  `system/ui-state/layout.json`.
- **A source or step chip already receives its whole Manage row**
  (`EntityView` from `POST /api/entities`), status and problems
  included, and draws only a dot for the loud statuses (running,
  waiting, queued, failed, blocked, interrupted; `cards/entities.ts`).
- **`POST /api/entities` rebuilds every Manage row for each batch**,
  and every chip surface revalidates on each `manage.rows` frame, about
  once a second during a sync. [`sources_from_memory.md`](sources_from_memory.md)
  is the plan that makes that rebuild cheap.
- **A `[[groups]]` entry takes `id`, `name`, `type` and `description`
  only**, and the loader refuses any other field.
- **A place's uuid is minted from upstream ids** by the render
  ([`entity_ids.md`](../entity_ids.md)), so a link to a channel should
  survive a re-render. A test has to show it (step 4).

## 1. Status on every source and step chip

A chip gets one mark after its name and, when there are any, its
problem counts:

| mark | when |
|---|---|
| spinner | a step of it is running; hover says "3 of 4 steps done" |
| red × | its last run failed, or it is blocked or interrupted |
| clock | waiting or queued |
| green ✓ | its last run succeeded |
| hollow circle | never run |
| grey dash | turned off |

Then the red and amber problem counts as the Sources card draws them
now, less the muted ones. The hover keeps what it says today and adds
when the status began.

What it takes:

- The chip draws from the `EntityView` it already has; only
  `entityLook` and `chip.css` change. `quiet` stays for places that
  draw the status beside the chip (the Sources card's Status column).
- "3 of 4" is new: the Manage row gets the count of the group's steps
  the current sync request has finished and the count it will run,
  from the same request rows `RequestStepPhase` reads.
- **The group's mark is decided by its worst step, not its last.** A
  group whose Import never ran or failed does not get a green check
  because its `embed` succeeded. The Sources card's Status column takes
  the same rule (decided 2026-11-28), so the chip and the row agree;
  `config_model.md` § "What the Manage screen and the wizard make of it"
  changes with it.
- **Every source and step chip shows it**, a chip in a document's text
  included (decided 2026-11-28, to be tried and looked at). Other chips
  (people, dates, places) are unchanged.
- Before this ships to every chip, `POST /api/entities` must stop
  rebuilding all the rows per batch
  ([`sources_from_memory.md`](sources_from_memory.md)), or each chip
  surface adds a full rebuild a second during a sync.

The Library tree's source nodes are already chips, so they get the
status with no change of their own.

## 2. A *Manage* area in the tree

The top level is Datalib's, as `/` is the system's on Linux: three
areas Datalib fills, and one the person fills (§3):

```
Library                      (Datalib's; fixed)
├── Sources
│   ├── google-takeout ✓ 1 6
│   │   ├── Import  1 6      (its steps first)
│   │   ├── Render markdown
│   │   ├── Semantic indexing
│   │   └── Gemini Activity  (then its places)
│   ├── slack
│   └── …
├── Applets                  (decided 2026-11-28)
│   ├── Contacts             (as the tree's Contacts is now)
│   │   └── senior-staff     (a label)
│   └── Time series
├── Home                     (the person's, as /home/<user> is)
│   └── Work                 (a folder the person made)
│       ├── → google-takeout (a link to the source)
│       └── → #bridge        (a link to a Slack channel)
└── Manage
    ├── Sources              a leaf: its pane is the Sources table
    ├── Comparisons          tng_contacts-diff, slack-diff, with their steps
    ├── Indexes              Grid and Search Index, Time Series Index
    ├── Applets
    ├── Packages
    └── config.toml
```

**Each applet a person opens is a node under *Applets***, with its
own children, as *Contacts* has its labels. A package's applets land
there too, so the top level does not grow with each package. *Manage ›
Applets* keeps what is about running them: their state, their
settings, their log. An applet that only serves other screens (the
grid and search index) has no node under *Applets*.

**A source's steps are its first children** (decided 2026-11-28),
drawn as step chips with their status, before its places. Steps are
nodes like any other: a step has a name (`datalib:step/<id>`), a
parent (its group), a status and a menu, so it sits in the tree where
it belongs rather than in a separate screen.

- **Picking *Manage › Sources* shows the Sources table in the pane.**
  The columns a tree row cannot hold (Items, Modal spend, Size, the
  Actions) and Sync everything, Add source, Packages stay there. A tree
  of one name column cannot replace a table for comparing sizes, so it
  does not try.
- **Picking a group shows its source card** (Documents | Sync), and
  picking a step its part of the Sync tab.
- **Every chip of a group has the same menu**, the Sources row's, in
  the tree too. Edit settings opens the form as it does now.
- **Manage keeps config order**, and its grip keeps reordering the
  config. The person's own order is §3's folders, not this.
- **The Sources pin becomes a pin of *Manage*** (§4). The Sources card
  stays until the tree does everything it does; it then becomes the
  pane of *Manage › Sources* and nothing else. That table is kept for
  now (decided 2026-11-28); the aim is to delete it once the tree shows
  what people read it for, so each column it has is a question for
  later: does the tree draw it, or does nobody need it?

**A source lives in one place, *Sources***, in config order, with its
steps and places under it; anywhere else it appears, it is a link.
*Manage › Sources* does not list the sources again: it is one node
whose pane is the table, for comparing them side by side. The groups
that are not sources (the comparisons, the indexes) are listed under
*Manage*.

## 3. Folders and links

**Every folder is one of two kinds, never both** (decided 2026-11-28):

1. **Made by Datalib.** *Sources*, a source, a place, *Applets*, an
   applet, a contact label, *Manage*. What is in it comes from the library: the
   config, the index, the contacts store. A person cannot drop anything
   into it or take anything out; it changes when the data does.
2. **Made by a person.** It holds only that person's links and their
   other folders. Nothing in it appears or goes on its own.

So a source is never moved: it stays in *Sources*, and putting it in
*Work* puts a link to it there. The same goes for a channel, a
document, a contact or a step.

A person's folder, like a source, has an id that is not its name
(decided 2026-11-28):

- its **id** is a uuid minted when it is made, never changed, and is
  what its `datalib:folder/<uuid>` link, a pin and every link to it
  name;
- its **name** is what the person typed: free text, editable at any
  time, and two folders may share one. Renaming a folder breaks no link
  and no pin.

**The top level is Datalib's, and *Home* is the person's.** The top
level always holds *Sources*, *Applets*, *Home* and *Manage*, in
that order, as a Linux system owns `/` and a user owns their home
directory. Everything a person makes is under *Home*, so nothing
they do can hide or remove a Datalib area, and nothing Datalib adds
lands among their folders. A person's folder one level down is one
click further away; pinning it (§4) puts it in the sidebar.

A person's folder lives in one place: dragging it into another moves
it. Removing a folder removes the links in it and the folders under
it, never what they point at (one Undo brings them back, below).

**A link** names any node by its `datalib:` name: a source, a step, a
place, a document, a contact, a contact label, a Datalib area, or a
person's folder. It draws as the target's chip with a small arrow.
Expanding it shows the target's children; double-click opens the
target. Deleting a link never touches the target. Its menu adds Reveal
original, which selects the target where it lives.

- **Making one**: drag any chip (from the tree, a search result, a
  document) onto a person's folder, or use Make link in… on its menu.
  A drop onto a folder Datalib makes is refused.
- **Moving one**: dragging a link from one of the person's folders to
  another moves the link.
- **A link whose target is gone stays**, greyed, with the name it last
  had and "gone" on hover, until the person deletes it. Nothing
  removes it for them. This happens whenever a source is removed from
  the config, so it has to read well.
- **Cycles are harmless**: the tree reads a level only when it is
  opened, and anything that counts or walks the tree does not follow
  links.

### Where the arrangement is kept

Recommended: **a store of its own under `datalib_curated/`**,
`datalib_curated/datalib_library/library.doltlite_db`, beside the
contacts and the problem mutes.

- It is a person's work, which no step can rebuild, and resets never
  touch `datalib_curated/`.
- A doltlite store keeps each change as a commit, so Undo after a wrong
  drag is a revert.
- Agents read and change it through the same routes the UI does.

One table is enough:

| column | |
|---|---|
| `uuid` | the entry's id; a folder is `datalib:folder/<uuid>` |
| `parent_uuid` | its folder; null directly under *Home* |
| `kind` | `folder` or `link` |
| `name` | a folder's name; null for a link |
| `target` | the `datalib:` name a link points at; null for a folder |
| `last_label` | the target's name when the link was made, for a gone one |
| `sort_key` | the person's order within the folder |

Its one writer would be the grid-and-search applet, which already
answers `/places`, so that one route can answer "the children of this
node" for every kind (step 3 below). The alternatives, and why not:

- **A `folder` field on `[[groups]]`**: the config is the pipeline's,
  it can only place sources (not links to channels or documents), and
  every group entry would grow a display field.
- **`system/ui-state/`**: opaque JSON the server never reads inside,
  no history, and a window's layout is a different thing from a
  library's arrangement.

## 4. Pin any node

The sidebar already pins cards. A pinned node is a pinned tab showing
the Library card **rooted at that node**:
`libraryTreeView({ root: "datalib:folder/<uuid>" })`. The node's menu
gets Pin to sidebar; the tab's existing pin icon unpins it.

- Pinning *Work* gives a sidebar entry that opens to the Work folder as
  a tree, with the pane beside it.
- The default pins become Dashboard, Search, Library (whole), *Manage*
  and *Applets › Contacts*. Contacts is already a Library node, so its
  pin is the same move, and it keeps Contacts one click away now that
  it is a level down.
- Nothing new is stored: the pin is a tab in `layout.json`, as now.

The sidebar's own containers (Put in a new container) arrange **views**,
per window; the Library's folders arrange **data**, once per library
and seen by every window and agent. Keeping those two apart is
deliberate; pinning is the bridge between them.

## Order of work

Each step is one PR and is useful alone.

1. **Status on chips** (§1), after `sources_from_memory.md` makes the
   entities answer cheap. Includes the step-count field and the
   worst-step rule for a group.
2. **The full menu on every group chip** (the tree's menu equals the
   Sources row's). Small, and removes the mismatch seen above.
3. **One route for a node's children**, for every kind (an applet, a
   label, a source and its steps, a place, Manage), replacing the four
   reads the card puts together. Folders need it: an arranged entry and
   a generated one sit in the same level.
4. **The Manage area** (§2), its pane the Sources table; the Sources
   pin moves to it.
5. **Folders and links** (§3): the store, its routes, *Home*, the
   menu items, drag and drop (check what `vs-tree`'s vendored drag support gives
   first), and a test that a link to a place survives a re-render.
6. **Pin a node** (§4): `libraryTreeView` takes a `root`.
7. **Later**: a folder as a search scope (`in:Work` searches every
   source a link in it names); folders Datalib makes from a saved
   search (§"Questions").

`docs/dev/plans/datalib_uris.md`, whose Claude Doc is the source of
truth, gets the new names (`folder/`, and a name for each top-level area and
for an applet's node) before step 5.

## Questions

Settled on 2026-11-28:

- the Status column takes the worst-step rule;
- status shows on every source and step chip, to be tried;
- a source's steps are its children;
- one route answers a node's children;
- a folder is made by Datalib or by a person, never both, and a
  person's folder holds only links and folders (this replaces "drag
  moves a source, ⌥-drag links it", answered earlier the same day);
- a person's folder has a uuid and an editable name;
- the top level is Datalib's (*Sources*, *Applets*, *Home*,
  *Manage*); the person's folders are under *Home*, named for a Linux
  user's home directory;
- *Manage › Sources* is one node showing the table, for now;
- the Library is a pinned tab;
- applets a person opens are under a top-level *Applets*, *Contacts*
  among them.

Not now (decided 2026-11-28): a home for agents beside the person's;
folders Datalib fills from a saved search, grouped by column values.
Nothing here should rule either out.

Still open: nothing. The plan is ready for step 1.
