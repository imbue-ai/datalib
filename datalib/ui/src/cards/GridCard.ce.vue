<script setup lang="ts">
// Search-grid card: a search bar + a slickgrid over a table the
// unified_index applet pages — its /search results by default, or another
// endpoint that pages the same way (/problems) — a page at a time
// (grid/pagedWindow.ts). The server orders the rows: a header click asks
// it again in the new order. What a row is named by, the document it
// opens and whether qmd ranks its free text, the server declares
// (`RowsSpec`).
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
  GroupingFormatterItem,
  MenuCommandItem,
  MenuFromCellCallbackArgs,
  OnClickEventArgs,
  OnDblClickEventArgs,
  OnSelectedRowsChangedEventArgs,
  SlickDraggableGrouping,
  SlickEventData,
} from "@slickgrid-universal/common";
import { copyText, typedColumns, groupTitle } from "./typedColumns";
import type { Identity } from "@/api";
import { entityFromUri, handleFromUri } from "./chipLinks";
import {
  browseQuery,
  entities,
  entityCardSource,
  entityCopyText,
  entityMenu,
  type EntityMenuEntry,
} from "./entities";
import {
  NOBODY,
  canLinkHandles,
  chipCell,
  chipLook,
  chipMenu,
  copyText as copyHandleText,
  handleValue,
  people,
  type ChipMenuEntry,
} from "./contacts";
import {
  SEARCH,
  type AccountsMap,
  type ColumnSpec,
  type QmdDocState,
  type RowsResponse,
  type RowsSpec,
  type SearchRow,
} from "@/api";
import { useApi } from "@/cards/cardApi";
import { slugify } from "@/config/sourceSteps";
import { copyToClipboard } from "@/clipboard";
import FeedbackModal from "@/components/FeedbackModal.vue";
import { buildContext, type FeedbackContext } from "@/feedback/context";
import { filePathFromUrl, isDesktopApp, revealActionLabel, revealInFileManager } from "@/desktop";
import { openExternal } from "@/externalLinks";
import { oneAtATime, subscribeLive } from "@/live";
import { encodeColumns } from "@/router/columns";
import { KEEP_COLUMN_WIDTHS } from "@/grid/columnLayout";
import { followFrame, isDarkTheme } from "@/grid/gridFrame";
import {
  filterToken,
  keepExcludeEntries,
  searchDelay,
  withToken,
  type FilterEntry,
} from "@/grid/query";
import { onAfterMenuShowFit, perOpening } from "@/grid/menu";
import { newlyPicked } from "@/grid/selection";
import { copySelectedRowsOnKey } from "@/grid/copyRows";
import { markdownsToAsk, widen } from "@/grid/qmdAsk";
import { searchCoverage, type SearchCoverage } from "@/grid/searchCoverage";
import { keepActiveOnRecord } from "@/grid/activeCell";
import { redrawChanged } from "@/grid/redrawChanged";
import { handedOf, isEmpty, patchRows, type Handed, type RowPatch } from "@/grid/rowPatch";
import {
  asking,
  firstWindow,
  MARGIN,
  nextFetch,
  PAGE,
  refreshLimit,
  withoutPage,
  withPage,
  type Page,
  type PagedWindow,
} from "@/grid/pagedWindow";
import {
  countsByKey,
  groupItems,
  groupKey,
  MORE,
  unread,
  type Getter,
  type GroupWindow,
  type ServerGroup,
} from "@/grid/serverGroups";
import { searchFailure, type SearchFailure } from "./searchFailure";
import { DEFAULT_QUERY, PLAIN_HINT, searchPlaceholder } from "./searchDefaults";
import { pushToast } from "@/toasts";
import type { CardCtx } from "./types";

const { fetchAccounts, fetchGroups, fetchQmdState, fetchRows } = useApi();

/// A row of whichever table the card pages: a search row's fields where
/// it is one, and nothing is assumed of any of them.
type Row = Partial<SearchRow> & Record<string, unknown>;

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
  // The table the card pages (`gridView({url: "/applet/unified_index/problems"})`);
  // the search when absent. Its groups are at `${url}/groups`.
  url?: string;
  // What the empty search bar suggests typing.
  placeholder?: string;
}>();

const url = props.url ?? SEARCH;

const initialState = new URLSearchParams(props.ctx.initialState);

const query = ref(initialState.get("q") ?? props.q ?? "");

// An unnamed card's name tracks the live query, not just the factory
// argument — searching from inside the card renames it.
watch(
  query,
  (q) => props.ctx.setTitle(props.name ?? (q && q !== DEFAULT_QUERY ? `Search: ${q}` : "Search")),
  { immediate: true },
);
const rows = shallowRef<Row[]>([]);
/// The columns the applet declares for its rows — see `ColumnSpec`.
const columns = ref<ColumnSpec[]>([]);
/// How the rows are named, what they open, and what free text matches —
/// see `RowsSpec`. Set by the first answer, before the grid is built.
let rowsSpec: RowsSpec | null = null;
/// qmd ranks this table's free text, and indexes its documents.
const qmd = () => rowsSpec?.free_text === "qmd";
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
  () =>
    (error.value !== null || unfinished.value !== null) &&
    rows.value.length > 0 &&
    shownQuery.value !== query.value,
);
// Why the search cannot read what is typed — often a filter not
// finished yet (`is:do`). The rows of the last query it could read stay.
const unfinished = ref<string | null>(null);
// A free-text search failed in qmd, and came back with no rows.
const qmdError = ref<string | null>(null);
// Free text asked of a root no sync has built a qmd index for yet.
const qmdIndexMissing = ref(false);
const accounts = ref<AccountsMap>({});

// --- qmd index state (the Indexed / Embedded columns) ---------------
// Answers for the documents behind the rows on screen, gathered as the
// grid scrolls, and started over when the result set changes.
const qmdState = ref<Map<string, QmdDocState>>(new Map());
// How much of the corpus each qmd index reaches, shown next to the row
// count.
const qmdCoverage = ref<SearchCoverage | null>(null);
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
// columns hidden this still asks, with no documents, for the "N documents
// searchable" line under the grid, which is the only thing on screen that
// hints the columns exist.
function refreshQmdState() {
  if (!qmd()) return;
  qmdGeneration++;
  qmdAsked.clear();
  for (const ctrl of qmdInflight) ctrl.abort();
  qmdInflight.clear();
  qmdState.value = new Map();
  refreshIndexCells();
  if (qmdColumnsVisible()) askAboutVisibleRows();
  else void askQmdState([]);
}

// Who the Author chips are comes from `people`, the one resolver every
// document and grid asks (docs/dev/plans/chips.md § "One resolver"): a
// cell that draws a handle asks as it draws, and when an answer changes —
// it lands, or a link made anywhere forgets it — the cells are drawn again.
// The Source cells are group chips, answered by `entities` the same way.
const AUTHOR_COLUMN = "author_ref";
const SOURCE_COLUMN = "source_ref";
const stopPeople = people.subscribe(() => refreshCells(AUTHOR_COLUMN));
const stopEntities = entities.subscribe(() => refreshCells(SOURCE_COLUMN));

function refreshCells(columnId: string) {
  const grid = vueGrid?.slickGrid;
  const column = grid?.getColumnIndex(columnId);
  if (!grid || column == null || column < 0) return;
  const { top, bottom } = grid.getRenderedRange();
  for (let row = top; row <= bottom; row++) grid.updateCell(row, column);
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
    qmdCoverage.value = searchCoverage(r);
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
  loadWanted();
  if (qmdScrollTimer) clearTimeout(qmdScrollTimer);
  qmdScrollTimer = setTimeout(() => {
    qmdScrollTimer = null;
    askAboutVisibleRows();
  }, 150);
}

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
function qmdDocState(row: Row | null | undefined): QmdDocState | undefined {
  if (!row?.markdown_uuid) return undefined;
  return qmdState.value.get(row.markdown_uuid);
}

function qmdFlagTooltip(row: Row | null | undefined, which: "indexed" | "embedded"): string {
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

function flagFormatter(which: "indexed" | "embedded"): Formatter<Row> {
  return (_r, _c, _v, _col, row) => {
    const flag = indexFlag(qmdDocState(row)?.[which]);
    const span = document.createElement("span");
    span.className = "qmd-flag";
    span.dataset.flag = flag;
    span.textContent = flag === "yes" ? "✅" : flag === "no" ? "❌" : "—";
    return { html: span, toolTip: qmdFlagTooltip(row, which) };
  };
}
const selectedRow = ref<Row | null>(null);
// Selected row uuid as persisted state — survives reloads so the
// deep-linked column highlights the same row.
const sel = ref<string | null>(initialState.get("sel"));

// The grid, once created: the SlickGrid and DataView objects and the
// services around them. `dataView` and `slickGrid` are optional on the
// bundle's type only because it can be asked for them before `init`;
// here it is never handed out before both exist.
type Grid = SlickVanillaGridBundle<Row> & {
  dataView: NonNullable<SlickVanillaGridBundle<Row>["dataView"]>;
  slickGrid: NonNullable<SlickVanillaGridBundle<Row>["slickGrid"]>;
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
  // Against the card source's query, not against empty: a cleared
  // search has to be saved, or a reload brings the default back.
  if (query.value !== (props.q ?? "")) params.set("q", query.value);
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

/// Show a sort on the headers. The rows are already in its order: the
/// server sorted them, and every column's comparer keeps what it sent.
function applySort(sorters: CurrentSorter[]) {
  if (!vueGrid) return;
  restoring = true;
  vueGrid.sortService.updateSorting(sorters, false, false);
  restoring = false;
}

/// Leaves rows where the server put them, so a header click only says
/// which order to ask for.
const serverOrder = () => 0;

/// The sort the rows are asked for in, as `/search` spells it: the
/// headers' once the grid exists, the persisted layout's before.
function currentSort(): string | null {
  const layout = colsEncoded ? decodeLayout(colsEncoded) : null;
  const sorters: CurrentSorter[] = vueGrid
    ? vueGrid.sortService.getCurrentLocalSorters()
    : (layout?.sort ?? []).map((s) => ({ columnId: s.id, direction: s.asc ? "ASC" : "DESC" }));
  if (sorters.length === 0) return null;
  return sorters.map((s) => `${s.columnId}:${String(s.direction).toLowerCase()}`).join(",");
}

function rowKey(row: Row): string {
  return String(row[rowsSpec?.row_key ?? "uuid"] ?? "");
}

function rowKeyOf(row: Row | undefined): string | null {
  return row ? rowKey(row) : null;
}

/// The document a row opens, and the section in it, or null for a row
/// with none.
function documentOf(row: Row): { md: string; anchor: string | null } | null {
  const link = rowsSpec?.document;
  if (!link) return null;
  const md = link.fields.map((f) => row[f]).find((v) => typeof v === "string" && v !== "");
  if (typeof md !== "string") return null;
  const anchor = row[link.anchor];
  return { md, anchor: typeof anchor === "string" && anchor !== "" ? anchor : null };
}

/// The record at a grid row, or null where the row is a group header
/// or its totals — the data view hands those out as items too.
function rowData(row: number): Row | null {
  const item = vueGrid?.dataView.getItem(row) as
    (Row & { __group?: boolean; __groupTotals?: boolean; [MORE]?: string }) | undefined;
  if (!item || item.__group || item.__groupTotals || item[MORE]) return null;
  return item;
}

/// The rows the grid has selected, in grid order.
function selectedRows(): Row[] {
  if (!vueGrid) return [];
  return vueGrid.slickGrid
    .getSelectedRows()
    .map(rowData)
    .filter((r): r is Row => r != null);
}

// Lightroom-style: if multiple rows are selected and the right-click anchor
// is part of that selection, the action targets all selected rows;
// otherwise it targets only the anchor row. The selection itself is left
// alone either way — a right-click aims the action, it does not re-select.
function resolveTargetRows(anchor: Row | null | undefined): Row[] {
  if (!anchor) return [];
  const selected = selectedRows();
  if (selected.length > 1 && selected.some((r) => rowKey(r) === rowKey(anchor))) return selected;
  return [anchor];
}

// Feedback modal state
const feedbackOpen = ref(false);
const feedbackContext = ref<FeedbackContext | null>(null);
const feedbackSurfaceLabel = ref("");

// Filter context for a right-clicked cell: the term its value makes in
// the search bar. Null for a column the search has no key for (Score,
// Contents) or a row with no value in it.
type FilterCtx = {
  // The search bar's key for the column (`author`, `source_id`).
  key: string;
  // Human-facing column header for menu labels.
  header: string;
  // The value a term names: what the column's key compares, which for
  // an id or a uuid behind a label is the id or uuid, not the cell's text.
  value: string;
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

/// What a cell copies as when its row is copied: by its declared type,
/// and the card's own two columns as the flag's word.
function copyCell(column: Column<Row>, row: Row): string {
  if (column.id === "qmd_indexed") return indexFlag(qmdDocState(row)?.indexed);
  if (column.id === "qmd_embedded") return indexFlag(qmdDocState(row)?.embedded);
  const spec = columns.value.find((c) => c.field === column.id);
  if (spec?.type === "identity") {
    const v = row[spec.field] as Identity | null | undefined;
    if (v?.entity) return entityCopyText(v.entity, v.label);
    const handle = v ? handleFromUri(v.id) : null;
    if (handle && v) return copyHandleText(handle, v.label);
  }
  return spec ? copyText(spec.type, row[spec.field]) : "";
}

/// Put one id per target on the clipboard, comma-separated.
async function copyIds(targets: Row[], pick: (r: Row) => string) {
  const text = targets
    .map(pick)
    .filter((v) => v.length > 0)
    .join(",");
  if (text.length === 0) return;
  await copyToClipboard(text);
}

// Build a FilterCtx for the cell at `colId` on the given row, from the
// search key the applet declares for the column.
function buildFilterCtx(colId: string, data: Row): FilterCtx | null {
  const spec = columns.value.find((c) => c.field === colId);
  if (!spec?.search) return null;
  const row = data as Record<string, unknown>;
  const raw = row[spec.search.field];
  if (raw == null || raw === "") return null;
  const value = String(raw);
  // A uuid rides with the name a person reads, as `slug-uuid`: an
  // account's from the accounts map, a conversation's from its own cell.
  const shown = row[colId];
  const label =
    colId === "author_ref" || colId === "account"
      ? (accounts.value[value]?.label ?? "")
      : spec.search.field !== colId && typeof shown === "string"
        ? shown
        : "";
  return { key: spec.search.key, header: spec.header, value: formatSlugUuid(label, value) };
}

function accountLabel(uuid: string): string {
  if (!uuid) return "";
  return accounts.value[uuid]?.label ?? uuid;
}

let inflight: AbortController | null = null;
let debounceTimer: ReturnType<typeof setTimeout> | null = null;

/// The rows the grid holds, a prefix of the search's.
let win: PagedWindow<Row, number> | null = null;
/// The search they answer. Replaced, never mutated, so a page that comes
/// back for a search since replaced can tell. `tail` is the default
/// order, newest first, which the grid shows the other way up: newest at
/// the bottom, where it opens, with older rows loading above. Rows a
/// sync adds then land below the ones on screen instead of moving them.
let shown: { q: string; sort: string | null; tail: boolean } | null = null;
/// A selection restored from the URL may be further down the list than
/// the first page; the grid loads through it. Typing a new search gives
/// up on it.
let seekingSelection = sel.value !== null;

/// While the grid is grouped, the search as the server groups it: every
/// group with its true count, and each group's rows a window of their own,
/// read as the group is opened and scrolled (grid/serverGroups.ts). Null
/// while nothing is grouped.
let grouped: {
  q: string;
  sort: string | null;
  by: string[];
  groups: ServerGroup<Row>[];
  windows: Map<string, GroupWindow<Row>>;
  counts: Map<string, number>;
} | null = null;

/// The columns dragged into the grouping bar, outermost first.
function groupedBy(): string[] {
  return groupingPlugin?.columnsGroupBy.map((c) => String(c.id)) ?? [];
}

/// A group's rows as `/search` narrows to them.
function withinOf(by: string[], values: (string | null)[]): string {
  return JSON.stringify(by.map((id, i) => [id, values[i]]));
}

/// How each grouped column reads its group's value off a row.
function gettersOf(by: string[]): Getter<Row>[] {
  return by.map(
    (id) => gridColumns.value.find((c) => c.id === id)!.grouping!.getter as Getter<Row>,
  );
}

/// The rows in the order the grid shows them.
function display(rows: Row[]): Row[] {
  return shown?.tail ? [...rows].reverse() : rows;
}

/// An answer as a page of the rows, read on by offset.
function searchPage(r: RowsResponse<Row>): Page<Row, number> {
  return { rows: r.rows, next: r.next_offset, total: r.total, at: r.at };
}

/// Ask for the first page of `q`, or, for a refresh of the search on
/// screen, for every row through the last one held, so nobody scrolled
/// along the list loses their place.
async function runSearch(q: string, refresh = false) {
  if (groupedBy().length > 0) return runGrouped(q, refresh);
  grouped = null;
  inflight?.abort();
  const ctrl = (inflight = new AbortController());
  const sort = currentSort();
  const again = refresh && win !== null && shown?.q === q && shown.sort === sort;
  const limit = again ? refreshLimit(win!) : PAGE;
  const through = again
    ? rowKeyOf(win!.rows[win!.rows.length - 1])
    : seekingSelection
      ? sel.value
      : null;
  loading.value = true;
  error.value = null;
  qmdError.value = null;
  qmdIndexMissing.value = false;
  try {
    // The card shows a failure itself, beside the rows it concerns.
    const r = await fetchRows<Row>(url, q, limit, ctrl.signal, { toast: false }, { sort, through });
    unfinished.value = r.refused?.[0] ?? null;
    if (unfinished.value !== null) return;
    rowsSpec = { row_key: r.row_key, document: r.document, free_text: r.free_text };
    if (r.columns?.length && JSON.stringify(r.columns) !== JSON.stringify(columns.value)) {
      columns.value = r.columns;
    }
    win = firstWindow(searchPage(r));
    // qmd's rank is not an order in time.
    const ranked = qmd() && !!r.query_echo?.free_text;
    if (!again) shown = { q, sort, tail: sort === null && !ranked };
    rows.value = win.rows;
    total.value = r.total;
    qmdError.value = typeof r.query_echo?.qmd_error === "string" ? r.query_echo.qmd_error : null;
    qmdIndexMissing.value = r.query_echo?.qmd_index_missing === true;
    shownQuery.value = q;
    if (again) showChanged(true);
    else showNew();
  } catch (e) {
    if ((e as { name?: string }).name === "AbortError") return;
    error.value = searchFailure(e);
  } finally {
    if (inflight === ctrl) loading.value = false;
  }
}

/// The page load on its way, for anything that has to wait for it.
let loadingMore: Promise<void> | null = null;

/// Load rows until index `through` of the search is held, and the row
/// `uuid` names if given, or until the search runs out.
async function loadThrough(through: number, uuid: string | null = null) {
  if (!win || !shown) return;
  const fetch = nextFetch(win, through);
  if (!fetch) return;
  const search = shown;
  win = asking(win, fetch);
  const load = (async () => {
    try {
      const r = await fetchRows<Row>(
        url,
        search.q,
        fetch.limit,
        undefined,
        { toast: false },
        { offset: fetch.from, sort: search.sort, through: uuid },
      );
      if (search !== shown || !win) return;
      const next = withPage(win, fetch.from, searchPage(r));
      if (next === "moved") {
        void runSearch(search.q, true);
        return;
      }
      win = next;
      rows.value = win.rows;
      total.value = r.total;
      showChanged(false);
    } catch (e) {
      if (search !== shown || !win) return;
      win = withoutPage(win, fetch.from);
      error.value = searchFailure(e);
    }
  })();
  loadingMore = load;
  await load;
  if (loadingMore === load) loadingMore = null;
}

/// Load whatever the grid needs now: the rows of the groups on screen,
/// else the rows the viewport is nearing, and a restored selection that
/// has not turned up.
function loadWanted() {
  if (grouped) {
    loadGroupsInView();
    return;
  }
  if (!vueGrid || !win) return;
  const { top, bottom } = vueGrid.slickGrid.getViewport();
  // Shown the other way up, the search's far end is the grid's top.
  const nearing = shown?.tail ? win.rows.length - 1 - top : bottom;
  const seeking = seekingSelection ? sel.value : null;
  void loadThrough(seeking ? win.rows.length : nearing + MARGIN, seeking);
}

/// Load pages until `uuid`'s row is held, and say where it is: null when
/// the search does not have it.
async function seek(uuid: string): Promise<number | null> {
  while (vueGrid && win && vueGrid.dataView.getRowById(uuid) == null) {
    if (loadingMore) await loadingMore;
    else if (win.next === null) break;
    else await loadThrough(win.rows.length, uuid);
  }
  return vueGrid?.dataView.getRowById(uuid) ?? null;
}

/// Ask for the groups of `q`. A refresh of the grouping on screen reads
/// each group opened so far again, through the last row it holds.
async function runGrouped(q: string, refresh: boolean) {
  inflight?.abort();
  const ctrl = (inflight = new AbortController());
  const sort = currentSort();
  const by = groupedBy();
  const was = grouped;
  const again =
    refresh && was !== null && was.q === q && was.sort === sort && was.by.join() === by.join();
  loading.value = true;
  error.value = null;
  qmdError.value = null;
  qmdIndexMissing.value = false;
  try {
    const r = await fetchGroups<Row>(q, by.join(","), ctrl.signal, url);
    unfinished.value = r.refused?.[0] ?? null;
    if (unfinished.value !== null) return;
    const windows = new Map(r.groups.map((g) => [groupKey(g.values), unread(g, r.at)]));
    if (again) {
      await Promise.all(
        r.groups.map(async (g) => {
          const held = was.windows.get(groupKey(g.values));
          const last = held?.rows[held.rows.length - 1];
          if (!last) return;
          const page = await fetchRows<Row>(
            url,
            q,
            refreshLimit(held!),
            ctrl.signal,
            { toast: false },
            { sort, within: withinOf(by, g.values), through: rowKey(last) },
          );
          windows.set(groupKey(g.values), firstWindow(searchPage(page)));
        }),
      );
    }
    grouped = {
      q,
      sort,
      by,
      groups: r.groups,
      windows,
      counts: countsByKey(r.groups, gettersOf(by)),
    };
    shown = { q, sort, tail: false };
    win = null;
    qmdError.value = r.qmd_error ?? null;
    qmdIndexMissing.value = r.qmd_index_missing ?? false;
    shownQuery.value = q;
    showGroups(again ? "refresh" : "new");
  } catch (e) {
    if ((e as { name?: string }).name === "AbortError") return;
    error.value = searchFailure(e);
  } finally {
    if (inflight === ctrl) loading.value = false;
  }
}

/// Read the next page of the group `key` names.
async function loadGroupPage(key: string) {
  const g = grouped;
  if (!g) return;
  const held = g.windows.get(key)!;
  const fetch = nextFetch(held, held.rows.length);
  if (!fetch) return;
  g.windows.set(key, asking(held, fetch));
  const values = JSON.parse(key) as (string | null)[];
  try {
    const r = await fetchRows<Row>(
      url,
      g.q,
      fetch.limit,
      undefined,
      { toast: false },
      { offset: fetch.from, sort: g.sort, within: withinOf(g.by, values) },
    );
    if (grouped !== g) return;
    const next = withPage(g.windows.get(key)!, fetch.from, searchPage(r));
    if (next === "moved") {
      void runSearch(g.q, true);
      return;
    }
    g.windows.set(key, next);
    showGroups("more");
  } catch (e) {
    if (grouped !== g) return;
    g.windows.set(key, withoutPage(g.windows.get(key)!, fetch.from));
    error.value = searchFailure(e);
  }
}

/// Read a page of every group whose placeholder is on screen, or nearly:
/// an open group shows one until its rows are all read.
function loadGroupsInView() {
  if (!vueGrid) return;
  const { dataView, slickGrid: grid } = vueGrid;
  const { top, bottom } = widen(grid.getRenderedRange(), grid.getDataLength());
  for (let row = top; row <= bottom; row++) {
    const key = (dataView.getItem(row) as Record<string, unknown> | undefined)?.[MORE];
    if (typeof key === "string") void loadGroupPage(key);
  }
}

/// The groups on screen: a new grouping replaces the grid's rows; more
/// rows of a group, or the grouping read again, touch only what changed.
function showGroups(kind: "new" | "refresh" | "more") {
  if (!vueGrid || !grouped) return;
  const items = groupItems(grouped.groups, grouped.windows, rowsSpec?.row_key ?? "uuid");
  rows.value = items.filter((r) => !(MORE in r));
  total.value = grouped.groups.reduce((n, g) => n + g.count, 0);
  if (kind === "new") {
    handed = handedOf(items, rowKey);
    vueGrid.dataset = items;
    vueGrid.slickGrid.scrollRowIntoView(0);
    refreshQmdState();
  } else {
    const next = patchRows(handed, items, rowKey);
    handed = next.handed;
    applyPatch(next.patch, items);
    if (kind === "refresh") {
      // A group's count can change with none of its rows on screen.
      redrawGroupRows();
      refreshQmdState();
    } else askAboutVisibleRows();
  }
  tryRestoreSelection();
  loadGroupsInView();
}

/// Draw the group rows on screen again, for their counts.
function redrawGroupRows() {
  const grid = vueGrid!.slickGrid;
  const { top, bottom } = grid.getRenderedRange();
  for (let row = top; row <= bottom; row++) {
    if ((vueGrid!.dataView.getItem(row) as { __group?: boolean } | undefined)?.__group) {
      grid.invalidateRow(row);
    }
  }
  grid.render();
}

watch(query, (q, before) => {
  if (debounceTimer) clearTimeout(debounceTimer);
  seekingSelection = false;
  // Show the spinner immediately on input change — otherwise the
  // debounce leaves the user staring at the old rows with no feedback.
  loading.value = true;
  debounceTimer = setTimeout(() => runSearch(q), searchDelay(before, q, qmd()));
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
  if (!target) {
    if (win?.next === null) seekingSelection = false;
    return;
  }
  seekingSelection = false;
  const row = vueGrid.dataView.getRowById(target_sel);
  if (row == null) return;
  restoring = true;
  vueGrid.slickGrid.setSelectedRows([row]);
  vueGrid.slickGrid.scrollRowIntoView(row);
  selectedRow.value = target;
  restoring = false;
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
  touched_at: "touched_at",
  author_ref: "author",
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

/// The rows last handed to the grid.
let handed: Handed = new Map();

/// A new search: its rows replace the grid's and shape the columns
/// around them.
function showNew() {
  if (!vueGrid || !win) return;
  handed = handedOf(win.rows, rowKey);
  const shownRows = display(win.rows);
  vueGrid.dataset = shownRows;
  applyAdaptiveVisibility();
  // Newest at the bottom for the default order, the top row otherwise.
  if (shownRows.length > 0)
    vueGrid.slickGrid.scrollRowIntoView(shown?.tail ? shownRows.length - 1 : 0);
  tryRestoreSelection();
  // Fire-and-forget: the badges fill in a beat after the rows land
  // rather than holding the result set hostage to a second request.
  refreshQmdState();
  loadWanted();
}

/// More rows of the search on screen, or the same search read again
/// after the index moved: only the rows that changed are touched, and
/// the rest is left to the person.
function showChanged(indexMoved: boolean) {
  if (!vueGrid || !win) return;
  const next = patchRows(handed, win.rows, rowKey);
  handed = next.handed;
  applyPatch(next.patch, display(win.rows));
  tryRestoreSelection();
  if (indexMoved) refreshQmdState();
  else askAboutVisibleRows();
  loadWanted();
}

/// Apply a patch in place, leaving the rows in `order`, the server's.
/// Rows inserted above the viewport would push what the person is
/// reading down, so the row at the top is held at the top.
function applyPatch(patch: RowPatch<Row>, order: Row[]) {
  if (!vueGrid || isEmpty(patch)) return;
  const { dataView, slickGrid: grid } = vueGrid;
  const top = grid.getViewport().top;
  const anchorRow = rowData(top);
  const anchor = anchorRow ? rowKey(anchorRow) : null;
  const position = new Map(order.map((r, i) => [rowKey(r), i]));
  keepActiveOnRecord(grid, dataView, () => {
    redrawChanged(grid, dataView, () => {
      dataView.beginUpdate();
      for (const id of patch.removed) dataView.deleteItem(id);
      for (const row of patch.changed) dataView.updateItem(rowKey(row), row);
      for (const row of patch.added) dataView.addItem(row);
      dataView.sort((a, b) => position.get(rowKey(a))! - position.get(rowKey(b))!);
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

const hint = ref(props.placeholder ?? PLAIN_HINT);
const namesASource = () => hint.value !== PLAIN_HINT;

/// A search card with no hint of its own suggests filters on this
/// library's sources, once the index holds any.
async function nameSourcesInPlaceholder() {
  if (props.placeholder != null || url !== SEARCH) return;
  try {
    hint.value = searchPlaceholder((await fetchGroups<Row>("", "source_ref")).groups);
  } catch {
    /* only a hint */
  }
}
onMounted(nameSourcesInPlaceholder);

// The index moved under us — a `grid_index` pass committed, which under
// streaming happens many times per sync, as each source's rows arrive.
// Ask the shown search again; the rows update in place while the
// download is still going.
const cardEl = ref<HTMLElement | null>(null);
let unsubscribeLive: (() => void) | null = null;
const refreshRows = oneAtATime(() => runSearch(query.value, true));
onMounted(() => {
  unsubscribeLive = subscribeLive(
    {
      root: (e) => {
        if (e.kind !== "index_changed") return;
        refreshRows();
        if (!namesASource()) void nameSourcesInPlaceholder();
      },
    },
    { onScreen: cardEl.value ?? undefined },
  );
});
onBeforeUnmount(() => unsubscribeLive?.());
onBeforeUnmount(stopPeople);
onBeforeUnmount(stopEntities);

function docSource(md: string, anchor: string | null): string {
  const args = [md, anchor].map((a) => JSON.stringify(a)).join(", ");
  return `documentView(${args})`;
}

function openRow(row: Row) {
  // Double-click → open this row's doc as a standalone single-column
  // page in a new tab, with the row's section highlighted.
  const doc = documentOf(row);
  if (!doc) return;
  const href = encodeColumns([{ code: docSource(doc.md, doc.anchor), state: "" }]);
  window.open(href, "_blank", "noopener");
}

/// A cell's text with the account name where the value is an account's
/// uuid.
const accountFormatter: Formatter<Row> = (_r, _c, value) => {
  const v = typeof value === "string" ? value : "";
  const label = accountLabel(v);
  return { text: label, toolTip: v && label !== v ? v : "" };
};

/// The Author cell: a chip where the author has a handle, drawn from
/// what `people` has answered so far; otherwise the name as shown, with
/// an account's uuid read as the account's name.
const authorFormatter: Formatter<Row> = (_r, _c, _v, _col, row) => {
  const ref = row.author_ref;
  const handle = ref ? handleFromUri(ref.id) : null;
  if (handle && ref) return chipCell(handle, ref.label, people.lookup(handle), canLinkHandles());
  const v = row.author ?? "";
  const label = accountLabel(v);
  return { text: label, toolTip: v && label !== v ? v : "" };
};

/// What this card adds to the applet's declared columns: the widths and
/// hovers a type cannot know, the account-name formatting on the
/// author/account cells (the accounts map is the browser's), and the
/// two-line clamp on the text.
const columnOverrides: Record<string, Partial<Column<Row>>> = {
  source_ref: { width: 150 },
  kind: { width: 110 },
  conversation_name: { width: 200 },
  channel: { width: 130 },
  snippet: {
    // Room for the default columns beside it in a 1440px window.
    width: 480,
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
  author_ref: {
    width: 130,
    formatter: authorFormatter,
    grouping: {
      getter: (row: Row) => accountLabel(row.author ?? ""),
      formatter: groupTitle("Author"),
      collapsed: false,
    },
  },
  account: {
    formatter: accountFormatter,
    grouping: {
      getter: (row: Row) => accountLabel(row.account ?? ""),
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
/// screen. Untyped by row: the grid's field type has no room for a row
/// type that names its fields only by string.
const extraColumns: Column[] = [
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
  }
  if (layout.group?.length && groupingPlugin) {
    restoring = true;
    groupingPlugin.setDroppedGroups(layout.group);
    restoring = false;
  }
}

/// A group row's title with the server's count for the group, while the
/// server groups: the rows held are only the ones read so far.
function trueCount(inner: (g: GroupingFormatterItem) => string) {
  return (g: GroupingFormatterItem) =>
    inner(grouped ? { ...g, count: grouped.counts.get(g.groupingKey)! } : g);
}

/// The grid's columns: the applet's, drawn by type, refined by the
/// overrides above, with the card's own beside `project` — among the
/// facets, where a 640px card still has them on screen. Built once per
/// declaration, since handing the grid new definitions resets its
/// layout.
const gridColumns = shallowRef<Column[]>([]);
watch(
  columns,
  (specs) => {
    // Nothing declared yet: the card's own two columns alone are not a
    // grid worth building.
    if (specs.length === 0) return;
    const typed = typedColumns<Row>(specs, {
      overrides: columnOverrides,
      groupable: true,
      chips: {
        who: (h) => people.lookup(h),
        canLink: canLinkHandles,
        entity: (uri) => entities.lookup(uri),
      },
    });
    const at = typed.findIndex((c) => c.id === "project") + 1;
    const own = qmd() ? extraColumns : [];
    gridColumns.value = [...typed.slice(0, at), ...own, ...typed.slice(at)].map((c) => ({
      ...c,
      sortComparer: serverOrder,
      ...(c.grouping
        ? { grouping: { ...c.grouping, formatter: trueCount(c.grouping.formatter!) } }
        : {}),
    }));
    createGrid();
  },
  { immediate: true },
);

/// A menu entry drawn like the others (text only, see grid/menu.ts),
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
      // The menu item lays its text out itself; the wrapper
      // only exists because a renderer returns one element.
      wrap.style.display = "contents";
      const text = document.createElement("span");
      text.className = "slick-menu-content";
      text.textContent = label(scopeOf(args)) ?? "";
      wrap.append(text);
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
  anchor: Row | null;
  /// The cell under the click: its column, its painted text (closer to
  /// what the user saw — an author's name, not their uuid — than the
  /// row's field), and its element, for the feedback breadcrumb.
  cell: { column: string; cellValue: string; el: HTMLElement | null } | null;
  /// The cell under the click as its row's copy has it; null when empty.
  copy: { header: string; text: string } | null;
  targets: Row[];
  filter: FilterEntry[];
  notion: FilterEntry[];
  links: { web: Row[]; local: string[] };
  /// The chip in the cell under the click, when the cell is an Author
  /// with a handle: the same entries a document's chip offers.
  chip: { handle: string; name: string; entries: ChipMenuEntry[] } | null;
  /// The group or step chip in the cell under the click: its entries.
  entity: { uri: string; name: string; entries: EntityMenuEntry[] } | null;
};

const linkOf = (r: Row): string => r.source_url || "";

function menuScope(args: MenuFromCellCallbackArgs): MenuScope {
  const anchor = args.row != null ? rowData(args.row) : null;
  // `onBeforeMenuShow`, where the scope is worked out, is handed the
  // cell's coordinates and nothing else; the item callbacks get the
  // column as well.
  const column = (args.column ?? args.grid.getColumns()[args.cell ?? -1]) as Column | undefined;
  const colId = String(column?.id ?? "");
  const el =
    args.row != null && args.cell != null
      ? (args.grid.getCellNode(args.row, args.cell) ?? null)
      : null;
  const cell = colId ? { column: colId, cellValue: el?.textContent?.trim() ?? "", el } : null;
  const copied = anchor && column ? copyCell(column, anchor) : "";
  const copy = copied ? { header: String(column!.name ?? colId), text: copied } : null;
  const targets = resolveTargetRows(anchor);
  const filterCtx = anchor ? buildFilterCtx(colId, anchor) : null;
  // Optional "Filter by Notion Page" entries, populated when the right-
  // clicked row has a non-empty `notion_page_uuid`. Lets users zoom into
  // all rows on a single Notion page from any cell of any row on that
  // page — useful because the page UUID isn't always the same as
  // conversation_uuid (e.g. comment threads use the discussion UUID).
  const page = anchor?.notion_page_uuid;
  const notionCtx: FilterCtx | null = page
    ? {
        key: "notion_page",
        header: "Notion Page",
        value: formatSlugUuid(
          anchor?.conversation_uuid === page ? (anchor?.conversation_name ?? "") : "",
          page,
        ),
      }
    : null;
  // A row's outbound linkout is source_url (Slack permalink, LinkedIn
  // post, …). Local files (today: the `pdf` source) carry a `file://` URL, which
  // `window.open` cannot usefully follow from an http origin — a
  // browser blocks it silently. Split on the URL SCHEME rather than on
  // provider, so any future local-file source inherits this.
  const linked = targets.filter((r) => linkOf(r));
  const chipEl = el?.querySelector<HTMLElement>("a.chip[data-handle]") ?? null;
  const chip = chipEl
    ? (() => {
        const handle = chipEl.dataset.handle ?? "";
        const shownAs = chipEl.dataset.shownAs ?? "";
        const w = people.get(handle) ?? NOBODY;
        // No popover in a grid cell yet, so the link entry is not offered
        // here; the document view has it.
        return {
          handle,
          name: chipLook(handle, shownAs, w, false).text,
          entries: chipMenu(handle, shownAs, w, false),
        };
      })()
    : null;
  const entityEl = el?.querySelector<HTMLElement>("a.chip[data-entity]") ?? null;
  const entity = entityEl
    ? (() => {
        const uri = entityEl.dataset.entity ?? "";
        const name = entityEl.dataset.label ?? entityEl.dataset.shownAs ?? uri;
        return { uri, name, entries: entityMenu(uri, name) };
      })()
    : null;
  return {
    anchor,
    cell,
    copy,
    targets,
    chip,
    entity,
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
  const rowUuids = m.targets.map(rowKey);
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

// The right-click menu, ahead of the grid's own entries (the grouping
// commands). Each entry decides for itself whether the
// cell under the click gives it anything to do.
const entityEntry = (id: EntityMenuEntry["id"], run: (m: MenuScope) => void) =>
  entry(`entity-${id}`, (m) => m.entity?.entries.find((e) => e.id === id)?.label ?? null, run);

/// A group's dashboard or a step's log, beside this card.
function openEntity(uri: string) {
  const source = entityCardSource(uri);
  if (source) props.ctx.host.openCards(source);
}

const chipEntry = (id: ChipMenuEntry["id"], run: (m: MenuScope) => void) =>
  entry(`chip-${id}`, (m) => m.chip?.entries.find((e) => e.id === id)?.label ?? null, run);

const menuItems: (MenuCommandItem | "divider")[] = [
  chipEntry("copy-name", (m) => void copyToClipboard(m.chip!.name)),
  chipEntry("copy-id", (m) => void copyToClipboard(handleValue(m.chip!.handle))),
  chipEntry("copy-both", (m) => void copyToClipboard(copyHandleText(m.chip!.handle, m.chip!.name))),
  chipEntry("search", (m) =>
    appendFilterToQuery(filterToken("author_handle", m.chip!.handle, false)),
  ),
  dividerAfter((m) => m.chip !== null),
  entityEntry("copy-name", (m) => void copyToClipboard(m.entity!.name)),
  entityEntry(
    "copy-id",
    (m) => void copyToClipboard(entityFromUri(m.entity!.uri)?.id ?? m.entity!.uri),
  ),
  entityEntry(
    "copy-both",
    (m) => void copyToClipboard(entityCopyText(m.entity!.uri, m.entity!.name)),
  ),
  entityEntry("open", (m) => openEntity(m.entity!.uri)),
  entityEntry("browse", (m) => {
    const q = browseQuery(m.entity!.uri);
    if (q) query.value = q;
  }),
  dividerAfter((m) => m.entity !== null),
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
    "copy-cell",
    (m) => (m.copy ? `Copy ${m.copy.header}` : null),
    (m) => void copyToClipboard(m.copy!.text),
  ),
  entry(
    "copy-uuids",
    (m) => (m.targets.length ? `Copy UUID${plural(m)}` : null),
    (m) => void copyIds(m.targets, rowKey),
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

function gridOptions(): GridOption {
  return {
    datasetIdPropertyName: rowsSpec?.row_key ?? "uuid",
    // Cells are text, never markup: a row's snippet is the source's own.
    enableHtmlRendering: false,
    enableEmptyDataWarningMessage: false,
    darkMode: isDarkTheme(),
    ...KEEP_COLUMN_WIDTHS,
    ...followFrame(boxEl.value!, 200),
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
    // The server sorts: a shift-click adds a column, numbered by how it
    // ranks, and each breaks the ties of the one before.
    enableSorting: true,
    multiColumnSort: true,
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
      onGroupChanged: () => {
        updateCols();
        void runSearch(query.value);
      },
      onExtensionRegistered: (plugin) => {
        groupingPlugin = plugin;
      },
    },
    enableContextMenu: true,
    contextMenu: {
      commandItems: menuItems,
      // Ours copies the cell as its row's copy does; the grid's copies
      // the raw field, an object as "[object Object]".
      hideCopyCellValueCommand: true,
      onBeforeMenuShow: scopes.onBeforeMenuShow,
      onAfterMenuShow: onAfterMenuShowFit,
      // The grid scrolls itself — a page landing above the viewport holds
      // the top row in place, a selection is scrolled back to — and a menu
      // that closed on every scroll closed under the person reading it.
      // Its entries stay with the row it opened on (`perOpening`).
      hideMenuOnScroll: false,
    },
  };
}

/// A diff group's rows say how they differ from the other commit
/// (`diff_status`; null on every real source's rows), and a modified
/// row names the columns that moved (`diff_changed_columns`, the
/// `grid_rows` names, `|`-joined). The row takes a band for the first
/// and the cell a highlight for the second — through the data view's
/// item metadata, which the grid reads for every row it paints. The
/// grouping extension installs its own provider for group rows, so
/// this wraps whatever is there rather than replacing it. A group's
/// placeholder, for its rows not yet read, is one cell across the row.
function changedColumns(row: Row | undefined): Set<string> {
  const names = row?.diff_changed_columns;
  if (!names) return new Set();
  // The body is shown as Contents: its preview, or its hash when the
  // change is past the preview.
  const asShown = (c: string) => (c === "preview" || c === "content_hash" ? "snippet" : c);
  return new Set(names.split("|").map(asShown));
}

const PLACEHOLDER_META = {
  cssClasses: "datalib-more",
  focusable: false,
  selectable: false,
  columns: { 0: { colspan: "*", formatter: () => "loading…" } },
};

function installRowMetadata(dataView: Grid["dataView"]) {
  const inner = dataView.getItemMetadata.bind(dataView);
  dataView.getItemMetadata = (row: number) => {
    const meta = inner(row);
    const item = dataView.getItem(row) as Row | undefined;
    if (item && MORE in item) return PLACEHOLDER_META;
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
  const bundle = new SlickVanillaGridBundle<Row>(
    boxEl.value,
    gridColumns.value,
    options,
    rows.value,
  ) as Grid;
  vueGrid = bundle;
  installRowMetadata(bundle.dataView);
  const grid = bundle.slickGrid;
  grid.onSelectedRowsChanged.subscribe(onSelectedRowsChanged);
  grid.onClick.subscribe(onClick);
  grid.onDblClick.subscribe(onDblClick);
  grid.onViewportChanged.subscribe(onViewportChanged);
  copySelectedRowsOnKey(grid, rowData, copyCell);
  bundle.instances?.eventPubSubService?.subscribe<GridStateChange>(
    "onGridStateChanged",
    onGridStateChanged,
  );
  // Expose the grid so e2e tests can scroll virtualized rows into view
  // before clicking. Last grid card wins when several are open — fine
  // for tests, which drive a single grid.
  (window as unknown as { __fwGridApi?: unknown }).__fwGridApi = {
    rowIndexOf: (uuid: string) => bundle.dataView.getRowById(uuid) ?? null,
    // Load pages until the row is held, as scrolling to it would.
    seek,
    // What a header dropped on the search bar does, without the mouse.
    dropOnSearch,
    // A search, a page, or a group's page on its way.
    busy: () =>
      loading.value ||
      loadingMore !== null ||
      [...(grouped?.windows.values() ?? [])].some((w) => w.pending !== null),
    uuidAt: (row: number) => {
      const item = bundle.dataView.getItem(row) as Row | undefined;
      return item ? rowKey(item) : null;
    },
    rows: () => (bundle.dataView.getItems() as Row[]).filter((r) => !(MORE in r)),
    scrollToRow: (row: number) => grid.scrollRowIntoView(row),
    scrollToColumn: (id: string) => {
      const idx = grid.getColumnIndex(id);
      if (idx != null) grid.scrollColumnIntoView(idx);
    },
    isSelected: (uuid: string) => selectedRows().some((r) => rowKey(r) === uuid),
    activeUuid: () => {
      const active = grid.getActiveCell();
      const row = active ? rowData(active.row) : null;
      return row ? rowKey(row) : null;
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
  // The rows are already loaded by the time the grid exists — the
  // columns arrive with the first results.
  showNew();
  installSearchDrop();
}

/// The search bar takes a column dragged from the headers, the way the
/// grouping bar does, and adds a term keeping the rows with a value in
/// it: `author:*`, which a person can then narrow to a value.
const searchWrapEl = ref<HTMLDivElement | null>(null);
type Sortable = { destroy(): void };
type SortableClass = { create(el: HTMLElement, options: object): Sortable };
let searchDrop: Sortable | null = null;

function dropOnSearch(colId: string) {
  const key = columns.value.find((c) => c.field === colId)?.search?.key;
  if (!key) {
    const name = gridColumns.value.find((c) => c.id === colId)?.name ?? colId;
    pushToast(`The search cannot filter by ${name}.`, "info");
    return;
  }
  appendFilterToQuery(`${key}:*`);
}

function installSearchDrop() {
  const el = searchWrapEl.value;
  const bar = groupingPlugin?.droppableInstance;
  if (!el || !bar || !vueGrid) return;
  // The Sortable the grouping bar is: a header's drag is offered to every
  // list in its `shared` group, and the search bar joins it.
  const Sortable = bar.constructor as unknown as SortableClass;
  const uid = vueGrid.slickGrid.getUID();
  searchDrop = Sortable.create(el, {
    group: "shared",
    // Nothing of its own to drag.
    draggable: ".search-drop-none",
    onAdd: (evt: { item: HTMLElement }) => {
      const id = evt.item.getAttribute("id") ?? "";
      evt.item.remove();
      // Another grid's header, dropped here, is not a column of this one.
      if (id.startsWith(uid)) dropOnSearch(id.slice(uid.length));
    },
  });
}

/// The records selected as of the last change the grid reported.
let selectedIds = new Set<string>();

function onSelectedRowsChanged(_e: SlickEventData, args: OnSelectedRowsChangedEventArgs) {
  if (!vueGrid) return;
  const now = args.rows.map(rowData).filter((d): d is Row => d != null);
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
  const doc = documentOf(data);
  if (doc) props.ctx.host.openCards(docSource(doc.md, doc.anchor));
}

/// The chip under a pointer event in a cell, if any.
function chipAt(e: SlickEventData): HTMLElement | null {
  const target = e.getNativeEvent<MouseEvent>()?.target as Element | null | undefined;
  return target?.closest?.<HTMLElement>("a.chip[data-handle], a.chip[data-entity]") ?? null;
}

function onClick(e: SlickEventData, args: OnClickEventArgs) {
  // A chip is a link; a click on it selects the row and nothing more —
  // the mail client its href would open is not what a click here asks.
  if (chipAt(e)) e.getNativeEvent<MouseEvent>()?.preventDefault();
  // A data row that is already the one selected selects again as far
  // as the reader is concerned, though the selection model sees no
  // change: keep the persisted selection on it.
  const data = rowData(args.row);
  const selected = selectedRows();
  if (data && selected.length === 1 && rowKey(selected[0]) === rowKey(data)) {
    selectedRow.value = data;
    sel.value = rowKey(data);
    saveState();
  }
}

function onDblClick(e: SlickEventData, args: OnDblClickEventArgs) {
  // Double-click on a chip is everything from that person: the grid,
  // narrowed to their handle (docs/dev/plans/chips.md § Clicks).
  // On a group or step chip, it opens that group's dashboard or that
  // step's log.
  const chip = chipAt(e);
  if (chip?.dataset.entity) {
    openEntity(chip.dataset.entity);
    return;
  }
  if (chip) {
    appendFilterToQuery(filterToken("author_handle", chip.dataset.handle ?? "", false));
    return;
  }
  const data = rowData(args.row);
  if (data) openRow(data);
}

// Any change a USER can make to columns gets reflected in the persisted
// state: the grid reports resize, reorder, picker and sort changes
// here, and only those — its own layout work (a fit to the card's
// width, the adaptive visibility above) never does.
function onGridStateChanged(change: GridStateChange) {
  if (restoring) return;
  updateCols();
  const type = change.change?.type;
  if (type === "sorter") void runSearch(query.value);
  if (type === "columns") onColumnsShown();
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
  themeWatch = new MutationObserver(() => vueGrid?.setDarkMode(isDarkTheme()));
  themeWatch.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ["data-theme"],
  });
});
onBeforeUnmount(() => {
  searchDrop?.destroy();
  searchDrop = null;
  themeWatch?.disconnect();
  themeWatch = null;
  vueGrid?.dispose();
  vueGrid = null;
});
</script>

<template>
  <div ref="cardEl" class="grid-column">
    <div ref="searchWrapEl" class="search-input-wrap">
      <input
        v-model="query"
        :placeholder="hint"
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
      <span v-if="qmdCoverage" class="qmd-summary" :title="qmdCoverage.title">
        · {{ qmdCoverage.text }}
      </span>
    </div>

    <p v-if="qmdError" class="qmd-error" role="alert">Free-text search failed: {{ qmdError }}</p>
    <p v-if="unfinished" class="query-unread" role="status">
      {{ unfinished }}
      <template v-if="showingStale">The rows below are from the previous search.</template>
    </p>
    <p v-if="qmdIndexMissing" class="qmd-unbuilt" role="status">
      Free-text search starts working once the first sync builds the search index.
    </p>

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
    <p
      v-if="!loading && rows.length === 0 && !error && !qmdError && !qmdIndexMissing"
      class="empty"
    >
      no matches.
    </p>

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
.query-unread,
.qmd-unbuilt {
  padding: 0.4rem 0.6rem;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
  color: var(--datalib-muted);
  font-size: 0.9rem;
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
     do. The Sources card's grid box (`.sx-grid` in
     cards/sourcesCard.css) is the same pattern — flex-sized — and
     without a positioned box it collapsed, to 2px. */
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
/* A header dragged over the search bar is put in it for the length of the
   drag; it is not shown there, and the bar lights up instead. */
.search-input-wrap > .slick-header-column {
  display: none;
}
.search-input-wrap:has(> .slick-header-column) .search-input {
  outline: 2px solid var(--datalib-accent, #4a8bff);
}
</style>
