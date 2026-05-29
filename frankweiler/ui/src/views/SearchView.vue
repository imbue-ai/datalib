<script setup lang="ts">
import { ref, watch, onMounted, computed, nextTick } from "vue";
import { useRouter, useRoute } from "vue-router";
import { AgGridVue } from "ag-grid-vue3";
import { Splitpanes, Pane } from "splitpanes";
import "splitpanes/dist/splitpanes.css";
import {
  ModuleRegistry,
  AllCommunityModule,
  themeQuartz,
  colorSchemeVariable,
  type ColDef,
  type ColumnState,
  type GridApi,
  type GridOptions,
  type GridReadyEvent,
  type RowSelectedEvent,
  type CellContextMenuEvent,
  type IRowNode,
  type GetRowIdParams,
} from "ag-grid-community";
import {
  fetchAccounts,
  fetchHealth,
  fetchSearch,
  type AccountsMap,
  type Health,
  type SearchRow,
} from "@/api";
import ChatPreviewPane from "@/components/ChatPreviewPane.vue";
import FeedbackModal from "@/components/FeedbackModal.vue";
import { buildContext, type FeedbackContext } from "@/feedback/context";
import claudeIconUrl from "@/assets/claude.svg";
import chatgptIconUrl from "@/assets/chatgpt.svg";
import slackIconUrl from "@/assets/slack.svg";
import githubIconUrl from "@/assets/github.svg";
import gitlabIconUrl from "@/assets/gitlab.svg";
import notionIconUrl from "@/assets/notion.svg";

const SOURCE_ICONS: Record<string, string> = {
  Claude: claudeIconUrl,
  ChatGPT: chatgptIconUrl,
  Slack: slackIconUrl,
  GitHub: githubIconUrl,
  GitLab: gitlabIconUrl,
  Notion: notionIconUrl,
};

ModuleRegistry.registerModules([AllCommunityModule]);

const gridTheme = themeQuartz.withPart(colorSchemeVariable);

const router = useRouter();
const route = useRoute();
const query = ref(typeof route.query.q === "string" ? route.query.q : "");
const rows = ref<SearchRow[]>([]);
const total = ref(0);
const loading = ref(false);
const error = ref<string | null>(null);
// qmd-routed search failed at runtime; backend served LIKE-based
// fallback rows. Surface as a banner so users notice the degradation
// instead of silently getting worse results.
const qmdError = ref<string | null>(null);
const health = ref<Health | null>(null);
const accounts = ref<AccountsMap>({});
const selectedRow = ref<SearchRow | null>(null);

// AG Grid handle for applying / reading column state. Set by onGridReady.
let gridApi: GridApi<SearchRow> | null = null;

// Suppress hash writes while we're applying state from the URL ourselves
// — otherwise the grid's column-events would clobber the URL we just read.
let restoring = false;

// True once the user has manually clicked a column header (or the URL
// restored an explicit column state). Once set, we stop forcing the
// score-vs-time default on subsequent query result loads.
let userSortedManually = false;

function encodeColumnState(state: ColumnState[]): string {
  // Compact base64url so the URL stays vaguely readable when it shows up
  // in dev tools / shared links.
  const json = JSON.stringify(state);
  return btoa(unescape(encodeURIComponent(json)))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

function decodeColumnState(s: string): ColumnState[] | null {
  try {
    const padded = s.replace(/-/g, "+").replace(/_/g, "/");
    const json = decodeURIComponent(escape(atob(padded)));
    const parsed = JSON.parse(json);
    return Array.isArray(parsed) ? (parsed as ColumnState[]) : null;
  } catch {
    return null;
  }
}

function rowKey(row: SearchRow): string {
  return row.uuid;
}

// Lightroom-style: if multiple rows are selected and the right-click anchor
// is part of that selection, the action targets all selected rows;
// otherwise it targets only the anchor row.
function resolveTargetRows(
  api: GridApi<SearchRow>,
  anchor: IRowNode<SearchRow> | null | undefined,
): SearchRow[] {
  if (!anchor?.data) return [];
  const selected = api.getSelectedNodes() as IRowNode<SearchRow>[];
  if (selected.length > 1 && anchor.isSelected()) {
    return selected.map((n) => n.data).filter((d): d is SearchRow => d != null);
  }
  return [anchor.data];
}

// Context menu state
const contextMenuVisible = ref(false);
const contextMenuPos = ref({ x: 0, y: 0 });
const contextMenuTargets = ref<SearchRow[]>([]);
// The DOM element under the right-click. Stashed so a follow-up
// "Feedback…" click can reconstruct the breadcrumb pointing at the
// exact cell the user was looking at — the context menu itself sits
// above the grid, so re-deriving from a later event would point at the
// menu, not the cell.
const contextAnchorEl = ref<Element | null>(null);
// Column id (e.g. "author") + raw cell value snapshot for the feedback
// payload. Captured at right-click time so the modal sees what the user
// was pointing at even if the grid re-renders behind the dialog.
const contextCellInfo = ref<{ column: string; cellValue: string } | null>(null);

// Feedback modal state. The modal is surface-agnostic — we hand it a
// fully-built FeedbackContext and a short label for the title bar.
const feedbackOpen = ref(false);
const feedbackContext = ref<FeedbackContext | null>(null);
const feedbackSurfaceLabel = ref("");
// Filter context: which column the user right-clicked on, and the raw
// cell value to filter by. Null when the right-click happened on a
// non-filterable column (Time, Contents) or a row with no value there.
type FilterCtx = {
  // Query-language key (e.g. "source", "channel"); maps to a backend Field.
  key: string;
  // Friendly column header for the menu label ("Source", "Channel", ...).
  header: string;
  // Raw value to filter by (UUIDs for author/account, not display labels).
  value: string;
};
const contextFilter = ref<FilterCtx | null>(null);

// Optional "Filter by Notion Page" menu entry, populated when the right-
// clicked row has a non-empty `notion_page_uuid`. Lets users zoom into all
// rows on a single Notion page from any cell of any row on that page —
// useful because the page UUID isn't always the same as conversation_uuid
// (e.g. comment threads use the discussion UUID for that column).
const contextNotionPage = ref<FilterCtx | null>(null);

// Map AG Grid colId → query-language key + header. Keep in sync with
// `column_for_field` in backend/core/src/db.rs.
//
// `uuidCol` (when set) names a sibling row field carrying the load-bearing
// UUID for this filter. The cell's display text becomes a non-load-bearing
// slug; the emitted token is `slug-uuid` (Notion-shaped). Filter comparison
// is on UUID only — the slug is decoration so URLs/tokens are self-describing.
const FILTER_COLUMNS: Record<
  string,
  { key: string; header: string; uuidCol?: keyof SearchRow }
> = {
  source: { key: "source", header: "Source" },
  kind: { key: "kind", header: "Type" },
  channel: { key: "channel", header: "Channel" },
  author: { key: "author", header: "Author", uuidCol: "author" },
  account: { key: "account", header: "Account", uuidCol: "account" },
  project: { key: "project", header: "Project", uuidCol: "project" },
  conversation_name: {
    key: "convo",
    header: "Conversation",
    uuidCol: "conversation_uuid",
  },
};

function closeContextMenu() {
  contextMenuVisible.value = false;
  contextMenuTargets.value = [];
  contextFilter.value = null;
  contextNotionPage.value = null;
  contextAnchorEl.value = null;
  contextCellInfo.value = null;
}

function openFeedbackForCell() {
  const targets = contextMenuTargets.value;
  const info = contextCellInfo.value;
  if (targets.length === 0 || !info) {
    closeContextMenu();
    return;
  }
  const rowUuids = targets.map((r) => r.uuid);
  feedbackContext.value = buildContext({
    surface: "grid_cell",
    anchor: contextAnchorEl.value,
    targetUuids: rowUuids,
    payload: {
      column: info.column,
      row_uuids: rowUuids,
      cell_value: info.cellValue || null,
    },
  });
  feedbackSurfaceLabel.value = `Grid cell · ${info.column}${
    targets.length > 1 ? ` · ${targets.length} rows` : ""
  }`;
  feedbackOpen.value = true;
  closeContextMenu();
}

function openFeedbackForColumnHeader(ev: MouseEvent, colId: string) {
  ev.preventDefault();
  const anchor = ev.target instanceof Element ? ev.target : null;
  feedbackContext.value = buildContext({
    surface: "column_header",
    anchor,
    targetUuids: [],
    payload: { key: colId },
  });
  feedbackSurfaceLabel.value = `Column header · ${colId}`;
  feedbackOpen.value = true;
}

// AG Grid doesn't expose a column-header right-click event, so we
// listen on the grid wrapper and check whether the target sits inside a
// `.ag-header-cell`. The colId comes from the AG-Grid-supplied
// `col-id` attribute on that wrapper. Falls through (no preventDefault)
// when the target isn't a header so the existing cell-context handler
// still gets a chance to fire.
function onGridWrapContextMenu(ev: MouseEvent) {
  // Suppress the UA's native menu synchronously during bubble — AG Grid
  // dispatches `cellContextMenu` asynchronously, so its `preventDefault`
  // lands too late and the native menu wins over our custom `.ctx-menu`.
  ev.preventDefault();
  if (!(ev.target instanceof Element)) return;
  const headerCell = ev.target.closest(".ag-header-cell");
  if (!headerCell) return;
  const colId = headerCell.getAttribute("col-id") || "";
  if (!colId) return;
  openFeedbackForColumnHeader(ev, colId);
}

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

function openFeedbackForRow() {
  const targets = contextMenuTargets.value;
  if (targets.length === 0) {
    closeContextMenu();
    return;
  }
  const rowUuids = targets.map((r) => r.uuid);
  feedbackContext.value = buildContext({
    surface: "grid_row",
    anchor: contextAnchorEl.value,
    targetUuids: rowUuids,
    payload: { row_uuids: rowUuids },
  });
  feedbackSurfaceLabel.value =
    targets.length === 1 ? "Grid row" : `Grid rows · ${targets.length}`;
  feedbackOpen.value = true;
  closeContextMenu();
}

/// Quote a value for the search bar. Quotes when it contains whitespace,
/// `:`, leading `-`, or is empty. Mirrors the backend tokenizer's
/// quoted-span handling (`\"` and `\\` escapes inside quotes).
function quoteValue(v: string): string {
  const needsQuotes =
    v === "" ||
    /[\s:"]/.test(v) ||
    v.startsWith("-") ||
    v.startsWith('"');
  if (!needsQuotes) return v;
  const escaped = v.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
  return `"${escaped}"`;
}

function formatFilterToken(key: string, value: string, exclude: boolean): string {
  return `${exclude ? "-" : ""}${key}:${quoteValue(value)}`;
}

function appendFilterToQuery(token: string) {
  const current = query.value.trim();
  // Skip if the exact token is already present as its own whitespace-
  // delimited word (cheap dedupe; doesn't try to canonicalize quoting
  // variants, which is fine — duplicates only widen on free-text and
  // these tokens are field-prefixed, so they collapse on a re-click).
  const re = new RegExp(`(^|\\s)${escapeRegExp(token)}(\\s|$)`);
  if (re.test(current)) return;
  query.value = current.length === 0 ? token : `${current} ${token}`;
}

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

// Slugify a human-readable label for use as the non-load-bearing prefix in a
// Notion-shaped `slug-uuid` token. Conservative: ASCII alnum + hyphens only,
// max 40 chars, leading/trailing hyphens stripped. The backend ignores the
// slug entirely — it's just for token/URL self-description.
function slugifyForToken(label: string): string {
  const ascii = label
    .normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
  return ascii.slice(0, 40).replace(/-+$/, "");
}

const UUID_RE =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

// Compose `slug-uuid` (Notion URL pattern). When `slug` is empty (no display
// label available) or `uuid` is not UUID-shaped, falls back to just `uuid`.
function formatSlugUuid(slug: string, uuid: string): string {
  if (!UUID_RE.test(uuid)) return uuid;
  const s = slugifyForToken(slug);
  return s.length === 0 ? uuid : `${s}-${uuid}`;
}

function applyContextFilter(exclude: boolean) {
  const ctx = contextFilter.value;
  if (!ctx) return;
  appendFilterToQuery(formatFilterToken(ctx.key, ctx.value, exclude));
  closeContextMenu();
}

function applyNotionPageFilter(exclude: boolean) {
  const ctx = contextNotionPage.value;
  if (!ctx) return;
  appendFilterToQuery(formatFilterToken(ctx.key, ctx.value, exclude));
  closeContextMenu();
}

const slackLinkTargets = computed(() =>
  contextMenuTargets.value.filter((r) => r.slack_link),
);

function openTargetsInSlack() {
  for (const r of slackLinkTargets.value) {
    window.open(r.slack_link, "_blank", "noopener");
  }
  closeContextMenu();
}

async function copyTargetUuids() {
  const text = contextMenuTargets.value.map((r) => r.uuid).join(",");
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // Fallback for non-secure contexts.
    const ta = document.createElement("textarea");
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    document.body.removeChild(ta);
  }
  closeContextMenu();
}

function syncHash() {
  if (restoring) return;
  const q: Record<string, string> = {};
  if (query.value) q.q = query.value;
  if (selectedRow.value) q.sel = rowKey(selectedRow.value);
  else if (typeof route.query.sel === "string" && rows.value.length === 0) {
    // Preserve the URL's pending `sel` until rows load and
    // tryRestoreSelection has had a chance to run. AG Grid's column
    // events fire as microtasks after applyColumnState, slipping out of
    // the `restoring=true` window, and would otherwise clobber the
    // URL's selection target before we can restore it on reload.
    q.sel = route.query.sel;
  }
  if (gridApi) {
    const state = gridApi.getColumnState();
    if (state.length > 0) q.cols = encodeColumnState(state);
  }
  // replace, not push — selection / column tweaks are not history events.
  router.replace({ name: "search", query: q });
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
// naturally on page reload (which also re-reads server state via /api/health).
const SEARCH_CACHE_MAX = 16;
// Backend's hard ceiling — anything lower surfaces as silently-missing
// rows for the user. Memory/render cost is fine at this size thanks to
// AG Grid's row virtualization.
const SEARCH_LIMIT = 100_000;
type SearchCacheEntry = { rows: SearchRow[]; total: number; qmdError: string | null };
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
    return;
  }
  inflight = new AbortController();
  loading.value = true;
  error.value = null;
  qmdError.value = null;
  try {
    const r = await fetchSearch(q, SEARCH_LIMIT, inflight.signal);
    rows.value = r.rows;
    total.value = r.total_estimated;
    const qe =
      typeof r.query_echo?.qmd_error === "string" ? r.query_echo.qmd_error : null;
    qmdError.value = qe;
    cachePut(q, { rows: r.rows, total: r.total_estimated, qmdError: qe });
  } catch (e) {
    if ((e as { name?: string }).name === "AbortError") return;
    error.value = (e as Error).message;
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
  syncHash();
});

// Restore the selected row from the URL after rows load (or after the
// grid first becomes ready, whichever happens last — onGridReady can
// race with the initial fetch). Selection state outlives the result
// set: searches that drop the selected row leave selection cleared,
// which is the right behavior for a deep-link.
async function tryRestoreSelection() {
  const sel = route.query.sel;
  if (typeof sel !== "string" || !gridApi || rows.value.length === 0) return;
  if (selectedRow.value && rowKey(selectedRow.value) === sel) return;
  const target = rows.value.find((r) => rowKey(r) === sel);
  if (!target) return;
  // AG Grid creates row nodes from rowData asynchronously after Vue
  // pushes the prop. Wait one tick so forEachNode actually sees them.
  await nextTick();
  restoring = true;
  let found = false;
  gridApi.forEachNode((node) => {
    if (node.data && rowKey(node.data) === sel) {
      node.setSelected(true);
      gridApi!.ensureNodeVisible(node, "middle");
      found = true;
    }
  });
  if (found) selectedRow.value = target;
  restoring = false;
}

// Apply the default sort whenever results change, unless the user has
// taken sort into their own hands.
//   - qmd-scored results → score desc, scroll to top.
//   - everything else    → time ascending, scroll to bottom so the most
//                          recent rows are what the user lands on.
function applyDefaultSort() {
  if (!gridApi || userSortedManually) return;
  const hasScores = rows.value.some((r) => typeof r.score === "number");
  restoring = true;
  if (hasScores) {
    gridApi.applyColumnState({
      state: [
        { colId: "score", sort: "desc", sortIndex: 0 },
        { colId: "when", sort: null, sortIndex: null },
      ],
      defaultState: { sort: null },
    });
  } else {
    gridApi.applyColumnState({
      state: [
        { colId: "score", sort: null, sortIndex: null },
        { colId: "when", sort: "asc", sortIndex: 0 },
      ],
      defaultState: { sort: null },
    });
  }
  restoring = false;
  // ensureIndexVisible needs the post-sort row order to be computed,
  // which happens after the current tick.
  nextTick(() => {
    if (!gridApi) return;
    // If the URL is pinning a row, tryRestoreSelection scrolls to it —
    // don't fight that with a default scroll.
    if (typeof route.query.sel === "string" && route.query.sel.length > 0) {
      return;
    }
    if (hasScores) {
      gridApi.ensureIndexVisible(0, "top");
    } else {
      const last = gridApi.getDisplayedRowCount() - 1;
      if (last >= 0) gridApi.ensureIndexVisible(last, "bottom");
    }
  });
}

// Adaptive column visibility: on every results load, columns whose
// values are all identical (including all-empty) get hidden; columns
// with varying values get shown. The user's pick was "adaptive rule
// wins" — manual column-visibility toggles get overwritten on the
// next query.
const ADAPTIVE_FIELDS: (keyof SearchRow)[] = [
  "score",
  "source",
  "kind",
  "channel",
  "when",
  "author",
  "account",
];

function stringifyForCompare(v: unknown): string {
  if (v == null) return "";
  return typeof v === "string" ? v : String(v);
}

function applyAdaptiveVisibility() {
  if (!gridApi || rows.value.length === 0) return;
  const state = ADAPTIVE_FIELDS.map((field) => {
    const first = stringifyForCompare(rows.value[0][field]);
    const allSame = rows.value.every(
      (r) => stringifyForCompare(r[field]) === first,
    );
    return { colId: field as string, hide: allSame };
  });
  restoring = true;
  gridApi.applyColumnState({ state });
  restoring = false;
}

watch(rows, () => {
  applyAdaptiveVisibility();
  applyDefaultSort();
  tryRestoreSelection();
});

onMounted(async () => {
  try {
    health.value = await fetchHealth();
  } catch {
    /* health is best-effort */
  }
  try {
    accounts.value = await fetchAccounts();
  } catch {
    /* accounts mapping is best-effort */
  }
  runSearch(query.value);
});

function openRow(row: SearchRow) {
  const href = router.resolve({
    name: "chat",
    params: { conversationUuid: row.conversation_uuid },
    hash: row.message_index != null ? `#m${row.message_index}` : undefined,
  }).href;
  window.open(href, "_blank", "noopener");
}

const columnDefs = computed<ColDef<SearchRow>[]>(() => [
  {
    field: "score",
    headerName: "Score",
    width: 90,
    // Default sort is applied programmatically on row updates (see
    // applyDefaultSort) — we don't bake it into the colDef so a user
    // re-sort sticks across query changes.
    valueFormatter: (p) => {
      const v = p.value;
      return typeof v === "number" ? v.toFixed(3) : "";
    },
    cellStyle: { "text-align": "right" } as Record<string, string>,
    // QMD scores aren't comparable across queries; hide the filter UI
    // (range filter would be misleading) but keep the column sortable.
    filter: false,
  },
  {
    field: "source",
    headerName: "Source",
    width: 90,
    cellRenderer: (params: { value: unknown }) => {
      const v = typeof params.value === "string" ? params.value : "";
      const icon = SOURCE_ICONS[v];
      if (!icon) return v;
      const img = document.createElement("img");
      img.src = icon;
      img.alt = v;
      img.title = v;
      img.className = "source-icon";
      return img;
    },
  },
  { field: "kind", headerName: "Type", width: 110 },
  { field: "channel", headerName: "Channel", width: 130 },
  {
    field: "when",
    headerName: "Time",
    width: 165,
  },
  {
    field: "snippet",
    headerName: "Contents",
    flex: 1,
    minWidth: 200,
    // Two-line clamp via a custom cellRenderer. autoHeight is intentionally
    // OFF (per-row measurement was the dominant render cost on large
    // result sets), and the row height is fixed at 52px to fit two lines.
    // We render our own <div> so the clamp styles land on the direct text
    // container — AG Grid's default .ag-cell-value span sits inside a
    // flex cell and won't clamp reliably.
    cellRenderer: (p: { value: unknown }) => {
      const div = document.createElement("div");
      div.className = "fw-clamp-2";
      div.textContent = p.value == null ? "" : String(p.value);
      return div;
    },
  },
  {
    field: "author",
    headerName: "Author",
    width: 130,
    valueFormatter: (p) => {
      const v = p.value as string | undefined;
      if (!v) return "";
      return accounts.value[v]?.label ?? v;
    },
  },
  {
    field: "account",
    headerName: "Account",
    width: 150,
    hide: true,
    valueFormatter: (p) => accountLabel(p.value as string),
  },
]);

const defaultColDef: ColDef = {
  resizable: true,
  sortable: true,
  filter: true,
};

const gridOptions: GridOptions<SearchRow> = {
  theme: gridTheme,
  animateRows: false,
  // Tall enough for two lines of clamped snippet text plus padding.
  rowHeight: 52,
  // Single-row selection drives the right preview pane. Cell text
  // selection is intentionally NOT enabled here: AG Grid's text-selection
  // mode swallows row clicks, breaking the preview wiring.
  // multiRow so right-click "Copy UUID(s)" can target several rows, like
  // Lightroom. Single-click still narrows to one row; preview pane follows
  // whichever row was most recently toggled on.
  rowSelection: { mode: "multiRow", checkboxes: false, enableClickSelection: true },
  ensureDomOrder: true,
  getRowId: (p: GetRowIdParams<SearchRow>) => p.data.uuid,
  onGridReady: (e: GridReadyEvent<SearchRow>) => {
    gridApi = e.api;
    // Expose the grid api so e2e tests can scroll virtualized rows
    // into view before clicking.
    (window as unknown as { __fwGridApi?: GridApi<SearchRow> }).__fwGridApi =
      e.api;
    const cols = route.query.cols;
    if (typeof cols === "string") {
      const state = decodeColumnState(cols);
      if (state) {
        restoring = true;
        gridApi.applyColumnState({ state, applyOrder: true });
        restoring = false;
        // An explicit URL-encoded column state carries the user's sort
        // choice — don't clobber it with our default.
        if (state.some((c) => c.sort != null)) userSortedManually = true;
      }
    }
    // Rows may already be loaded by the time the grid is ready.
    applyDefaultSort();
    tryRestoreSelection();
  },
  onRowSelected: (e: RowSelectedEvent<SearchRow>) => {
    if (e.node.isSelected() && e.data) {
      selectedRow.value = e.data;
      syncHash();
    }
  },
  onRowDoubleClicked: (e) => {
    if (e.data) openRow(e.data);
  },
  onCellContextMenu: (e: CellContextMenuEvent<SearchRow>) => {
    if (!gridApi) return;
    const targets = resolveTargetRows(gridApi, e.node);
    if (targets.length === 0) return;
    const me = e.event as MouseEvent | null;
    if (me) {
      me.preventDefault();
      contextMenuPos.value = { x: me.clientX, y: me.clientY };
      contextAnchorEl.value =
        me.target instanceof Element ? me.target : null;
    }
    // Lightroom: right-clicking an unselected row narrows selection to it.
    if (!e.node.isSelected()) {
      gridApi.deselectAll();
      e.node.setSelected(true);
    }
    contextMenuTargets.value = targets;
    // Capture which column was clicked so we can offer "Keep only" /
    // "Exclude" entries for filterable columns. For UUID-bearing columns
    // (author / account / project / convo) the emitted token follows the
    // Notion URL pattern `slug-uuid` — the slug is non-load-bearing display,
    // the trailing UUID is what the backend filters on. Slug comes from the
    // human-readable display of the cell; UUID from the row's `uuidCol`.
    const colId = e.column?.getColId() ?? "";
    const meta = FILTER_COLUMNS[colId];
    contextFilter.value = null;
    // Snapshot the cell value for "Feedback…". valueFormatter result is
    // closer to what the user actually sees (e.g. author UUID → label)
    // than the raw row field, so prefer it when available.
    let cellRendered: string;
    try {
      const fmt = e.value;
      cellRendered =
        typeof fmt === "string"
          ? fmt
          : fmt == null
            ? ""
            : String(fmt);
    } catch {
      cellRendered = "";
    }
    contextCellInfo.value = { column: colId, cellValue: cellRendered };
    if (meta && e.data) {
      const cellRaw = (e.data as Record<string, unknown>)[colId];
      if (meta.uuidCol) {
        const uuid = (e.data as Record<string, unknown>)[meta.uuidCol];
        if (typeof uuid === "string" && uuid.length > 0) {
          // For author/account, the cell raw IS the UUID; display the label
          // from accounts.json. For convo, the cell raw is the conversation
          // name. For project, no display label source — slug stays empty.
          let displayLabel = "";
          if (colId === "author" || colId === "account") {
            displayLabel = accounts.value[uuid]?.label ?? "";
          } else if (colId === "conversation_name") {
            displayLabel = typeof cellRaw === "string" ? cellRaw : "";
          }
          const value = formatSlugUuid(displayLabel, uuid);
          contextFilter.value = { key: meta.key, header: meta.header, value };
        }
      } else if (typeof cellRaw === "string" && cellRaw.length > 0) {
        contextFilter.value = { key: meta.key, header: meta.header, value: cellRaw };
      }
    }
    // Independent of the per-column filter: if this row belongs to a Notion
    // page, offer a "Filter by Notion Page" entry. The slug comes from
    // conversation_name when that row's conversation_uuid equals the page
    // uuid (i.e. the row is the page itself); otherwise we have no clean
    // display label and emit just the UUID.
    if (e.data && e.data.notion_page_uuid) {
      const pageUuid = e.data.notion_page_uuid;
      const slug =
        e.data.conversation_uuid === pageUuid ? e.data.conversation_name : "";
      contextNotionPage.value = {
        key: "notion_page",
        header: "Notion Page",
        value: formatSlugUuid(slug, pageUuid),
      };
    }
    contextMenuVisible.value = true;
  },
  // Any change a user can make to columns gets reflected in the URL.
  onColumnVisible: () => syncHash(),
  onColumnResized: (e) => {
    if (e.finished) syncHash();
  },
  onColumnMoved: (e) => {
    if (e.finished) syncHash();
  },
  onSortChanged: (e) => {
    // Only treat header-click sorts as "user intent". Programmatic
    // sorts (our applyDefaultSort) come through with source 'api', so
    // they don't flip the flag.
    if (!restoring && e.source === "uiColumnSorted") {
      userSortedManually = true;
    }
    syncHash();
  },
};
</script>

<template>
  <section class="search-view">
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

    <div v-if="health" class="health">
      backend ok · {{ total }} conversations indexed under
      <code>{{ health.root }}</code>
      <span v-if="!health.root_exists" class="warn"> (root does not exist)</span>
    </div>

    <p v-if="qmdError" class="qmd-error" role="alert">
      qmd search failed — results below are from a degraded SQL-LIKE
      fallback: {{ qmdError }}
    </p>

    <p v-if="error" class="error">error: {{ error }}</p>

    <Splitpanes class="split" :dbl-click-splitter="false">
      <Pane size="55" min-size="25" class="left-pane">
        <div class="grid-wrap" @contextmenu="onGridWrapContextMenu">
          <AgGridVue
            class="grid"
            :class="{ 'grid--loading': loading }"
            :rowData="rows"
            :columnDefs="columnDefs"
            :defaultColDef="defaultColDef"
            :gridOptions="gridOptions"
          />
          <div v-if="loading" class="grid-spinner" aria-label="searching">
            <div class="grid-spinner__ring" />
            <div class="grid-spinner__label">searching…</div>
          </div>
        </div>
        <p v-if="!loading && rows.length === 0 && !error" class="empty">
          no matches.
        </p>
      </Pane>
      <Pane size="45" min-size="20" class="right-pane">
        <ChatPreviewPane
          :conversation-uuid="selectedRow?.conversation_uuid ?? null"
          :selected-section-uuid="
            selectedRow != null && selectedRow.kind !== 'Chat'
              ? selectedRow.uuid
              : null
          "
        />
      </Pane>
    </Splitpanes>

    <div
      v-if="contextMenuVisible"
      class="ctx-overlay"
      @click="closeContextMenu"
      @contextmenu.prevent="closeContextMenu"
    >
      <div
        class="ctx-menu"
        :style="{ top: contextMenuPos.y + 'px', left: contextMenuPos.x + 'px' }"
        @click.stop
      >
        <div class="ctx-header">
          Targeting {{ contextMenuTargets.length }} row{{ contextMenuTargets.length === 1 ? '' : 's' }}
        </div>
        <template v-if="contextFilter">
          <div class="ctx-item" @click="applyContextFilter(false)">
            Keep only {{ contextFilter.header }}={{ contextFilter.value }}
          </div>
          <div class="ctx-item" @click="applyContextFilter(true)">
            Exclude all {{ contextFilter.header }}={{ contextFilter.value }}
          </div>
          <div class="ctx-divider" />
        </template>
        <template v-if="contextNotionPage">
          <div class="ctx-item" @click="applyNotionPageFilter(false)">
            Keep only Notion Page={{ contextNotionPage.value }}
          </div>
          <div class="ctx-item" @click="applyNotionPageFilter(true)">
            Exclude all Notion Page={{ contextNotionPage.value }}
          </div>
          <div class="ctx-divider" />
        </template>
        <div class="ctx-item" @click="copyTargetUuids">
          Copy UUID{{ contextMenuTargets.length === 1 ? '' : 's' }}
        </div>
        <div
          v-if="slackLinkTargets.length > 0"
          class="ctx-item"
          @click="openTargetsInSlack"
        >
          Open in Slack{{ slackLinkTargets.length === 1 ? '' : ` (${slackLinkTargets.length})` }}
        </div>
        <div class="ctx-divider" />
        <div v-if="contextCellInfo" class="ctx-item" @click="openFeedbackForCell">
          Feedback on this cell…
        </div>
        <div class="ctx-item" @click="openFeedbackForRow">
          Feedback on row{{ contextMenuTargets.length === 1 ? '' : 's' }}…
        </div>
      </div>
    </div>

    <FeedbackModal
      :open="feedbackOpen"
      :surface-label="feedbackSurfaceLabel"
      :context="feedbackContext"
      @close="feedbackOpen = false"
    />
  </section>
</template>

<style scoped>
.search-view {
  display: flex;
  flex-direction: column;
  height: calc(100vh - 6rem);
  gap: 0.5rem;
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
  background: var(--fw-input-bg);
  color: var(--fw-fg);
  border: 1px solid var(--fw-border);
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
  color: var(--fw-muted);
  background: transparent;
  border: none;
  border-radius: 50%;
  cursor: pointer;
}
.search-clear:hover {
  color: var(--fw-fg);
  background: var(--fw-border);
}
.health {
  font-size: 0.85rem;
  color: var(--fw-muted);
}
.health code {
  background: var(--fw-code-bg);
  padding: 0 0.25rem;
  border-radius: 2px;
}
.warn {
  color: #d18a3a;
  margin-left: 0.5rem;
}
.empty,
.error {
  color: var(--fw-muted);
}
.error {
  color: #e35d6a;
}
.qmd-error {
  padding: 0.4rem 0.6rem;
  border: 1px solid #d18a3a;
  border-radius: 4px;
  background: rgba(209, 138, 58, 0.1);
  color: #d18a3a;
  font-size: 0.9rem;
}
.split {
  flex: 1 1 auto;
  min-height: 300px;
}
.left-pane {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.right-pane {
  display: flex;
  flex-direction: column;
  min-width: 0;
  border-left: 1px solid var(--fw-border);
  background: var(--fw-bg);
}
.grid-wrap {
  flex: 1 1 auto;
  min-height: 200px;
  position: relative;
}
.grid {
  width: 100%;
  height: 100%;
  transition: filter 120ms ease-out;
}
.grid--loading {
  filter: blur(2px);
  pointer-events: none;
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
  /* Above the (blurred) grid but below any modals/menus. */
  z-index: 5;
}
.grid-spinner__ring {
  width: 36px;
  height: 36px;
  border-radius: 50%;
  border: 3px solid var(--fw-border);
  border-top-color: var(--fw-accent, #4a8bff);
  animation: fw-spin 800ms linear infinite;
}
.grid-spinner__label {
  font-size: 0.85rem;
  color: var(--fw-muted);
}
@keyframes fw-spin {
  to { transform: rotate(360deg); }
}
.ctx-overlay {
  position: fixed;
  inset: 0;
  z-index: 1500;
  background: transparent;
}
.ctx-menu {
  position: fixed;
  background: var(--fw-input-bg, #fff);
  color: var(--fw-fg, #000);
  border: 1px solid var(--fw-border, #ccc);
  border-radius: 4px;
  box-shadow: 0 2px 10px rgba(0, 0, 0, 0.2);
  min-width: 180px;
  padding: 4px 0;
  z-index: 1501;
  font-size: 14px;
}
.ctx-header {
  padding: 6px 12px 8px;
  font-size: 11px;
  font-weight: 600;
  letter-spacing: 0.02em;
  text-transform: uppercase;
  color: var(--fw-fg-muted, #888);
  border-bottom: 1px solid var(--fw-border, #ccc);
  margin-bottom: 2px;
  user-select: none;
}
.ctx-item {
  padding: 8px 16px;
  cursor: pointer;
  user-select: none;
}
.ctx-item:hover {
  background: var(--fw-accent, #eee);
}
.ctx-divider {
  height: 1px;
  background: var(--fw-border, #ccc);
  margin: 4px 0;
}
</style>

<style>
/* Splitter styling — outside scoped block so it reaches splitpanes' DOM. */
.splitpanes__splitter {
  background: var(--fw-border);
  position: relative;
  width: 6px;
  cursor: col-resize;
}
.splitpanes__splitter:hover {
  background: var(--fw-accent);
}
.source-icon {
  width: 20px;
  height: 20px;
  vertical-align: middle;
  display: inline-block;
}
.fw-clamp-2 {
  display: -webkit-box;
  -webkit-box-orient: vertical;
  -webkit-line-clamp: 2;
  line-clamp: 2;
  overflow: hidden;
  white-space: normal;
  line-height: 1.25;
  width: 100%;
  /* break-all lets the ellipsis land mid-word when a long word would
     otherwise wrap whole to a clipped third line, leaving line 2 short. */
  word-break: break-all;
  overflow-wrap: anywhere;
}
</style>
