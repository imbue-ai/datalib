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
import claudeIconUrl from "@/assets/claude.svg";
import chatgptIconUrl from "@/assets/chatgpt.svg";
import slackIconUrl from "@/assets/slack.svg";

const SOURCE_ICONS: Record<string, string> = {
  Claude: claudeIconUrl,
  ChatGPT: chatgptIconUrl,
  Slack: slackIconUrl,
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
const health = ref<Health | null>(null);
const accounts = ref<AccountsMap>({});
const selectedRow = ref<SearchRow | null>(null);

// AG Grid handle for applying / reading column state. Set by onGridReady.
let gridApi: GridApi<SearchRow> | null = null;

// Suppress hash writes while we're applying state from the URL ourselves
// — otherwise the grid's column-events would clobber the URL we just read.
let restoring = false;

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

function closeContextMenu() {
  contextMenuVisible.value = false;
  contextMenuTargets.value = [];
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

async function runSearch(q: string) {
  inflight?.abort();
  inflight = new AbortController();
  loading.value = true;
  error.value = null;
  try {
    const r = await fetchSearch(q, 100_000, inflight.signal);
    rows.value = r.rows;
    total.value = r.total_estimated;
  } catch (e) {
    if ((e as { name?: string }).name === "AbortError") return;
    error.value = (e as Error).message;
  } finally {
    loading.value = false;
  }
}

watch(query, (q) => {
  if (debounceTimer) clearTimeout(debounceTimer);
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

watch(rows, tryRestoreSelection);

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
    sort: "desc",
  },
  {
    field: "snippet",
    headerName: "Contents",
    flex: 1,
    minWidth: 200,
    wrapText: true,
    autoHeight: true,
    cellStyle: { whiteSpace: "normal", lineHeight: "1.3em" },
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
  rowHeight: 56,
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
      }
    }
    // Rows may already be loaded by the time the grid is ready.
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
    }
    // Lightroom: right-clicking an unselected row narrows selection to it.
    if (!e.node.isSelected()) {
      gridApi.deselectAll();
      e.node.setSelected(true);
    }
    contextMenuTargets.value = targets;
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
  onSortChanged: () => syncHash(),
};
</script>

<template>
  <section class="search-view">
    <input
      v-model="query"
      placeholder="search messages…  (try: type:chat, account:…, before:2025-01-01)"
      class="search-input"
      data-testid="search-input"
      autofocus
    />

    <div v-if="health" class="health">
      backend ok · {{ total }} conversations indexed under
      <code>{{ health.root }}</code>
      <span v-if="!health.root_exists" class="warn"> (root does not exist)</span>
    </div>

    <p v-if="error" class="error">error: {{ error }}</p>

    <Splitpanes class="split" :dbl-click-splitter="false">
      <Pane size="55" min-size="25" class="left-pane">
        <div class="grid-wrap">
          <AgGridVue
            class="grid"
            :rowData="rows"
            :columnDefs="columnDefs"
            :defaultColDef="defaultColDef"
            :gridOptions="gridOptions"
          />
        </div>
        <p v-if="loading && rows.length === 0" class="empty">searching…</p>
        <p v-else-if="!loading && rows.length === 0 && !error" class="empty">
          no matches.
        </p>
      </Pane>
      <Pane size="45" min-size="20" class="right-pane">
        <ChatPreviewPane
          :conversation-uuid="selectedRow?.conversation_uuid ?? null"
          :message-index="selectedRow?.message_index ?? null"
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
      </div>
    </div>
  </section>
</template>

<style scoped>
.search-view {
  display: flex;
  flex-direction: column;
  height: calc(100vh - 6rem);
  gap: 0.5rem;
}
.search-input {
  width: 100%;
  padding: 0.5rem 0.75rem;
  font-size: 1rem;
  box-sizing: border-box;
  background: var(--fw-input-bg);
  color: var(--fw-fg);
  border: 1px solid var(--fw-border);
  border-radius: 4px;
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
}
.grid {
  width: 100%;
  height: 100%;
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
</style>
