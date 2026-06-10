<script setup lang="ts">
// One Miller column: the search bar + AG Grid + grid context menu +
// feedback wiring. Self-contained — owns query, fetched rows, accounts,
// and writes its own URL params (`q`, `sel`, `cols` (AG Grid state)).
// Emits `select-row` upward so the parent MillerView can decide what
// document column to show.
import { ref, watch, onMounted, computed, nextTick } from "vue";
import { useRouter } from "vue-router";
import { AgGridVue } from "ag-grid-vue3";
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
  type MenuItemDef,
  type DefaultMenuItem,
  type GetContextMenuItemsParams,
} from "ag-grid-community";
import { AllEnterpriseModule } from "ag-grid-enterprise";
import {
  fetchAccounts,
  fetchHealth,
  fetchSearch,
  type AccountsMap,
  type Health,
  type SearchRow,
} from "@/api";
import FeedbackModal from "@/components/FeedbackModal.vue";
import { buildContext, type FeedbackContext } from "@/feedback/context";
import { encodeStack } from "@/router/columns";
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

ModuleRegistry.registerModules([AllCommunityModule, AllEnterpriseModule]);

const gridTheme = themeQuartz.withPart(colorSchemeVariable);

// Parent (MillerView) owns the column's URL-persisted state — query,
// selected row uuid, AG Grid column-state blob — and threads it
// through these props. We mirror them to local refs for input
// reactivity; a `syncing` flag breaks the prop → local → emit → prop
// loop that would otherwise fire on every external write.
const props = defineProps<{
  q: string;
  sel: string | null;
  agCols: string | null;
}>();

const emit = defineEmits<{
  // Fired whenever a grid row becomes the active selection. The
  // payload is the row data (so the parent can read `markdown_uuid`,
  // `uuid`, `kind`, etc.) and a `restoring` flag — true when the
  // event came from URL-restore rather than a user click, so the
  // parent can avoid clobbering already-restored downstream state.
  (e: "select-row", row: SearchRow, restoring: boolean): void;
  // v-model-style writebacks for the three pieces of URL state this
  // column owns. `update:agCols` may pass `null` when the AG Grid
  // column state is empty (no custom sort / column changes yet).
  (e: "update:q", q: string): void;
  (e: "update:agCols", agCols: string | null): void;
}>();

const router = useRouter();
const query = ref(props.q);
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

// True while we are applying a value that came from a prop (rather
// than from a user action), so internal watchers know to skip the
// emit-back. Same role as the existing `restoring` flag used for
// AG-Grid-API-driven mutations.
let syncingFromProps = false;

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

// The DOM element under the most recent right-click. Stashed in
// `onCellContextMenu` so a follow-up "Feedback…" menu action can
// reconstruct the breadcrumb pointing at the exact cell the user was
// looking at — AG Grid's `getContextMenuItems` callback doesn't get
// the originating MouseEvent, so we capture it on the side.
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
// Filter context for a right-clicked cell — built on the fly inside
// `getContextMenuItems`. Null for non-filterable columns (Time,
// Contents) or rows with no value in the clicked column.
type FilterCtx = {
  // Query-language key (e.g. "source", "channel"); maps to a backend Field.
  key: string;
  // Friendly column header for the menu label ("Source", "Channel", ...).
  header: string;
  // Raw value to filter by (UUIDs for author/account, not display labels).
  value: string;
};

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
    .replace(/[̀-ͯ]/g, "")
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

async function copyUuids(targets: SearchRow[]) {
  const text = targets.map((r) => r.uuid).join(",");
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
}

// Build a FilterCtx for the cell at `colId` on the given row, or null
// when the column is non-filterable or has no value to filter by.
// Mirrors the per-column logic from the old DOM-menu builder.
function buildFilterCtx(colId: string, data: SearchRow): FilterCtx | null {
  const meta = FILTER_COLUMNS[colId];
  if (!meta) return null;
  const row = data as Record<string, unknown>;
  const cellRaw = row[colId];
  if (meta.uuidCol) {
    const uuid = row[meta.uuidCol as string];
    if (typeof uuid !== "string" || uuid.length === 0) return null;
    let displayLabel = "";
    if (colId === "author" || colId === "account") {
      displayLabel = accounts.value[uuid]?.label ?? "";
    } else if (colId === "conversation_name") {
      displayLabel = typeof cellRaw === "string" ? cellRaw : "";
    }
    return { key: meta.key, header: meta.header, value: formatSlugUuid(displayLabel, uuid) };
  }
  if (typeof cellRaw === "string" && cellRaw.length > 0) {
    return { key: meta.key, header: meta.header, value: cellRaw };
  }
  return null;
}

// Emit the current AG Grid column state to the parent. Called from
// the column-event hooks (resize / sort / move / visibility) — query
// changes flow via `watch(query, …)` and selection via `select-row`,
// so each piece of state has its own narrow emit path. Skipped during
// programmatic mutation (`restoring`) and during prop sync.
function emitAgCols() {
  if (restoring || syncingFromProps) return;
  if (!gridApi) return;
  const state = gridApi.getColumnState();
  emit("update:agCols", state.length > 0 ? encodeColumnState(state) : null);
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
  if (!syncingFromProps) emit("update:q", q);
});

// Mirror external prop changes (back/forward nav, parent-driven
// rewrites) into our local refs without re-emitting them.
watch(
  () => props.q,
  (q) => {
    if (q === query.value) return;
    syncingFromProps = true;
    query.value = q;
    syncingFromProps = false;
  },
);

// Restore the selected row from the parent-provided `sel` prop after
// rows load (or after the grid first becomes ready, whichever happens
// last — onGridReady can race with the initial fetch). Selection state
// outlives the result set: searches that drop the selected row leave
// selection cleared, which is the right behavior for a deep-link.
async function tryRestoreSelection() {
  const sel = props.sel;
  if (!sel || !gridApi || rows.value.length === 0) return;
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

// External `sel` change (back/forward nav, or parent rewrote URL):
// reflect into the grid selection. Skipped while we're already
// applying it, and idempotent if the prop matches what we have.
watch(
  () => props.sel,
  () => {
    tryRestoreSelection();
  },
);

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
  // ensureIndexVisible needs the post-sort row order to be computed
  // AND the new rowData to be ingested by AG Grid's virtualizer. A
  // single Vue nextTick fires before that finishes — the grid is
  // still showing the previous result set, so ensureIndexVisible
  // operates on stale rows and the scroll write gets clobbered as
  // soon as the new rows land. We instead listen for the grid's own
  // `rowDataUpdated` event, which fires once the new rows are in
  // place; the listener runs at most once per applyDefaultSort call.
  if (props.sel) {
    // tryRestoreSelection will scroll to the pinned row; don't fight it.
    return;
  }
  const target: "top" | "bottom" = hasScores ? "top" : "bottom";
  const api = gridApi;
  const scrollToEnd = () => {
    if (target === "top") {
      api.ensureIndexVisible(0, "top");
    } else {
      const last = api.getDisplayedRowCount() - 1;
      if (last >= 0) api.ensureIndexVisible(last, "bottom");
    }
  };
  // Subscribe to the next rowDataUpdated event, then deregister.
  // Wrapped in a try/catch because ag-grid versions disagree on
  // whether one-shot subscriptions are allowed.
  const handler = () => {
    scrollToEnd();
    api.removeEventListener("rowDataUpdated", handler);
  };
  try {
    api.addEventListener("rowDataUpdated", handler);
  } catch {
    /* fall through to the rAF-based scroll */
  }
  // Also schedule a deferred scroll via two animation frames — covers
  // the case where rowDataUpdated already fired before we subscribed
  // (the row prop assignment that triggered this applyDefaultSort
  // call also lands in AG Grid synchronously in some code paths).
  requestAnimationFrame(() => {
    requestAnimationFrame(() => {
      scrollToEnd();
    });
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
  // Double-click → open this row's doc as a standalone single-column
  // stack in a new tab. Same shape as the "view this column alone ↗"
  // link in DocColumn: `/doc:<uuid>`. The message-anchor hash
  // (`#m<index>`) is appended when present so deep-linking to a
  // specific message still works.
  const md = row.markdown_uuid ?? row.uuid;
  const path = encodeStack([{ kind: "doc", md }]);
  const hash = row.message_index != null ? `#m${row.message_index}` : "";
  const href = router.resolve(path).href + hash;
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
  {
    // Cell renders the human-readable org_name; the row also carries
    // org_uuid (shown on hover as the tooltip) so filtering / scripts
    // can target the stable opaque key.
    field: "org_name",
    headerName: "Org",
    width: 130,
    hide: true,
    tooltipField: "org_uuid",
  },
]);

const defaultColDef: ColDef = {
  resizable: true,
  sortable: true,
  filter: true,
  enableRowGroup: true,
};

const gridOptions: GridOptions<SearchRow> = {
  theme: gridTheme,
  animateRows: false,
  // Enterprise: drag-to-group panel above the grid + columns tool panel on
  // the right. Both are pure UI affordances over existing column state, so
  // they cost nothing when unused.
  rowGroupPanelShow: "always",
  sideBar: "columns",
  // `preventDefaultOnContextMenu: true` makes AG Grid call
  // preventDefault() synchronously on the contextmenu event, which is
  // what the grid-context-menu e2e test pins (so the browser's native
  // menu never shows over the grid's). Our app-specific entries are
  // prepended to AG Grid's defaults via `getContextMenuItems` below.
  preventDefaultOnContextMenu: true,
  getContextMenuItems: (
    params: GetContextMenuItemsParams<SearchRow>,
  ): (MenuItemDef<SearchRow> | DefaultMenuItem)[] => {
    const defaults = params.defaultItems ?? [];
    if (!gridApi) return defaults;
    const node = params.node as IRowNode<SearchRow> | null;
    if (!node?.data) return defaults;
    const targets = resolveTargetRows(gridApi, node);
    if (targets.length === 0) return defaults;
    const rowUuids = targets.map((r) => r.uuid);
    const colId = params.column?.getColId() ?? "";
    const filterCtx = buildFilterCtx(colId, node.data);
    const notionCtx: FilterCtx | null = node.data.notion_page_uuid
      ? {
          key: "notion_page",
          header: "Notion Page",
          value: formatSlugUuid(
            node.data.conversation_uuid === node.data.notion_page_uuid
              ? node.data.conversation_name
              : "",
            node.data.notion_page_uuid,
          ),
        }
      : null;
    const slackTargets = targets.filter((r) => r.slack_link);
    // Anchor + cell info come from onCellContextMenu (it fires before
    // getContextMenuItems on the same right-click). Snapshot now so
    // each item action closes over the right values even if the user
    // dismisses and re-opens the menu before clicking.
    const anchor = contextAnchorEl.value;
    const cellInfo = contextCellInfo.value;
    const plural = targets.length === 1 ? "" : "s";

    const items: (MenuItemDef<SearchRow> | DefaultMenuItem)[] = [];
    if (filterCtx) {
      items.push(
        {
          name: `Keep only ${filterCtx.header}=${filterCtx.value}`,
          action: () =>
            appendFilterToQuery(
              formatFilterToken(filterCtx.key, filterCtx.value, false),
            ),
        },
        {
          name: `Exclude all ${filterCtx.header}=${filterCtx.value}`,
          action: () =>
            appendFilterToQuery(
              formatFilterToken(filterCtx.key, filterCtx.value, true),
            ),
        },
        "separator",
      );
    }
    if (notionCtx) {
      items.push(
        {
          name: `Keep only Notion Page=${notionCtx.value}`,
          action: () =>
            appendFilterToQuery(
              formatFilterToken(notionCtx.key, notionCtx.value, false),
            ),
        },
        {
          name: `Exclude all Notion Page=${notionCtx.value}`,
          action: () =>
            appendFilterToQuery(
              formatFilterToken(notionCtx.key, notionCtx.value, true),
            ),
        },
        "separator",
      );
    }
    items.push({
      name: `Copy UUID${plural}`,
      action: () => {
        void copyUuids(targets);
      },
    });
    if (slackTargets.length > 0) {
      items.push({
        name: `Open in Slack${
          slackTargets.length === 1 ? "" : ` (${slackTargets.length})`
        }`,
        action: () => {
          for (const r of slackTargets) {
            window.open(r.slack_link, "_blank", "noopener");
          }
        },
      });
    }
    if (cellInfo) {
      items.push({
        name: "Feedback on this cell…",
        action: () => {
          feedbackContext.value = buildContext({
            surface: "grid_cell",
            anchor,
            targetUuids: rowUuids,
            payload: {
              column: cellInfo.column,
              row_uuids: rowUuids,
              cell_value: cellInfo.cellValue || null,
            },
          });
          feedbackSurfaceLabel.value = `Grid cell · ${cellInfo.column}${
            targets.length > 1 ? ` · ${targets.length} rows` : ""
          }`;
          feedbackOpen.value = true;
        },
      });
    }
    items.push({
      name: `Feedback on row${plural}…`,
      action: () => {
        feedbackContext.value = buildContext({
          surface: "grid_row",
          anchor,
          targetUuids: rowUuids,
          payload: { row_uuids: rowUuids },
        });
        feedbackSurfaceLabel.value =
          targets.length === 1 ? "Grid row" : `Grid rows · ${targets.length}`;
        feedbackOpen.value = true;
      },
    });
    if (defaults.length > 0) items.push("separator", ...defaults);
    return items;
  },
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
    if (props.agCols) {
      const state = decodeColumnState(props.agCols);
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
      // Tell the parent which row is selected. `restoring` is true
      // when this is a URL-driven re-selection (we don't want the
      // parent to wipe URL-restored downstream column state in that
      // case). The parent rewrites both `sel` and the deeper columns
      // for us — there's no separate `update:sel` event.
      emit("select-row", e.data, restoring);
    }
  },
  onRowDoubleClicked: (e) => {
    if (e.data) openRow(e.data);
  },
  onCellContextMenu: (e: CellContextMenuEvent<SearchRow>) => {
    if (!gridApi) return;
    const me = e.event as MouseEvent | null;
    contextAnchorEl.value =
      me?.target instanceof Element ? me.target : null;
    // Snapshot the cell value for the eventual "Feedback…" action. The
    // `e.value` here is the displayed value (post-valueFormatter), which
    // is closer to what the user actually sees (e.g. author UUID → label)
    // than the raw row field.
    const colId = e.column?.getColId() ?? "";
    const v = e.value;
    const cellRendered =
      typeof v === "string" ? v : v == null ? "" : String(v);
    contextCellInfo.value = { column: colId, cellValue: cellRendered };
    // Lightroom: right-clicking an unselected row narrows selection to it.
    if (e.node && !e.node.isSelected()) {
      gridApi.deselectAll();
      e.node.setSelected(true);
    }
  },
  // Any change a user can make to columns gets reflected in the URL.
  onColumnVisible: () => emitAgCols(),
  onColumnResized: (e) => {
    if (e.finished) emitAgCols();
  },
  onColumnMoved: (e) => {
    if (e.finished) emitAgCols();
  },
  onSortChanged: (e) => {
    // Only treat header-click sorts as "user intent". Programmatic
    // sorts (our applyDefaultSort) come through with source 'api', so
    // they don't flip the flag.
    if (!restoring && e.source === "uiColumnSorted") {
      userSortedManually = true;
    }
    emitAgCols();
  },
};

// External `agCols` change (parent rewrote the URL): re-apply to the
// grid. No-op when we don't have an api yet (the agCols prop is also
// read in onGridReady for that case).
watch(
  () => props.agCols,
  (next) => {
    if (!gridApi) return;
    if (!next) return;
    const state = decodeColumnState(next);
    if (!state) return;
    // Compare encoded form to current — avoid re-applying our own
    // emitted value and triggering another column-event burst.
    const current = encodeColumnState(gridApi.getColumnState());
    if (current === next) return;
    syncingFromProps = true;
    restoring = true;
    gridApi.applyColumnState({ state, applyOrder: true });
    restoring = false;
    syncingFromProps = false;
  },
);
</script>

<template>
  <div class="grid-column">
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

    <div class="grid-wrap">
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

    <FeedbackModal
      :open="feedbackOpen"
      :surface-label="feedbackSurfaceLabel"
      :context="feedbackContext"
      @close="feedbackOpen = false"
    />
  </div>
</template>

<style scoped>
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
</style>

<style>
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
  /* `word-break: normal` keeps line wraps at word boundaries (so we
     never get "bu / t the trilithium…" from the user's bug report),
     while `overflow-wrap: break-word` still allows a single super-long
     word to break when it can't fit on its own line. The line-clamp
     ellipsis on line 2 is independent of `word-break` and will land
     mid-word when truncating a long word, which is fine — wraps stay
     clean, only the visible truncation cuts mid-word. */
  word-break: normal;
  overflow-wrap: break-word;
}
</style>
