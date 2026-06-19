<script setup lang="ts">
// Search-grid card: a search bar + AG Grid over /api/search results.
//
// Selecting a row opens the row's document as a new card via
// ctx.host.openCards — structural changes never go through the bus.
// Double-clicking a row opens that document as a standalone
// single-column page in a new tab.
//
// Persistence: the card owns an opaque state string (see
// HostCommands.setState) holding URLSearchParams of
//   q    — the search query
//   sel  — the selected row uuid
//   cols — AG Grid column state, base64url-encoded JSON
// The host puts it in this column's URL segment; on load it comes
// back via ctx.initialState.
import { computed, nextTick, onMounted, ref, watch } from "vue";
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
  fetchSearch,
  type AccountsMap,
  type SearchRow,
} from "@/api";
import FeedbackModal from "@/components/FeedbackModal.vue";
import { buildContext, type FeedbackContext } from "@/feedback/context";
import claudeIconUrl from "@/assets/claude.svg";
import chatgptIconUrl from "@/assets/chatgpt.svg";
import slackIconUrl from "@/assets/slack.svg";
import githubIconUrl from "@/assets/github.svg";
import gitlabIconUrl from "@/assets/gitlab.svg";
import notionIconUrl from "@/assets/notion.svg";
import whatsappIconUrl from "@/assets/whatsapp.svg";
import signalIconUrl from "@/assets/signal.svg";
import emailIconUrl from "@/assets/email.svg";
import smsIconUrl from "@/assets/sms.svg";
import linkedinIconUrl from "@/assets/linkedin.svg";
import { encodeColumns } from "@/router/columns";
import type { CardCtx } from "./types";

const SOURCE_ICONS: Record<string, string> = {
  Claude: claudeIconUrl,
  ChatGPT: chatgptIconUrl,
  Slack: slackIconUrl,
  GitHub: githubIconUrl,
  GitLab: gitlabIconUrl,
  Notion: notionIconUrl,
  WhatsApp: whatsappIconUrl,
  Signal: signalIconUrl,
  Mail: emailIconUrl,
  SMS: smsIconUrl,
  LinkedIn: linkedinIconUrl,
};

ModuleRegistry.registerModules([AllCommunityModule, AllEnterpriseModule]);

const gridTheme = themeQuartz.withPart(colorSchemeVariable);

const props = defineProps<{
  ctx: CardCtx;
  // Initial query from the card source (`gridView({q: "…"})`); the
  // persisted state's `q` wins over it when present.
  q?: string;
}>();

const initialState = new URLSearchParams(props.ctx.initialState);

const query = ref(initialState.get("q") ?? props.q ?? "");
const rows = ref<SearchRow[]>([]);
const total = ref(0);
const loading = ref(false);
const error = ref<string | null>(null);
// qmd-routed search failed at runtime; backend served LIKE-based
// fallback rows. Surface as a banner so users notice the degradation
// instead of silently getting worse results.
const qmdError = ref<string | null>(null);
const accounts = ref<AccountsMap>({});
const selectedRow = ref<SearchRow | null>(null);
// Selected row uuid as persisted state — survives reloads so the
// deep-linked column highlights the same row.
const sel = ref<string | null>(initialState.get("sel"));

// AG Grid handle for applying / reading column state. Set by onGridReady.
let gridApi: GridApi<SearchRow> | null = null;

// Suppress state writes (and column-open side effects) while we're
// applying state from the URL ourselves — otherwise the grid's
// column/selection events would clobber the state we just read, and
// restoring a selection would open a duplicate document column.
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

// Latest encoded column state; null while the columns are still at
// their defaults (so a pristine grid serializes to a short segment).
let colsEncoded: string | null = initialState.get("cols");

function saveState() {
  if (restoring) return;
  const params = new URLSearchParams();
  if (query.value) params.set("q", query.value);
  if (sel.value) params.set("sel", sel.value);
  if (colsEncoded) params.set("cols", colsEncoded);
  props.ctx.host.setState(params.toString());
}

// Reflect any user-driven column change (resize / sort / move /
// visibility) into the persisted state. Skipped during programmatic
// mutation (`restoring`).
function updateCols() {
  if (restoring || !gridApi) return;
  const state = gridApi.getColumnState();
  colsEncoded = state.length > 0 ? encodeColumnState(state) : null;
  saveState();
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
// right-clicked even if selection changed since.
const contextCellInfo = ref<{ column: string; cellValue: string } | null>(null);

// Feedback modal state
const feedbackOpen = ref(false);
const feedbackContext = ref<FeedbackContext | null>(null);
const feedbackSurfaceLabel = ref("");

// Filter context for a right-clicked cell — built on the fly inside
// `getContextMenuItems`. Null for non-filterable columns (Time,
// Contents) or rows with no value in the clicked column.
type FilterCtx = {
  // Query-language key (e.g. "source", "channel"); maps to a backend Field.
  key: string;
  // Human-facing column header for menu labels.
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
    v === "" || /[\s:"]/.test(v) || v.startsWith("-") || v.startsWith('"');
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
// AG Grid's row virtualization.
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
  saveState();
});

// Restore the selected row from persisted state after rows load (or
// after the grid first becomes ready, whichever happens last —
// onGridReady can race with the initial fetch). Selection state
// outlives the result set: searches that drop the selected row leave
// selection cleared, which is the right behavior for a deep-link.
async function tryRestoreSelection() {
  const target_sel = sel.value;
  if (!target_sel || !gridApi || rows.value.length === 0) return;
  if (selectedRow.value && rowKey(selectedRow.value) === target_sel) return;
  const target = rows.value.find((r) => rowKey(r) === target_sel);
  if (!target) return;
  // AG Grid creates row nodes from rowData asynchronously after Vue
  // pushes the data. Wait one tick so forEachNode actually sees them.
  await nextTick();
  if (!gridApi) return;
  restoring = true;
  let found = false;
  gridApi.forEachNode((node) => {
    if (node.data && rowKey(node.data) === target_sel) {
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
  // ensureIndexVisible needs the post-sort row order to be computed
  // AND the new rowData to be ingested by AG Grid's virtualizer. We
  // listen for the grid's own `rowDataUpdated` event, which fires once
  // the new rows are in place; the listener runs at most once per
  // applyDefaultSort call.
  if (sel.value) {
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
// with varying values get shown. "Adaptive rule wins" — manual
// column-visibility toggles get overwritten on the next query.
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
    accounts.value = await fetchAccounts();
  } catch {
    /* accounts mapping is best-effort */
  }
  runSearch(query.value);
});

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
  // they cost nothing when unused. Object form (not the "columns"
  // shorthand) so no `defaultToolPanel` is set and the side bar starts
  // collapsed — just the tab strip.
  rowGroupPanelShow: "always",
  sideBar: { toolPanels: ["columns"] },
  // `preventDefaultOnContextMenu: true` makes AG Grid call
  // preventDefault() synchronously on the contextmenu event so the
  // browser's native menu never shows over the grid's. Our
  // app-specific entries are prepended to AG Grid's defaults via
  // `getContextMenuItems` below.
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
    // Optional "Filter by Notion Page" entry, populated when the right-
    // clicked row has a non-empty `notion_page_uuid`. Lets users zoom into
    // all rows on a single Notion page from any cell of any row on that
    // page — useful because the page UUID isn't always the same as
    // conversation_uuid (e.g. comment threads use the discussion UUID).
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
  // multiRow so right-click "Copy UUID(s)" can target several rows, like
  // Lightroom. Single-click still narrows to one row; the document column
  // follows whichever row was most recently toggled on.
  rowSelection: { mode: "multiRow", checkboxes: false, enableClickSelection: true },
  ensureDomOrder: true,
  getRowId: (p: GetRowIdParams<SearchRow>) => p.data.uuid,
  onGridReady: (e: GridReadyEvent<SearchRow>) => {
    gridApi = e.api;
    // Expose the grid api so e2e tests can scroll virtualized rows
    // into view before clicking. Last grid card wins when several are
    // open — fine for tests, which drive a single grid.
    (window as unknown as { __fwGridApi?: GridApi<SearchRow> }).__fwGridApi =
      e.api;
    if (colsEncoded) {
      const state = decodeColumnState(colsEncoded);
      if (state) {
        restoring = true;
        gridApi.applyColumnState({ state, applyOrder: true });
        restoring = false;
        // An explicit persisted column state carries the user's sort
        // choice — don't clobber it with our default.
        if (state.some((c) => c.sort != null)) userSortedManually = true;
      }
    }
    // Rows may already be loaded by the time the grid is ready.
    applyDefaultSort();
    tryRestoreSelection();
  },
  onRowSelected: (e: RowSelectedEvent<SearchRow>) => {
    if (!e.node.isSelected() || !e.data) return;
    selectedRow.value = e.data;
    sel.value = rowKey(e.data);
    // `restoring` is true when this is a URL-driven re-selection — the
    // document column is already in the URL, so don't open a duplicate
    // (and don't rewrite the state we just read).
    if (restoring) return;
    saveState();
    const md = e.data.markdown_uuid ?? e.data.uuid;
    props.ctx.host.openCards(docSource(md, e.data.uuid));
  },
  onRowDoubleClicked: (e) => {
    if (e.data) openRow(e.data);
  },
  onCellContextMenu: (e: CellContextMenuEvent<SearchRow>) => {
    if (!gridApi) return;
    const me = e.event as MouseEvent | null;
    contextAnchorEl.value = me?.target instanceof Element ? me.target : null;
    // Snapshot the cell value for the eventual "Feedback…" action. The
    // `e.value` here is the displayed value (post-valueFormatter), which
    // is closer to what the user actually sees (e.g. author UUID → label)
    // than the raw row field.
    const colId = e.column?.getColId() ?? "";
    const v = e.value;
    const cellRendered = typeof v === "string" ? v : v == null ? "" : String(v);
    contextCellInfo.value = { column: colId, cellValue: cellRendered };
    // Lightroom: right-clicking an unselected row narrows selection to it.
    if (e.node && !e.node.isSelected()) {
      gridApi.deselectAll();
      e.node.setSelected(true);
    }
  },
  // Any change a USER can make to columns gets reflected in the
  // persisted state. Filtered by event source: the grid also fires
  // these events for its own layout work (flex sizing on load,
  // adaptive visibility, programmatic default sort), and persisting
  // those would stamp a column-state blob into the URL of a grid the
  // user never touched.
  onColumnVisible: (e) => {
    if (e.source === "toolPanelUi" || e.source === "contextMenu") updateCols();
  },
  onColumnResized: (e) => {
    if (e.finished && e.source === "uiColumnResized") updateCols();
  },
  onColumnMoved: (e) => {
    if (e.finished && e.source === "uiColumnMoved") updateCols();
  },
  onColumnRowGroupChanged: () => updateCols(),
  onSortChanged: (e) => {
    if (restoring || e.source !== "uiColumnSorted") return;
    userSortedManually = true;
    updateCols();
  },
};
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

    <div class="status">{{ rows.length }} rows (of {{ total }})</div>

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
.status {
  font-size: 0.85rem;
  color: var(--fw-muted);
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
  /* Fill the positioned .grid-wrap absolutely instead of with
     height:100%. WebKit (Safari + the Tauri WKWebView) resolves a
     percentage height against a flex-sized parent with no explicit
     height as `auto`, collapsing the grid to its row-group panel
     (~50px) while Chromium gives it the full flexed height — the
     Chromium-only e2e suite never sees this. */
  position: absolute;
  inset: 0;
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
