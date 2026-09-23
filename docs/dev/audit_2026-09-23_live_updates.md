# Audit: the UI replacing what is under the pointer

A record, not reference. It reads the UI at `a2f6b31c` for one complaint,
heard from a person using the app and from the e2e suite alike. While a
sync runs, the screen changes under the pointer:
- a click lands on a row that was just redrawn;
- a right-click menu closes by itself;
- a grid jumps to its last row;
- a panel remounts and forgets what was open.

How it was read: three passes over `datalib/ui/src`, one each for
- where live updates come from,
- how each SlickGrid applies new data,
- where Vue components remount.

Findings 1 and 2 were checked by hand against the code. The rest cite
file and line but were not all re-read, and a claim marked *(inferred)*
was not traced to the end. Nothing here was measured in a running app.

## Why this matters beyond annoyance

The e2e suite hits the same races, and each spec that did has grown
its own retry around them. `grid-helpers.ts` has four: `actOnRowByUuid`,
`selectRowByUuid`, `expandRow`, `pickRowMenu`. `run-log.spec.ts` has a
fifth (#726, #734). Each retry works around the UI instead of fixing
it. A person clicking at the same moment gets no retry.

## Where the updates come from

Almost everything arrives on one EventSource, `/api/sync/stream`
(`live.ts:116`). Its frames:
- `job`: a sync job moved.
- `root`: something under the data root changed. Its kinds include
  `manage.rows`, `dag`, `index_changed`, `config_changed` and `log`.
- `resync`: fired after a reconnect, and whenever the tab becomes
  visible again (`live.ts:164-176`). Every subscriber then refetches
  everything.

The only timer is TableGrid's one-second clock for "N minutes ago"
cells (`TableGrid.ce.vue:416-432`). No polling is left.

## Findings, by how often a person meets them during a sync

### 1. The Manage grid redraws every row several times a second (verified)

`TableGrid.syncRows` diffs new rows by key and calls `updateItem` only
for the ones that changed (`TableGrid.ce.vue:137-148`). SourcesCard
then calls `repaint()` → `refreshCells()` → `invalidateAllRows()` +
`render()` regardless (`SourcesCard.ce.vue:911`,
`TableGrid.ce.vue:163-176`).

That happens on every `manage.rows` frame ("a few times a second while
a run is going", `SourcesCard.ce.vue:1452-1456` → `commitRows`
`:947-953`), on every jobs commit (`:915-919`) and on `reparse`
(`:873`). The grid is not virtualized (`virtualizeRows=false`), so
every row element is destroyed each time, whether or not anything
changed.

The relative-time clock does the same whenever any row's
"N minutes ago" text would change (`TableGrid.ce.vue:416-427`).

**Fix:** `repaint()` exists because some cells draw from state outside
the row: the job queue, the clock. Put that state on the row, so the
existing diff sees it change. Alternatively, have `refreshCells` take
the keys whose outside state moved, and invalidate only those rows.

### 2. The search grid reloads and scrolls to the bottom on every index commit (verified)

Each `index_changed` frame clears the search cache and runs the query
again (`GridCard.ce.vue:736-741`). Under streaming that is many times
per sync. Each result then:
- is assigned whole to `vueGrid.dataset`, with no diff (`:712-714`),
  so every row is redrawn;
- has the default sort applied again (`:605-612`);
- has the adaptive column hiding re-run, which overrides a column the
  person hid or showed (`:626`, `:677-685`, through `setColumns`, which
  rebuilds header and rows);
- fetches index state, then calls `invalidate()` again (`:168-170`);
- scrolls to the last row (or the first, for scored results) unless
  the person sorted by hand or a row is pinned by `sel` (`:617-620`).

The grid's context menu uses the library default
`hideMenuOnScroll: true` (`:1207`), so that scroll also closes an open
menu.

**Fix:**
- A refresh of the query already shown diffs by `uuid`: `updateItem`
  for changed rows, add and remove for the rest.
- Default sort, auto-hide and the scroll happen when the query
  changes, not when the same query is refreshed.

### 3. The live run log redraws every row per line, and its menu closes

New lines go in through `addItems(..., resortGrid: true)`
(`RunLogPanel.ce.vue:251-257`). The row count changes, so SlickGrid
redraws every row (acknowledged at `:180-185`).

The panel holds the tail back while a mouse button is down on the grid
(`:186-204`, `:236`). That covers presses, column resizes and
scrollbar drags. It does not cover:
- a context menu still open after the button is released;
- hover;
- a drag-selection of text.

Following the tail calls `scrollRowIntoView`. The menu takes the
default `hideMenuOnScroll: true` (`:797-799`), so the next line closes
an open menu.

**Fix:** hold the tail while a menu is open too, and set
`hideMenuOnScroll: false` as TableGrid does (`TableGrid.ce.vue:300`).

### 4. Every context menu finds its row by index, not id

The menu element lives on `document.body`, so a redraw does not remove
it. But SlickGrid keeps the row *index* it was opened on, and our menus
read the row again from that index when an entry is clicked:
- `grid/menu.ts:58` (TableGrid);
- `GridCard.ce.vue:918, 933, 962-969`, including `getCellNode` for the
  feedback anchor;
- `RunLogPanel.ce.vue:695-698`.

If rows are inserted above while the menu is open, the action runs on
a different row. That is a correctness bug, not only an annoyance.

**Fix:** capture the row's id when the menu opens, and act on that id.

### 5. Banners above the Manage grid move every row

The `loadError`, `parseError`, `configError`, `droppedRows` and job
`banner` blocks sit above the grid (`SourcesCard.ce.vue:1511-1545`). A
job banner retires itself when the job stops (`:151-155`, `:924-929`),
and `loadConfig` clears `loadError` and may set it again on each
`config_changed` or resync (`:877`, `:902`). Each change moves the grid
by the banner's height under the pointer.

Toasts do the same in their own stack: they are bottom-anchored and
auto-dismiss after 4 s (`ToastStack.vue:61-64`, `toasts.ts:37-42`),
so the Copy and × buttons of the ones left move *(inferred)*.

**Fix:** reserve the banner's space, or overlay it instead of pushing
the grid down.

### 6. The pipeline DAG card rebuilds from scratch on every `dag` frame

`sourceDagView.ts:249-256` calls `paint()` on every frame, whether or
not anything changed. `paint()` calls `wrap.replaceChildren()` and
builds a new scroller and SVG (`:113`, `:140-148`). The scroll position
goes back to the top left, and hover is lost.

**Fix:** skip the repaint when the loaded graph has not changed, and
carry the scroll offsets across a repaint that is needed.

### 7. Remounts that are rarer but lose more

- **The whole card surface.** `App.vue:26-34, 85-92` shows the error,
  first-run or newer-root view *instead of* the router view whenever
  the fetched config is missing, fails to parse, or is not
  `app_ready`. `refresh()` runs on every `config_changed` and resync
  (`:57-62`), and has no guard against an out-of-order reply
  (`:36-44`). A config written non-atomically by an agent or an editor
  unmounts every column *(inferred)*.
- **The commit-history grid.** It sits in a `v-else-if` on
  `historyError` (`SourcesCard.ce.vue:1606-1623`), and
  `refreshHistory` runs on every `dag` frame. One failed fetch
  unmounts it, and the next success mounts a new one with every commit
  folded again.
- **A column after Back.** `millerStack.ts:70-82` keeps a column only
  when both its code and its state match the history entry. A query
  changed after navigating away no longer matches, so Back remounts the
  column fresh.
- **Tab refocus.** `resync` makes every subscriber refetch. Combined
  with findings 1 and 2, coming back to the tab redraws everything.

### 8. Smaller ones

- `DocCard` shows a fetch error *instead of* the document already on
  screen (`DocCard.ce.vue:493-495`).
- The run picker `<select>`s are repopulated on `runs` frames and
  resync (`RunLogPanel.ce.vue:853-859`, `:905-934`). Whether an open
  native dropdown closes is unverified.
- The hidden legacy `/sources` view keys rows by list index
  (`SourcesView.vue:510`, `:619`).

## Checked and fine

- No `:key` changes on a data update. Each one is a stable id, or a
  counter bumped by a person's action (`wizardKey`).
- Custom-element cards re-run only when their source changes, or when
  a component they use changes hash (`ShadowCard.vue:185-208`). The
  manifest is swapped only when it differs (`frontendRegistry.ts:43-48`).
- Inputs keep unsaved text: ConfigCard (`ConfigCard.ce.vue:48`),
  ConfigErrorView (`:28-36`) and the wizard (`SourceWizard.vue:125-164`).
- TableCard skips an answer identical to the last one
  (`TableCard.ce.vue:28-38`).

## The rules these point to

1. **A refresh of the same view updates what changed and nothing
   else.** Replacing the dataset, `invalidateAllRows` and
   `replaceChildren` are for a different view, not a newer copy of
   the same one.
2. **Sort, scroll and column visibility change only when the person
   acts.** A new query or a click counts; a background refresh does
   not.
3. **An interaction in progress holds updates back.** That covers a
   button held down, a menu open and a cell being edited. Menus act
   on the row's id, never on its index.

Findings 1 and 2 are small, contained changes, and account for most of
what a person feels during a sync.
