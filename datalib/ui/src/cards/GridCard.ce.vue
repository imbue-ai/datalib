<script setup lang="ts">
// Search-grid card: a search bar + a slickgrid over the unified_index
// applet's /search results.
//
// Selecting a row opens the row's document as a new card via
// ctx.host.openCards — structural changes never go through the bus.
// Double-clicking a row opens that document as a standalone
// single-column page in a new tab.
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, watch } from "vue";
// The grid is the framework-agnostic bundle, not the Vue wrapper: a
// card is a custom element, and the wrapper finds its container by a
// selector on `document`, which cannot see into a shadow root. The
// bundle takes the element itself.
import { SlickVanillaGridBundle } from "@slickgrid-universal/vanilla-bundle";
import type {
  Column,
  CurrentColumn,
  CurrentSorter,
  Formatter,
  GridOption,
  GridStateChange,
  MenuCommandItem,
  MenuFromCellCallbackArgs,
  OnClickEventArgs,
  OnDblClickEventArgs,
  OnSelectedRowsChangedEventArgs,
  SlickDraggableGrouping,
  SlickEventData,
} from "@slickgrid-universal/common";
import { FILTER_GRID_OPTIONS, typedColumns, groupTitle } from "./typedColumns";
import { type AccountsMap, type ColumnSpec, type QmdDocState, type SearchRow } from "@/api";
import { useApi } from "@/cards/cardApi";
import { slugify } from "@/config/sourceSteps";
import { copyToClipboard } from "@/clipboard";
import FeedbackModal from "@/components/FeedbackModal.vue";
import { buildContext, type FeedbackContext } from "@/feedback/context";
import { filePathFromUrl, isDesktopApp, revealActionLabel, revealInFileManager } from "@/desktop";
import { openExternal } from "@/externalLinks";
import { subscribeLive } from "@/live";
import { encodeColumns } from "@/router/columns";
import { KEEP_COLUMN_WIDTHS } from "@/grid/columnLayout";
import { keepExcludeEntries, withToken, type FilterEntry } from "@/grid/query";
import { perOpening } from "@/grid/menu";
import { newlyPicked } from "@/grid/selection";
import { markdownsToAsk, widen } from "@/grid/qmdAsk";
import { keepActiveOnRecord } from "@/grid/activeCell";
import { redrawChanged } from "@/grid/redrawChanged";
import { handedOf, isEmpty, patchRows, type Handed, type RowPatch } from "@/grid/rowPatch";
import { searchFailure, type SearchFailure } from "./searchFailure";
import type { CardCtx } from "./types";

const { fetchAccounts, fetchQmdState, fetchSearch } = useApi();

const props = defineProps<{
  ctx: CardCtx;
  // Initial query from the card source (`gridView({q: "…"})`); the
  // persisted state's `q` wins over it when present.
  q?: string;
  // Which columns this card opens with, from the card source
  // (`gridView({columns: ["kind", …]})`) — a Browse card names the set
  // that suits its source's type (see config/browsePresets.ts).
  // Undefined keeps the grid's own defaults. A persisted column state
  // wins over it, same as `q`: once the user has moved a column, this
  // card is theirs.
  columns?: string[];
  // The card's name, from the card source (`gridView({name: "Slack
  // documents"})`). Without one the card is named for its live query.
  name?: string;
}>();

const initialState = new URLSearchParams(props.ctx.initialState);

const query = ref(initialState.get("q") ?? props.q ?? "");

// An unnamed card's name tracks the live query, not just the factory
// argument — searching from inside the card renames it.
watch(query, (q) => props.ctx.setTitle(props.name ?? (q ? `Search: ${q}` : "Search")), {
  immediate: true,
});
const rows = shallowRef<SearchRow[]>([]);
/// The columns the applet declares for its rows — see `ColumnSpec`.
const columns = ref<ColumnSpec[]>([]);
// The query whose results are actually painted right now — not `query`
// (what is typed) and not `!loading` (which flips in both directions
// within one tick, so an observer can miss the transition entirely).
const shownQuery = ref<string | null>(null);
const total = ref(0);
const loading = ref(false);
const error = ref<SearchFailure | null>(null);
// A failed search leaves the previous query's rows painted; say so, or
// the count above them reads as the answer to what is typed.
const showingStale = computed(
  () => error.value !== null && rows.value.length > 0 && shownQuery.value !== query.value,
);
// A free-text search failed in qmd, and came back with no rows.
const qmdError = ref<string | null>(null);
const accounts = ref<AccountsMap>({});

// --- qmd index state (the Indexed / Embedded columns) ---------------
// Answers for the documents behind the rows on screen, gathered as the
// grid scrolls, and started over when the result set changes.
const qmdState = ref<Map<string, QmdDocState>>(new Map());
// Collection-wide totals, shown next to the row count.
const qmdSummary = ref<{ documents: number; embedded: number } | null>(null);
// Bumped when the result set changes. An answer for an older one is
// dropped rather than merged into state describing rows now gone.
let qmdGeneration = 0;
// What is out for this generation, so a scroll does not ask twice.
const qmdAsked = new Set<string>();
const qmdInflight = new Set<AbortController>();

// True when either index-state column is on screen. Both are hidden by
// default, and asking about a document costs a file read + a SHA-256
// server-side, so this is what keeps the feature free for everyone who
// isn't using it.
function qmdColumnsVisible(): boolean {
  if (!vueGrid) return false;
  return vueGrid.slickGrid
    .getColumns()
    .some((c) => (c.id === "qmd_indexed" || c.id === "qmd_embedded") && !c.hidden);
}

// The result set changed: forget every answer and ask afresh. With both
// columns hidden this still asks, with no documents, for the "N of M
// documents searchable" line under the grid, which is the only thing on
// screen that hints the columns exist.
function refreshQmdState() {
  qmdGeneration++;
  qmdAsked.clear();
  for (const ctrl of qmdInflight) ctrl.abort();
  qmdInflight.clear();
  qmdState.value = new Map();
  refreshIndexCells();
  if (qmdColumnsVisible()) askAboutVisibleRows();
  else void askQmdState([]);
}

function askAboutVisibleRows() {
  const grid = vueGrid?.slickGrid;
  if (!grid || !qmdColumnsVisible()) return;
  const { top, bottom } = widen(grid.getRenderedRange(), grid.getDataLength());
  const rowsInView = [];
  for (let row = top; row <= bottom; row++) rowsInView.push(rowData(row));
  const uuids = markdownsToAsk(rowsInView, qmdState.value, qmdAsked);
  if (uuids.length > 0) void askQmdState(uuids);
}

async function askQmdState(uuids: string[]) {
  const generation = qmdGeneration;
  for (const u of uuids) qmdAsked.add(u);
  const ctrl = new AbortController();
  qmdInflight.add(ctrl);
  try {
    const r = await fetchQmdState(uuids, ctrl.signal);
    if (generation !== qmdGeneration) return;
    const merged = new Map(qmdState.value);
    for (const [uuid, st] of Object.entries(r.docs)) merged.set(uuid, st);
    qmdState.value = merged;
    qmdSummary.value = r.summary;
    refreshIndexCells();
  } catch (e) {
    if ((e as { name?: string }).name === "AbortError") return;
    // Non-fatal: these cells stay "unknown", and the next scroll over
    // them asks again. fetchQmdState has already raised a toast for
    // anything the user should see.
    if (generation === qmdGeneration) for (const u of uuids) qmdAsked.delete(u);
  } finally {
    qmdInflight.delete(ctrl);
  }
}

// The grid calls this as it scrolls, many times a second; ask once it
// settles.
let qmdScrollTimer: ReturnType<typeof setTimeout> | null = null;
function onViewportChanged() {
  if (qmdScrollTimer) clearTimeout(qmdScrollTimer);
  qmdScrollTimer = setTimeout(() => {
    qmdScrollTimer = null;
    askAboutVisibleRows();
  }, 150);
}

const qmdSummaryTitle = computed(() => {
  const s = qmdSummary.value;
  if (!s) return "";
  const pending = s.documents - s.embedded;
  return pending > 0
    ? `${pending.toLocaleString()} indexed document(s) are still waiting on embeddings; ` +
        `semantic search cannot reach them yet.`
    : "Every indexed document has embeddings.";
});

// Repaint the two index columns, which read `qmdState`, a map the grid
// has no way to observe on its own. Only those cells: a row rebuilt
// under the pointer loses the click aimed at it.
function refreshIndexCells() {
  const grid = vueGrid?.slickGrid;
  if (!grid) return;
  const cells = ["qmd_indexed", "qmd_embedded"]
    .map((id) => grid.getColumnIndex(id))
    .filter((i): i is number => i != null && i >= 0);
  const { top, bottom } = grid.getRenderedRange();
  for (let row = top; row <= bottom; row++) for (const c of cells) grid.updateCell(row, c);
}

// Tri-state cell: true → ✅, false → ❌, null/unknown → an em dash. The
// third state is not decoration — "no rendered document" and "the index
// is unreadable" are different from "not indexed", and showing a red ❌
// for them would assert something we did not check.
function indexFlag(v: boolean | null | undefined): string {
  if (v === true) return "yes";
  if (v === false) return "no";
  return "unknown";
}

// The index state for a row's document, or undefined before the first
// /qmd_state response lands.
function qmdDocState(row: SearchRow | null | undefined): QmdDocState | undefined {
  if (!row?.markdown_uuid) return undefined;
  return qmdState.value.get(row.markdown_uuid);
}

function qmdFlagTooltip(row: SearchRow | null | undefined, which: "indexed" | "embedded"): string {
  if (!row) return "";
  if (!row.markdown_uuid) return "This row has no rendered document.";
  const st = qmdState.value.get(row.markdown_uuid);
  if (!st) return "Checking the qmd index…";
  if (st.note) return st.note;
  const v = which === "indexed" ? st.indexed : st.embedded;
  if (v === null) return "Unknown.";
  if (which === "indexed") {
    return v
      ? "This document's current content is in the qmd keyword index."
      : "Not in the qmd index — either never indexed, or re-rendered since the last indexing run.";
  }
  return v
    ? "Embedded: semantic search can reach this document."
    : "No complete set of embedding vectors yet — semantic search will not find this document.";
}

function flagFormatter(which: "indexed" | "embedded"): Formatter<SearchRow> {
  return (_r, _c, _v, _col, row) => {
    const flag = indexFlag(qmdDocState(row)?.[which]);
    const span = document.createElement("span");
    span.className = "qmd-flag";
    span.dataset.flag = flag;
    span.textContent = flag === "yes" ? "✅" : flag === "no" ? "❌" : "—";
    return { html: span, toolTip: qmdFlagTooltip(row, which) };
  };
}
const selectedRow = ref<SearchRow | null>(null);
// Selected row uuid as persisted state — survives reloads so the
// deep-linked column highlights the same row.
const sel = ref<string | null>(initialState.get("sel"));

// The grid, once created: the SlickGrid and DataView objects and the
// services around them. `dataView` and `slickGrid` are optional on the
// bundle's type only because it can be asked for them before `init`;
// here it is never handed out before both exist.
type Grid = SlickVanillaGridBundle<SearchRow> & {
  dataView: NonNullable<SlickVanillaGridBundle<SearchRow>["dataView"]>;
  slickGrid: NonNullable<SlickVanillaGridBundle<SearchRow>["slickGrid"]>;
};
let vueGrid: Grid | null = null;
let groupingPlugin: SlickDraggableGrouping | null = null;
/// The element the grid is built in — the resizer measures it, and
/// the grid's own stylesheet goes into the root it sits in.
const boxEl = ref<HTMLDivElement | null>(null);

// Suppress state writes (and column-open side effects) while we're
// applying state from the URL ourselves — otherwise the grid's
// column/selection events would clobber the state we just read, and
// restoring a selection would open a duplicate document column.
let restoring = false;

// True once the user has manually clicked a column header (or the URL
// restored an explicit column state). Once set, we stop forcing the
// score-vs-time default on subsequent query result loads.
let userSortedManually = false;

/// What the URL keeps of the grid's shape: every column in order with
/// whether it is hidden and how wide it is, the sort, the grouping.
type Layout = {
  cols: { id: string; hidden?: boolean; width?: number }[];
  sort?: { id: string; asc: boolean }[];
  group?: string[];
};

function encodeLayout(layout: Layout): string {
  // Compact base64url so the URL stays vaguely readable when it shows up
  // in dev tools / shared links.
  const json = JSON.stringify(layout);
  return btoa(unescape(encodeURIComponent(json)))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

function decodeLayout(s: string): Layout | null {
  try {
    const padded = s.replace(/-/g, "+").replace(/_/g, "/");
    const json = decodeURIComponent(escape(atob(padded)));
    const parsed = JSON.parse(json) as Layout;
    return Array.isArray(parsed?.cols) ? parsed : null;
  } catch {
    return null;
  }
}

// Latest encoded layout; null while the columns are still at their
// defaults (so a pristine grid serializes to a short segment).
let colsEncoded: string | null = initialState.get("cols");

function saveState() {
  if (restoring) return;
  const params = new URLSearchParams();
  if (query.value) params.set("q", query.value);
  if (sel.value) params.set("sel", sel.value);
  if (colsEncoded) params.set("cols", colsEncoded);
  props.ctx.host.setState(params.toString());
}

/// The grid's shape as it is now.
function readLayout(): Layout {
  const grid = vueGrid!.slickGrid;
  const cols = grid.getColumns().map((c) => ({
    id: String(c.id),
    ...(c.hidden ? { hidden: true } : {}),
    ...(c.width ? { width: c.width } : {}),
  }));
  const sort = grid
    .getSortColumns()
    .map((s) => ({ id: String(s.columnId), asc: s.sortAsc !== false }));
  const group = groupingPlugin?.columnsGroupBy.map((c) => String(c.id)) ?? [];
  return { cols, ...(sort.length ? { sort } : {}), ...(group.length ? { group } : {}) };
}

// Reflect any user-driven column change (resize / sort / move /
// visibility / grouping) into the persisted state. Skipped during
// programmatic mutation (`restoring`).
function updateCols() {
  if (restoring || !vueGrid) return;
  colsEncoded = encodeLayout(readLayout());
  saveState();
}

/// Give the grid a column order, visibility and widths. Every column
/// the grid has is named, in the order wanted; one left out keeps its
/// place at the end.
function applyColumns(cols: CurrentColumn[]) {
  if (!vueGrid) return;
  const known = new Set(cols.map((c) => c.columnId));
  const rest = vueGrid.slickGrid
    .getColumns()
    .filter((c) => !known.has(String(c.id)))
    .map((c) => ({ columnId: String(c.id), hidden: !!c.hidden, width: c.width }));
  restoring = true;
  vueGrid.gridStateService.applyColumnLayout([...cols, ...rest], false);
  restoring = false;
}

/// Hide or show columns by id, keeping their order and widths.
function setHidden(hidden: Record<string, boolean>) {
  if (!vueGrid) return;
  applyColumns(
    vueGrid.slickGrid.getColumns().map((c) => ({
      columnId: String(c.id),
      hidden: hidden[String(c.id)] ?? !!c.hidden,
      width: c.width,
    })),
  );
}

function applySort(sorters: CurrentSorter[]) {
  if (!vueGrid) return;
  restoring = true;
  if (sorters.length === 0) vueGrid.sortService.clearSorting(false);
  else {
    vueGrid.sortService.updateSorting(sorters, false, false);
    // `updateSorting` sorts a tick later (its local path awaits an event
    // first). Sort now as well, with its comparer, so a row looked up
    // next — a restored selection scrolling to itself — is already where
    // it will stay.
    const sortService = vueGrid.sortService;
    const columns = vueGrid.slickGrid.getColumns();
    const sortCols = sorters.flatMap((s) => {
      const col = columns.find((c) => c.id === s.columnId);
      return col ? [{ columnId: col.id, sortAsc: s.direction === "ASC", sortCol: col }] : [];
    });
    vueGrid.dataView.sort((a, b) => sortService.sortComparers(sortCols, a, b));
  }
  restoring = false;
}

function rowKey(row: SearchRow): string {
  return row.uuid;
}

/// The record at a grid row, or null where the row is a group header
/// or its totals — the data view hands those out as items too.
function rowData(row: number): SearchRow | null {
  const item = vueGrid?.dataView.getItem(row) as
    (SearchRow & { __group?: boolean; __groupTotals?: boolean }) | undefined;
  if (!item || item.__group || item.__groupTotals) return null;
  return item;
}

/// The rows the grid has selected, in grid order.
function selectedRows(): SearchRow[] {
  if (!vueGrid) return [];
  return vueGrid.slickGrid
    .getSelectedRows()
    .map(rowData)
    .filter((r): r is SearchRow => r != null);
}

// Lightroom-style: if multiple rows are selected and the right-click anchor
// is part of that selection, the action targets all selected rows;
// otherwise it targets only the anchor row. The selection itself is left
// alone either way — a right-click aims the action, it does not re-select.
function resolveTargetRows(anchor: SearchRow | null | undefined): SearchRow[] {
  if (!anchor) return [];
  const selected = selectedRows();
  if (selected.length > 1 && selected.some((r) => r.uuid === anchor.uuid)) return selected;
  return [anchor];
}

// Feedback modal state
const feedbackOpen = ref(false);
const feedbackContext = ref<FeedbackContext | null>(null);
const feedbackSurfaceLabel = ref("");

// Filter context for a right-clicked cell. Null for non-filterable
// columns (Time, Contents) or rows with no value in the clicked column.
type FilterCtx = {
  // Query-language key (e.g. "source", "channel"); maps to a backend Field.
  key: string;
  // Human-facing column header for menu labels.
  header: string;
  // Raw value to filter by: whatever the column stores, which for a
  // uuidCol column is the sibling UUID rather than the cell's text.
  value: string;
};

// Map column id → query-language key + header. Keep in sync with
// `column_for_field` in backend/unified_index/src/db.rs.
//
// `uuidCol` (when set) names a sibling row field carrying the load-bearing
// UUID for this filter. The cell's display text becomes a non-load-bearing
// slug; the emitted token is `slug-uuid` (Notion-shaped). Filter comparison
// is on UUID only — the slug is decoration so URLs/tokens are self-describing.
const FILTER_COLUMNS: Record<
  string,
  { key: string; header: string; uuidCol?: keyof SearchRow; field?: keyof SearchRow }
> = {
  source_ref: { key: "source_id", header: "Source", field: "source_id" },
  kind: { key: "kind", header: "Type" },
  channel: { key: "channel", header: "Channel" },
  author: { key: "author", header: "Author", uuidCol: "author" },
  account: { key: "account", header: "Account", uuidCol: "account" },
  project: { key: "project", header: "Project", uuidCol: "project" },
  conversation_name: {
    key: "convo",
    header: "Title",
    uuidCol: "conversation_uuid",
  },
};

function openFeedbackForSearchBar(ev: MouseEvent) {
  ev.preventDefault();
  const anchor = ev.target instanceof Element ? ev.target : null;
  // The search bar is the entire filter set: treat it as a single chip
  // keyed "query" with the literal query text. We don't try to parse
  // individual tokens — the comment + breadcrumb is enough to find what
  // was being looked at.
  feedbackContext.value = buildContext({
    surface: "filter_chip",
    anchor,
    targetUuids: [],
    payload: { key: "query", value: query.value },
  });
  feedbackSurfaceLabel.value = "Search bar";
  feedbackOpen.value = true;
}

function appendFilterToQuery(token: string) {
  query.value = withToken(query.value, token);
}

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

// Compose `slug-uuid` (Notion URL pattern). When `slug` is empty (no display
// label available) or `uuid` is not UUID-shaped, falls back to just `uuid`.
function formatSlugUuid(slug: string, uuid: string): string {
  if (!UUID_RE.test(uuid)) return uuid;
  const s = slugify(slug);
  return s.length === 0 ? uuid : `${s}-${uuid}`;
}

/// Put one id per target on the clipboard, comma-separated.
async function copyIds(targets: SearchRow[], pick: (r: SearchRow) => string) {
  const text = targets
    .map(pick)
    .filter((v) => v.length > 0)
    .join(",");
  if (text.length === 0) return;
  await copyToClipboard(text);
}

// Build a FilterCtx for the cell at `colId` on the given row, or null
// when the column is non-filterable or has no value to filter by.
function buildFilterCtx(colId: string, data: SearchRow): FilterCtx | null {
  const meta = FILTER_COLUMNS[colId];
  if (!meta) return null;
  const row = data as Record<string, unknown>;
  const cellRaw = row[(meta.field ?? colId) as string];
  if (meta.uuidCol) {
    const uuid = row[meta.uuidCol as string];
    if (typeof uuid !== "string" || uuid.length === 0) return null;
    let displayLabel = "";
    if (colId === "author" || colId === "account") {
      displayLabel = accounts.value[uuid]?.label ?? "";
    } else if (colId === "conversation_name") {
      displayLabel = typeof cellRaw === "string" ? cellRaw : "";
    }
    return {
      key: meta.key,
      header: meta.header,
      value: formatSlugUuid(displayLabel, uuid),
    };
  }
  if (typeof cellRaw === "string" && cellRaw.length > 0) {
    return { key: meta.key, header: meta.header, value: cellRaw };
  }
  return null;
}

function accountLabel(uuid: string): string {
  if (!uuid) return "";
  return accounts.value[uuid]?.label ?? uuid;
}

let inflight: AbortController | null = null;
let debounceTimer: ReturnType<typeof setTimeout> | null = null;

// Per-query LRU cache so re-typing a recent query feels instant.
// Keyed by the exact search string. Bounded — older entries evicted on insert.
// Lives in module scope but is intentionally not exported: cache invalidates
// naturally on page reload.
const SEARCH_CACHE_MAX = 16;
// Backend's hard ceiling — anything lower surfaces as silently-missing
// rows for the user. Memory/render cost is fine at this size thanks to
// the grid's row virtualization.
const SEARCH_LIMIT = 100_000;
type SearchCacheEntry = {
  rows: SearchRow[];
  total: number;
  qmdError: string | null;
};
const searchCache = new Map<string, SearchCacheEntry>();

function cacheGet(key: string): SearchCacheEntry | undefined {
  const hit = searchCache.get(key);
  if (!hit) return undefined;
  // LRU touch: re-insert to move to the end of the iteration order.
  searchCache.delete(key);
  searchCache.set(key, hit);
  return hit;
}

function cachePut(key: string, entry: SearchCacheEntry) {
  searchCache.delete(key);
  searchCache.set(key, entry);
  while (searchCache.size > SEARCH_CACHE_MAX) {
    const oldest = searchCache.keys().next().value;
    if (oldest === undefined) break;
    searchCache.delete(oldest);
  }
}

async function runSearch(q: string) {
  inflight?.abort();
  const cached = cacheGet(q);
  if (cached) {
    rows.value = cached.rows;
    total.value = cached.total;
    loading.value = false;
    error.value = null;
    qmdError.value = cached.qmdError;
    shownQuery.value = q;
    return;
  }
  inflight = new AbortController();
  loading.value = true;
  error.value = null;
  qmdError.value = null;
  try {
    // The card shows a failure itself, beside the rows it concerns.
    const r = await fetchSearch(q, SEARCH_LIMIT, inflight.signal, { toast: false });
    if (r.columns?.length && JSON.stringify(r.columns) !== JSON.stringify(columns.value)) {
      columns.value = r.columns;
    }
    rows.value = r.rows;
    total.value = r.total_estimated;
    const qe = typeof r.query_echo?.qmd_error === "string" ? r.query_echo.qmd_error : null;
    qmdError.value = qe;
    cachePut(q, { rows: r.rows, total: r.total_estimated, qmdError: qe });
    shownQuery.value = q;
  } catch (e) {
    if ((e as { name?: string }).name === "AbortError") return;
    error.value = searchFailure(e);
  } finally {
    loading.value = false;
  }
}

watch(query, (q) => {
  if (debounceTimer) clearTimeout(debounceTimer);
  // Show the spinner immediately on input change (unless we'll serve from
  // cache) — otherwise the 150ms debounce + multi-second backend latency
  // leaves the user staring at stale rows with no feedback.
  if (!searchCache.has(q)) loading.value = true;
  debounceTimer = setTimeout(() => runSearch(q), 150);
  saveState();
});

// Restore the selected row from persisted state after rows load (or
// after the grid is first created, whichever happens last — creation
// can race with the initial fetch). Selection state outlives the
// result set: searches that drop the selected row leave selection
// cleared, which is the right behavior for a deep-link.
function tryRestoreSelection() {
  const target_sel = sel.value;
  if (!target_sel || !vueGrid || rows.value.length === 0) return;
  if (selectedRow.value && rowKey(selectedRow.value) === target_sel) return;
  const target = rows.value.find((r) => rowKey(r) === target_sel);
  if (!target) return;
  const row = vueGrid.dataView.getRowById(target_sel);
  if (row == null) return;
  restoring = true;
  vueGrid.slickGrid.setSelectedRows([row]);
  vueGrid.slickGrid.scrollRowIntoView(row);
  selectedRow.value = target;
  restoring = false;
}

// Apply the default sort whenever results change, unless the user has
// taken sort into their own hands.
//   - qmd-scored results → score desc, scroll to top.
//   - everything else    → time ascending, scroll to bottom so the most
//                          recent rows are what the user lands on.
function applyDefaultSort() {
  if (!vueGrid) return;
  if (userSortedManually) {
    // Fresh rows arrive in the server's order: put them in the one
    // chosen before anything looks up where a row is.
    applySort(vueGrid.sortService.getCurrentLocalSorters());
    return;
  }
  const hasScores = rows.value.some((r) => typeof r.score === "number");
  applySort(
    hasScores
      ? [{ columnId: "score", direction: "DESC" }]
      : [{ columnId: "created_at", direction: "ASC" }],
  );
  if (sel.value) {
    // tryRestoreSelection will scroll to the pinned row; don't fight it.
    return;
  }
  const grid = vueGrid.slickGrid;
  const last = grid.getDataLength() - 1;
  if (last < 0) return;
  grid.scrollRowIntoView(hasScores ? 0 : last);
}

// Adaptive column visibility: on every results load, columns whose
// values are all identical (including all-empty) get hidden; columns
// with varying values get shown. "Adaptive rule wins" — manual
// column-visibility toggles get overwritten on the next query.
//
// This list is what the rule may *reveal*, so it is deliberately not
// "every optional column": a column named here appears in the default
// grid whenever its values vary, which is exactly what `hidden` on a
// definition is there to prevent. It stays the set it has always been.
/// Column id → the row field it reads.
const ADAPTIVE_FIELDS: Record<string, keyof SearchRow> = {
  score: "score",
  kind: "kind",
  channel: "channel",
  created_at: "created_at",
  author: "author",
  account: "account",
};

/// The preset's columns, or null when this card has none — or when the
/// user's own persisted column state has superseded it, which is the
/// same rule `q` follows: once someone has moved a column, this card is
/// theirs. A function, not a computed: `colsEncoded` is a plain `let`,
/// so a computed would cache its first answer forever.
function presetColumns(): Set<string> | null {
  return !colsEncoded && props.columns?.length ? new Set(props.columns) : null;
}

// A card opened with a `columns` preset runs the rule over the preset's
// own columns instead. Those are already on screen, so there the rule
// can only *trim* — hiding whichever the source leaves empty, never
// revealing one the preset left out. That split is what lets a preset
// be generous: it names what would be meaningful for the source, and
// this decides what is actually there.
//
// `snippet` is excluded because it is the content, not a facet: a
// corpus where every row's text matched would otherwise hide the one
// column worth reading.
function adaptiveFields(): [string, keyof SearchRow][] {
  const allowed = presetColumns();
  if (!allowed) return Object.entries(ADAPTIVE_FIELDS) as [string, keyof SearchRow][];
  return [...allowed]
    .filter((c) => c !== "snippet")
    .map((c) => [c, ADAPTIVE_FIELDS[c] ?? (c as keyof SearchRow)]);
}

function stringifyForCompare(v: unknown): string {
  if (v == null) return "";
  return typeof v === "string" ? v : String(v);
}

function applyAdaptiveVisibility() {
  if (!vueGrid || rows.value.length === 0) return;
  const hidden: Record<string, boolean> = {};
  for (const [colId, field] of adaptiveFields()) {
    const first = stringifyForCompare(rows.value[0][field]);
    hidden[colId] = rows.value.every((r) => stringifyForCompare(r[field]) === first);
  }
  setHidden(hidden);
}

/// Show exactly the preset's columns, in its order, and hide every
/// other optional one. Runs once, before any results land, so the first
/// paint is already the right shape rather than flickering through the
/// default set.
function applyPresetColumns() {
  const wanted = props.columns;
  if (!vueGrid || !wanted?.length || colsEncoded) return;
  const widths = new Map(vueGrid.slickGrid.getColumns().map((c) => [String(c.id), c.width]));
  const ordered: CurrentColumn[] = wanted.map((columnId) => ({
    columnId,
    hidden: false,
    width: widths.get(columnId),
  }));
  // …and hide everything else the grid offers. `snippet` is in every
  // preset, so nothing here can hide the text column by accident.
  const set = new Set(wanted);
  const rest: CurrentColumn[] = vueGrid.slickGrid
    .getColumns()
    .filter((c) => !set.has(String(c.id)))
    .map((c) => ({ columnId: String(c.id), hidden: true, width: c.width }));
  applyColumns([...ordered, ...rest]);
}

/// The rows last handed to the grid, and the query they answer. A new
/// answer to the same query is a refresh: the index moved under a view
/// the person is still looking at.
let handed: Handed = new Map();
let handedQuery: string | null = null;

// A new query replaces the rows and shapes the columns, the sort and
// the scroll around them. A refresh of the one shown changes only the
// rows that changed, and leaves the rest to the person.
watch(rows, (r) => {
  if (!vueGrid) return;
  const refresh = handedQuery !== null && handedQuery === shownQuery.value;
  handedQuery = shownQuery.value;
  if (refresh) {
    const next = patchRows(handed, r, rowKey);
    handed = next.handed;
    applyPatch(next.patch);
  } else {
    handed = handedOf(r, rowKey);
    vueGrid.dataset = r;
    applyAdaptiveVisibility();
    applyDefaultSort();
  }
  tryRestoreSelection();
  // Fire-and-forget: the badges fill in a beat after the rows land
  // rather than holding the result set hostage to a second request.
  refreshQmdState();
});

/// Apply a refresh in place. Rows inserted above the viewport would
/// push what the person is reading down, so the row at the top is held
/// at the top.
function applyPatch(patch: RowPatch<SearchRow>) {
  if (!vueGrid || isEmpty(patch)) return;
  const { dataView, slickGrid: grid } = vueGrid;
  const top = grid.getViewport().top;
  const anchor = rowData(top)?.uuid ?? null;
  keepActiveOnRecord(grid, dataView, () => {
    redrawChanged(grid, dataView, () => {
      dataView.beginUpdate();
      for (const id of patch.removed) dataView.deleteItem(id);
      for (const row of patch.changed) dataView.updateItem(row.uuid, row);
      for (const row of patch.added) dataView.addItem(row);
      // A new or changed row takes its place in whatever order is showing.
      dataView.reSort();
      dataView.endUpdate();
    });
    const moved = anchor ? dataView.getRowById(anchor) : undefined;
    if (moved != null && moved !== top) grid.scrollRowToTop(moved);
  });
}

onMounted(async () => {
  try {
    accounts.value = await fetchAccounts();
  } catch {
    /* accounts mapping is best-effort */
  }
  runSearch(query.value);
});

// The index moved under us — a `grid_index` pass committed, which under
// streaming happens many times per sync, as each source's rows arrive.
// Every cached answer is stale, so drop them all and ask the shown query
// again; the row set updates in place while the download is still going.
const cardEl = ref<HTMLElement | null>(null);
let unsubscribeLive: (() => void) | null = null;
onMounted(() => {
  unsubscribeLive = subscribeLive(
    {
      root: (e) => {
        if (e.kind !== "index_changed") return;
        searchCache.clear();
        void runSearch(query.value);
      },
    },
    { onScreen: cardEl.value ?? undefined },
  );
});
onBeforeUnmount(() => unsubscribeLive?.());

function docSource(md: string, anchor: string | null): string {
  const args = [md, anchor].map((a) => JSON.stringify(a)).join(", ");
  return `documentView(${args})`;
}

function openRow(row: SearchRow) {
  // Double-click → open this row's doc as a standalone single-column
  // page in a new tab, with the row's section highlighted.
  const md = row.markdown_uuid ?? row.uuid;
  const href = encodeColumns([{ code: docSource(md, row.uuid), state: "" }]);
  window.open(href, "_blank", "noopener");
}

/// A cell's text with the account name where the value is an account's
/// uuid.
const accountFormatter: Formatter<SearchRow> = (_r, _c, value) => {
  const v = typeof value === "string" ? value : "";
  const label = accountLabel(v);
  return { text: label, toolTip: v && label !== v ? v : "" };
};

/// What this card adds to the applet's declared columns: the widths and
/// hovers a type cannot know, the account-name formatting on the
/// author/account cells (the accounts map is the browser's), and the
/// two-line clamp on the text.
const columnOverrides: Record<string, Partial<Column<SearchRow>>> = {
  // Default sort is applied programmatically on row updates (see
  // applyDefaultSort) — not baked into the definition so a user re-sort
  // sticks across query changes.
  source_ref: { width: 150 },
  kind: { width: 110 },
  conversation_name: { width: 200 },
  channel: { width: 130 },
  snippet: {
    width: 600,
    // Two-line clamp via our own <div>, so the clamp styles land on the
    // direct text container. The row height is fixed at 52px to fit
    // two lines; per-row measurement was the dominant render cost on
    // large result sets.
    formatter: (_r, _c, value) => {
      const div = document.createElement("div");
      div.className = "datalib-clamp-2";
      div.textContent = value == null ? "" : String(value);
      return div;
    },
  },
  author: {
    width: 130,
    formatter: accountFormatter,
    grouping: {
      getter: (row: SearchRow) => accountLabel(row.author ?? ""),
      formatter: groupTitle("Author"),
      collapsed: false,
    },
  },
  account: {
    formatter: accountFormatter,
    grouping: {
      getter: (row: SearchRow) => accountLabel(row.account ?? ""),
      formatter: groupTitle("Account"),
      collapsed: false,
    },
  },
  // Cell renders the human-readable org_name; the row also carries
  // org_uuid (shown on hover) so filtering / scripts can target the
  // stable opaque key.
  org_name: {
    width: 130,
    formatter: (_r, _c, value, _col, row) => ({
      text: value == null ? "" : String(value),
      toolTip: row?.org_uuid ?? "",
    }),
  },
};

/// Two columns rather than one combined "search state": `qmd update`
/// and `qmd embed` are separate passes, so "in the keyword index" and
/// "reachable by semantic search" are genuinely different facts, and
/// the gap between them is exactly what a user hunting a missing
/// result needs to see. The card's own, not the applet's: they are
/// answered by a second request the card makes only when they are on
/// screen.
const extraColumns: Column<SearchRow>[] = [
  {
    id: "qmd_indexed",
    field: "markdown_uuid",
    name: "Indexed",
    hidden: true,
    toolTip:
      "Whether this row's rendered document is in the qmd keyword index, at its current content",
    width: 100,
    cssClass: "tg-center",
    cellAttrs: { "col-id": "qmd_indexed" },
    headerCellAttrs: { "col-id": "qmd_indexed" },
    formatter: flagFormatter("indexed"),
    sortable: false,
  },
  {
    id: "qmd_embedded",
    field: "markdown_uuid",
    name: "Embedded",
    hidden: true,
    toolTip:
      "Whether this document has a complete set of embedding vectors — semantic search cannot reach it until it does",
    width: 110,
    cssClass: "tg-center",
    cellAttrs: { "col-id": "qmd_embedded" },
    headerCellAttrs: { "col-id": "qmd_embedded" },
    formatter: flagFormatter("embedded"),
    sortable: false,
  },
];

/// The preset's columns, then the layout the URL carries. The grid is
/// created only once the applet has declared its columns, so by then
/// there is something to apply them to.
function applyInitialLayout() {
  if (!vueGrid) return;
  applyPresetColumns();
  if (!colsEncoded) return;
  const layout = decodeLayout(colsEncoded);
  if (!layout) return;
  applyColumns(layout.cols.map((c) => ({ columnId: c.id, hidden: !!c.hidden, width: c.width })));
  if (layout.sort?.length) {
    applySort(layout.sort.map((s) => ({ columnId: s.id, direction: s.asc ? "ASC" : "DESC" })));
    // An explicit persisted layout carries the user's sort choice —
    // don't clobber it with our default.
    userSortedManually = true;
  }
  if (layout.group?.length && groupingPlugin) {
    restoring = true;
    groupingPlugin.setDroppedGroups(layout.group);
    restoring = false;
  }
}

/// The grid's columns: the applet's, drawn by type, refined by the
/// overrides above, with the card's own beside `project` — among the
/// facets, where a 640px card still has them on screen. Built once per
/// declaration, since handing the grid new definitions resets its
/// layout.
const gridColumns = shallowRef<Column<SearchRow>[]>([]);
watch(
  columns,
  (specs) => {
    // Nothing declared yet: the card's own two columns alone are not a
    // grid worth building.
    if (specs.length === 0) return;
    const typed = typedColumns<SearchRow>(specs, {
      rows: () => rows.value,
      overrides: columnOverrides,
      groupable: true,
      filterable: true,
    });
    const at = typed.findIndex((c) => c.id === "project") + 1;
    gridColumns.value = [...typed.slice(0, at), ...extraColumns, ...typed.slice(at)];
    createGrid();
  },
  { immediate: true },
);

/// A menu entry drawn like the built-in ones (icon slot, then text),
/// with a label decided when the menu opens. The grid copies the options
/// it is given, so an entry cannot be retitled from outside once the
/// menu exists; a renderer is handed the cell instead.
function entry(
  command: string,
  label: (m: MenuScope) => string | null,
  run: (m: MenuScope) => void,
): MenuCommandItem {
  return {
    command,
    itemVisibilityOverride: (args) => label(scopeOf(args)) !== null,
    slotRenderer: (_item, args) => {
      const wrap = document.createElement("div");
      // The menu item lays its icon and text out itself; the wrapper
      // only exists because a renderer returns one element.
      wrap.style.display = "contents";
      const icon = document.createElement("div");
      icon.className = "slick-menu-icon";
      icon.textContent = "◦";
      const text = document.createElement("span");
      text.className = "slick-menu-content";
      text.textContent = label(scopeOf(args)) ?? "";
      wrap.append(icon, text);
      return wrap;
    },
    action: (_e, args) => run(scopeOf(args)),
  };
}

/// A divider that shows only when something above it did.
function dividerAfter(shown: (m: MenuScope) => boolean): MenuCommandItem {
  return {
    command: "",
    divider: true,
    itemVisibilityOverride: (args) => shown(scopeOf(args)),
  };
}

/// What one right-click is about: the row under it, the rows it aims
/// at, and the filters its cell offers.
type MenuScope = {
  anchor: SearchRow | null;
  /// The cell under the click: its column, its painted text (closer to
  /// what the user saw — an author's name, not their uuid — than the
  /// row's field), and its element, for the feedback breadcrumb.
  cell: { column: string; cellValue: string; el: HTMLElement | null } | null;
  targets: SearchRow[];
  filter: FilterEntry[];
  notion: FilterEntry[];
  links: { web: SearchRow[]; local: string[] };
};

const linkOf = (r: SearchRow): string => r.source_url || "";

function menuScope(args: MenuFromCellCallbackArgs): MenuScope {
  const anchor = args.row != null ? rowData(args.row) : null;
  const colId = String((args.column as Column | undefined)?.id ?? "");
  const el =
    args.row != null && args.cell != null
      ? (args.grid.getCellNode(args.row, args.cell) ?? null)
      : null;
  const cell = colId ? { column: colId, cellValue: el?.textContent?.trim() ?? "", el } : null;
  const targets = resolveTargetRows(anchor);
  const filterCtx = anchor ? buildFilterCtx(colId, anchor) : null;
  // Optional "Filter by Notion Page" entries, populated when the right-
  // clicked row has a non-empty `notion_page_uuid`. Lets users zoom into
  // all rows on a single Notion page from any cell of any row on that
  // page — useful because the page UUID isn't always the same as
  // conversation_uuid (e.g. comment threads use the discussion UUID).
  const notionCtx: FilterCtx | null = anchor?.notion_page_uuid
    ? {
        key: "notion_page",
        header: "Notion Page",
        value: formatSlugUuid(
          anchor.conversation_uuid === anchor.notion_page_uuid ? anchor.conversation_name : "",
          anchor.notion_page_uuid,
        ),
      }
    : null;
  // A row's outbound linkout is source_url (Slack permalink, LinkedIn
  // post, …). Local files (today: the `pdf` source) carry a `file://` URL, which
  // `window.open` cannot usefully follow from an http origin — a
  // browser blocks it silently. Split on the URL SCHEME rather than on
  // provider, so any future local-file source inherits this.
  const linked = targets.filter((r) => linkOf(r));
  return {
    anchor,
    cell,
    targets,
    filter: filterCtx ? keepExcludeEntries(filterCtx) : [],
    notion: notionCtx ? keepExcludeEntries(notionCtx) : [],
    links: {
      web: linked.filter((r) => !filePathFromUrl(linkOf(r))),
      local: linked.map((r) => filePathFromUrl(linkOf(r))).filter((p): p is string => p !== null),
    },
  };
}

/// A right-click's scope, worked out once as the menu opens: see
/// `perOpening`.
const scopes = perOpening(menuScope);
const scopeOf = (args: unknown) => scopes.read(args as MenuFromCellCallbackArgs);

const plural = (m: MenuScope) => (m.targets.length === 1 ? "" : "s");
const countSuffix = (n: number) => (n === 1 ? "" : ` (${n})`);

function openFeedback(surface: "grid_cell" | "grid_row", m: MenuScope) {
  const rowUuids = m.targets.map((r) => r.uuid);
  const anchor = m.cell?.el ?? null;
  if (surface === "grid_cell" && m.cell) {
    feedbackContext.value = buildContext({
      surface,
      anchor,
      targetUuids: rowUuids,
      payload: { column: m.cell.column, row_uuids: rowUuids, cell_value: m.cell.cellValue || null },
    });
    feedbackSurfaceLabel.value = `Grid cell · ${m.cell.column}${
      m.targets.length > 1 ? ` · ${m.targets.length} rows` : ""
    }`;
  } else {
    feedbackContext.value = buildContext({
      surface: "grid_row",
      anchor,
      targetUuids: rowUuids,
      payload: { row_uuids: rowUuids },
    });
    feedbackSurfaceLabel.value =
      m.targets.length === 1 ? "Grid row" : `Grid rows · ${m.targets.length}`;
  }
  feedbackOpen.value = true;
}

// The right-click menu, ahead of the grid's own entries (copy the cell,
// the grouping commands). Each entry decides for itself whether the
// cell under the click gives it anything to do.
const menuItems: (MenuCommandItem | "divider")[] = [
  entry(
    "keep",
    (m) => m.filter[0]?.label ?? null,
    (m) => appendFilterToQuery(m.filter[0].token),
  ),
  entry(
    "exclude",
    (m) => m.filter[1]?.label ?? null,
    (m) => appendFilterToQuery(m.filter[1].token),
  ),
  dividerAfter((m) => m.filter.length > 0),
  entry(
    "keep-notion",
    (m) => m.notion[0]?.label ?? null,
    (m) => appendFilterToQuery(m.notion[0].token),
  ),
  entry(
    "exclude-notion",
    (m) => m.notion[1]?.label ?? null,
    (m) => appendFilterToQuery(m.notion[1].token),
  ),
  dividerAfter((m) => m.notion.length > 0),
  entry(
    "copy-uuids",
    (m) => (m.targets.length ? `Copy UUID${plural(m)}` : null),
    (m) => void copyIds(m.targets, (r) => r.uuid),
  ),
  // Only offered when at least one selected row actually carries an
  // upstream id — a provider that hasn't been ported onto
  // `datalib_id` writes NULL here, and a menu item that silently
  // copies nothing is worse than no menu item.
  entry(
    "copy-upstream",
    (m) => (m.targets.some((r) => r.upstream_id) ? `Copy upstream ID${plural(m)}` : null),
    (m) => void copyIds(m.targets, (r) => r.upstream_id ?? ""),
  ),
  entry(
    "open-source",
    (m) => (m.links.web.length ? `Open source${countSuffix(m.links.web.length)}` : null),
    (m) => {
      for (const r of m.links.web) {
        // `openExternal`, not `window.open`: the desktop app has no
        // tabs and does not implement `window.open`, so the bare
        // call was a silent no-op there.
        void openExternal(linkOf(r));
      }
    },
  ),
  // Only offered in the desktop app: revealing a file is something a
  // browser fundamentally cannot do. In a browser the honest fallback
  // is handing over the path, rather than an "Open" that quietly does
  // nothing.
  entry(
    "reveal",
    (m) =>
      m.links.local.length
        ? isDesktopApp()
          ? `${revealActionLabel()}${countSuffix(m.links.local.length)}`
          : `Copy file path${countSuffix(m.links.local.length)}`
        : null,
    (m) => {
      const paths = m.links.local;
      if (!isDesktopApp()) {
        void navigator.clipboard.writeText(paths.join("\n"));
        return;
      }
      void (async () => {
        const failed: string[] = [];
        for (const p of paths) {
          if (!(await revealInFileManager(p))) failed.push(p);
        }
        // The IPC bridge can be present while the reveal command is
        // unauthorized (a capability whose URL patterns stopped
        // matching, say). Silently doing nothing is the worst outcome,
        // so degrade to the same thing the browser offers.
        if (failed.length > 0) void navigator.clipboard.writeText(failed.join("\n"));
      })();
    },
  ),
  entry(
    "feedback-cell",
    (m) => (m.targets.length && m.cell ? "Feedback on this cell…" : null),
    (m) => openFeedback("grid_cell", m),
  ),
  entry(
    "feedback-row",
    (m) => (m.targets.length ? `Feedback on row${plural(m)}…` : null),
    (m) => openFeedback("grid_row", m),
  ),
  "divider",
];

const GROUP_HINT = "Drag columns here to group rows by them — source, then type, say";

function isDark(): boolean {
  return document.documentElement.dataset.theme === "dark";
}

function gridOptions(): GridOption {
  return {
    datasetIdPropertyName: "uuid",
    // Cells are text, never markup: a row's snippet is the source's own.
    enableHtmlRendering: false,
    enableEmptyDataWarningMessage: false,
    darkMode: isDark(),
    enableAutoResize: true,
    ...KEEP_COLUMN_WIDTHS,
    autoResize: {
      // The frame around the box, not the box: the resizer sizes the
      // box to what it measures, and a box it also measured would then
      // stop following the card. The frame is what the card sizes.
      container: boxEl.value!.parentElement!,
      calculateAvailableSizeBy: "container",
      resizeDetection: "container",
      autoHeight: false,
      bottomPadding: 0,
      minHeight: 200,
    },
    // Tall enough for two lines of clamped snippet text plus padding.
    rowHeight: 52,
    enableTextSelectionOnCells: true,
    // Rows select on click, several with a modifier — so right-click
    // "Copy UUID(s)" can target several rows, like Lightroom. The
    // document column follows whichever row was most recently selected.
    enableCellNavigation: true,
    enableSelection: true,
    multiSelect: true,
    selectionOptions: { selectActiveRow: true },
    // Per-column filters in a row under the header; the query bar is
    // the one the server answers, these narrow what it returned.
    enableFiltering: true,
    ...FILTER_GRID_OPTIONS,
    showHeaderRow: true,
    headerRowHeight: 28,
    defaultFilterPlaceholder: "",
    filterTypingDebounce: 250,
    enableSorting: true,
    multiColumnSort: false,
    enableColumnReorder: true,
    enableHeaderMenu: true,
    enableGridMenu: true,
    enableColumnPicker: true,
    // Grouping this grid by source is the single most useful thing to do
    // with it — the unified projection holds every source at once, and
    // "which of my things is this" is the first question anyone asks —
    // and the bar that does it reads as decoration until you know. So it
    // says what it is for.
    enableGrouping: true,
    enableDraggableGrouping: true,
    createPreHeaderPanel: true,
    showPreHeaderPanel: true,
    preHeaderPanelHeight: 30,
    draggableGrouping: {
      dropPlaceHolderText: GROUP_HINT,
      // One control to fold or open every group, shown only while
      // something is grouped; the right-click menu has the same pair.
      hideToggleAllButton: false,
      toggleAllButtonText: "Expand / collapse all",
      toggleAllPlaceholderText: "Fold every group, or open every one",
      deleteIconCssClass: "mdi mdi-close",
      sortAscIconCssClass: "mdi mdi-arrow-up",
      sortDescIconCssClass: "mdi mdi-arrow-down",
      onGroupChanged: () => updateCols(),
      onExtensionRegistered: (plugin) => {
        groupingPlugin = plugin;
      },
    },
    enableContextMenu: true,
    contextMenu: { commandItems: menuItems, onBeforeMenuShow: scopes.onBeforeMenuShow },
  };
}

/// A diff group's rows say how they differ from the other commit
/// (`diff_status`; null on every real source's rows), and a modified
/// row names the columns that moved (`diff_changed_columns`, the
/// `grid_rows` names, `|`-joined). The row takes a band for the first
/// and the cell a highlight for the second — through the data view's
/// item metadata, which the grid reads for every row it paints. The
/// grouping extension installs its own provider for group rows, so
/// this wraps whatever is there rather than replacing it.
function changedColumns(row: SearchRow | undefined): Set<string> {
  const names = row?.diff_changed_columns;
  if (!names) return new Set();
  // The body is shown as Contents: its preview, or its hash when the
  // change is past the preview.
  const asShown = (c: string) => (c === "preview" || c === "content_hash" ? "snippet" : c);
  return new Set(names.split("|").map(asShown));
}

function installDiffMetadata(dataView: Grid["dataView"]) {
  const inner = dataView.getItemMetadata.bind(dataView);
  dataView.getItemMetadata = (row: number) => {
    const meta = inner(row);
    const item = dataView.getItem(row) as SearchRow | undefined;
    const status = item?.diff_status;
    if (!status || status === "unchanged") return meta;
    const columns: Record<string, { cssClass: string }> = {};
    if (status === "modified") {
      for (const id of changedColumns(item)) columns[id] = { cssClass: "datalib-diff-cell" };
    }
    return {
      ...(meta ?? {}),
      cssClasses: [meta?.cssClasses, `datalib-diff-${status}`].filter(Boolean).join(" "),
      columns: { ...(meta?.columns ?? {}), ...columns },
    };
  };
}

/// Build the grid, once the box is on the page and the applet has
/// declared its columns — whichever comes second. Handing a grid new
/// definitions resets its layout, so the columns it is built with are
/// the ones it keeps.
function createGrid() {
  if (vueGrid || !boxEl.value || gridColumns.value.length === 0) return;
  const options = gridOptions();
  const root = boxEl.value.getRootNode();
  // The grid's own stylesheet must live in this card's shadow root,
  // where the grid is.
  if (root instanceof ShadowRoot) options.shadowRoot = root;
  const bundle = new SlickVanillaGridBundle<SearchRow>(
    boxEl.value,
    gridColumns.value,
    options,
    rows.value,
  ) as Grid;
  vueGrid = bundle;
  installDiffMetadata(bundle.dataView);
  const grid = bundle.slickGrid;
  grid.onSelectedRowsChanged.subscribe(onSelectedRowsChanged);
  grid.onClick.subscribe(onClick);
  grid.onDblClick.subscribe(onDblClick);
  grid.onViewportChanged.subscribe(onViewportChanged);
  bundle.instances?.eventPubSubService?.subscribe<GridStateChange>(
    "onGridStateChanged",
    onGridStateChanged,
  );
  // Expose the grid so e2e tests can scroll virtualized rows into view
  // before clicking. Last grid card wins when several are open — fine
  // for tests, which drive a single grid.
  (window as unknown as { __fwGridApi?: unknown }).__fwGridApi = {
    rowIndexOf: (uuid: string) => bundle.dataView.getRowById(uuid) ?? null,
    uuidAt: (row: number) => (bundle.dataView.getItem(row) as SearchRow | undefined)?.uuid ?? null,
    rows: () => bundle.dataView.getItems() as SearchRow[],
    filteredRows: () => bundle.dataView.getFilteredItems() as SearchRow[],
    scrollToRow: (row: number) => grid.scrollRowIntoView(row),
    scrollToColumn: (id: string) => {
      const idx = grid.getColumnIndex(id);
      if (idx != null) grid.scrollColumnIntoView(idx);
    },
    isSelected: (uuid: string) => selectedRows().some((r) => r.uuid === uuid),
    activeUuid: () => {
      const active = grid.getActiveCell();
      return active ? (rowData(active.row)?.uuid ?? null) : null;
    },
    hiddenColumns: () =>
      grid
        .getColumns()
        .filter((c) => c.hidden)
        .map((c) => String(c.id)),
    // What the column picker and the group bar do, without the mouse.
    showColumns: (ids: string[]) => {
      setHidden(Object.fromEntries(ids.map((id) => [id, false])));
      onColumnsShown();
    },
    groupBy: (ids: string[]) => groupingPlugin?.setDroppedGroups(ids),
  };
  applyInitialLayout();
  // The rows may already be loaded by the time the grid exists — the
  // columns arrive with the first results — so this is the moment the
  // `rows` watcher would otherwise have.
  handed = handedOf(rows.value, rowKey);
  handedQuery = shownQuery.value;
  applyAdaptiveVisibility();
  applyDefaultSort();
  tryRestoreSelection();
  refreshQmdState();
}

/// The records selected as of the last change the grid reported.
let selectedIds = new Set<string>();

function onSelectedRowsChanged(_e: SlickEventData, args: OnSelectedRowsChangedEventArgs) {
  if (!vueGrid) return;
  const now = args.rows.map(rowData).filter((d): d is SearchRow => d != null);
  const { picked, selected } = newlyPicked(selectedIds, now, rowKey);
  selectedIds = selected;
  const data = picked[picked.length - 1];
  if (!data) return;
  selectedRow.value = data;
  sel.value = rowKey(data);
  // `restoring` is true when this is a URL-driven re-selection — the
  // document column is already in the URL, so don't open a duplicate
  // (and don't rewrite the state we just read).
  if (restoring) return;
  saveState();
  const md = data.markdown_uuid ?? data.uuid;
  props.ctx.host.openCards(docSource(md, data.uuid));
}

function onClick(_e: SlickEventData, args: OnClickEventArgs) {
  // A data row that is already the one selected selects again as far
  // as the reader is concerned, though the selection model sees no
  // change: keep the persisted selection on it.
  const data = rowData(args.row);
  const selected = selectedRows();
  if (data && selected.length === 1 && selected[0].uuid === data.uuid) {
    selectedRow.value = data;
    sel.value = rowKey(data);
    saveState();
  }
}

function onDblClick(_e: SlickEventData, args: OnDblClickEventArgs) {
  const data = rowData(args.row);
  if (data) openRow(data);
}

// Any change a USER can make to columns gets reflected in the persisted
// state: the grid reports resize, reorder, picker and sort changes
// here, and only those — its own layout work (a fit to the card's
// width, the adaptive visibility above) never does.
function onGridStateChanged(change: GridStateChange) {
  if (restoring) return;
  if (change.change?.type === "sorter") userSortedManually = true;
  updateCols();
  if (change.change?.type === "columns") onColumnsShown();
}

// Turning an index-state column on is the first moment we owe the user
// per-document answers. Asking only for what is missing, hiding and
// re-showing refetches nothing already held for these rows.
function onColumnsShown() {
  askAboutVisibleRows();
}

/// The app's theme is an attribute on `<html>`; the grid's is an option.
let themeWatch: MutationObserver | null = null;
onMounted(() => {
  createGrid();
  themeWatch = new MutationObserver(() => vueGrid?.setDarkMode(isDark()));
  themeWatch.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ["data-theme"],
  });
});
onBeforeUnmount(() => {
  themeWatch?.disconnect();
  themeWatch = null;
  vueGrid?.dispose();
  vueGrid = null;
});
</script>

<template>
  <div ref="cardEl" class="grid-column">
    <div class="search-input-wrap">
      <input
        v-model="query"
        placeholder="search messages…  (try: source:Slack, -channel:announce, before:2025-01-01)"
        class="search-input"
        data-testid="search-input"
        autofocus
        @contextmenu="openFeedbackForSearchBar"
      />
      <button
        v-if="query.length > 0"
        type="button"
        class="search-clear"
        aria-label="Clear search"
        title="Clear search"
        data-testid="search-clear"
        @click="query = ''"
      >
        ×
      </button>
    </div>

    <div class="status">
      {{ rows.length }} rows (of {{ total }})
      <span v-if="qmdSummary" class="qmd-summary" :title="qmdSummaryTitle">
        · {{ qmdSummary.embedded.toLocaleString() }} of
        {{ qmdSummary.documents.toLocaleString() }} documents searchable
      </span>
    </div>

    <p v-if="qmdError" class="qmd-error" role="alert">Free-text search failed: {{ qmdError }}</p>

    <p v-if="error" class="error" role="alert" :title="error.detail">
      {{ error.message }}
      <template v-if="showingStale">The rows below are from the previous search.</template>
      <button type="button" class="error-retry" @click="runSearch(query)">Retry</button>
    </p>

    <div class="grid-wrap" :data-shown-query="shownQuery">
      <!-- The grid is built into this box by `createGrid`, once the
           applet has declared its columns. -->
      <!-- Two elements: the grid adds classes of its own to the box it
           is built in (its theme's dark mode among them), and a Vue
           class binding on that same element would wipe them on every
           change. -->
      <div class="grid" :class="{ 'grid--loading': loading, 'grid--stale': showingStale }">
        <div ref="boxEl" class="grid-box" />
      </div>
      <div v-if="loading" class="grid-spinner" aria-label="searching">
        <div class="grid-spinner__ring" />
        <div class="grid-spinner__label">searching…</div>
      </div>
    </div>
    <p v-if="!loading && rows.length === 0 && !error && !qmdError" class="empty">no matches.</p>

    <FeedbackModal
      :open="feedbackOpen"
      :surface-label="feedbackSurfaceLabel"
      :context="feedbackContext"
      @close="feedbackOpen = false"
    />
  </div>
</template>

<style scoped>
.qmd-summary {
  opacity: 0.75;
  cursor: help;
}

.grid-column {
  display: flex;
  flex-direction: column;
  height: 100%;
  gap: 0.5rem;
  padding: 0.5rem;
  box-sizing: border-box;
}
.search-input-wrap {
  position: relative;
  width: 100%;
}
.search-input {
  width: 100%;
  padding: 0.5rem 2rem 0.5rem 0.75rem;
  font-size: 1rem;
  box-sizing: border-box;
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
}
.search-clear {
  position: absolute;
  top: 50%;
  right: 0.4rem;
  transform: translateY(-50%);
  width: 1.4rem;
  height: 1.4rem;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 0;
  font-size: 1.1rem;
  line-height: 1;
  color: var(--datalib-muted);
  background: transparent;
  border: none;
  border-radius: 50%;
  cursor: pointer;
}
.search-clear:hover {
  color: var(--datalib-fg);
  background: var(--datalib-border);
}
.status {
  font-size: 0.85rem;
  color: var(--datalib-muted);
}
.empty,
.error {
  color: var(--datalib-muted);
}
.error {
  color: #e35d6a;
}
.error-retry {
  margin-left: 0.4rem;
  padding: 0.1rem 0.5rem;
  font: inherit;
  font-size: 0.85rem;
  color: var(--datalib-fg);
  background: transparent;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
  cursor: pointer;
}
.error-retry:hover {
  background: var(--datalib-border);
}
.qmd-error {
  padding: 0.4rem 0.6rem;
  border: 1px solid #d18a3a;
  border-radius: 4px;
  background: rgba(209, 138, 58, 0.1);
  color: #d18a3a;
  font-size: 0.9rem;
}
.grid-wrap {
  flex: 1 1 auto;
  min-height: 200px;
  position: relative;
}
.grid {
  /* Fill the positioned .grid-wrap absolutely instead of with
     height:100%. WebKit (Safari + the Tauri WKWebView) resolves a
     percentage height against a flex-sized parent with no explicit
     height as `auto`, collapsing the grid to its row-group panel
     (~50px) while Chromium gives it the full flexed height.

     tests/e2e/grid-populated.spec.ts asserts this grid's painted height
     under the suite's `webkit` project, but note what that does and
     does not prove: reverting this rule to `height: 100%` today does
     NOT fail that spec, because `.card-app-root` above us is
     `position: absolute` and hands the whole chain a definite height,
     so the percentage resolves after all. The assertion guards the
     surface (it fails the moment that stops being true); the rule stays
     because it is correct independent of what an ancestor happens to
     do. The same pattern under `.m2-grid` in cards/sourcesCard.css —
     flex-sized, no positioned ancestor — did collapse, to 2px, and its
     spec does fail without the fix. */
  position: absolute;
  inset: 0;
  transition: filter 120ms ease-out;
}
.grid-box {
  height: 100%;
}
.grid--loading {
  filter: blur(2px);
  pointer-events: none;
}
.grid--stale {
  opacity: 0.5;
}
.grid-spinner {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 0.75rem;
  pointer-events: none;
  z-index: 5;
}
.grid-spinner__ring {
  width: 36px;
  height: 36px;
  border-radius: 50%;
  border: 3px solid var(--datalib-border);
  border-top-color: var(--datalib-accent, #4a8bff);
  animation: datalib-spin 800ms linear infinite;
}
.grid-spinner__label {
  font-size: 0.85rem;
  color: var(--datalib-muted);
}
@keyframes datalib-spin {
  to {
    transform: rotate(360deg);
  }
}
</style>

<style>
/* Built by a cellRenderer, so it never receives the scoped-style
   attribute — same reason `.datalib-clamp-2` lives in
   this unscoped block. */
.qmd-flag {
  display: inline-block;
  font-size: 0.95em;
  line-height: 1;
}
.datalib-clamp-2 {
  display: -webkit-box;
  -webkit-box-orient: vertical;
  -webkit-line-clamp: 2;
  line-clamp: 2;
  overflow: hidden;
  white-space: normal;
  line-height: 1.25;
  width: 100%;
  /* `word-break: normal` keeps line wraps at word boundaries, while
     `overflow-wrap: break-word` still allows a single super-long word
     to break when it can't fit on its own line. The line-clamp
     ellipsis on line 2 is independent of `word-break` and will land
     mid-word when truncating a long word, which is fine — wraps stay
     clean, only the visible truncation cuts mid-word. */
  word-break: normal;
  overflow-wrap: break-word;
}
</style>
