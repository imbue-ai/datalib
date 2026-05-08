<script setup lang="ts">
import { ref, watch, onMounted, computed } from "vue";
import { useRouter } from "vue-router";
import { AgGridVue } from "ag-grid-vue3";
import {
  ModuleRegistry,
  AllCommunityModule,
  themeQuartz,
  colorSchemeVariable,
  type ColDef,
  type GridOptions,
  type ICellRendererParams,
} from "ag-grid-community";
import {
  fetchAccounts,
  fetchHealth,
  fetchSearch,
  type AccountsMap,
  type Health,
  type SearchRow,
} from "@/api";

ModuleRegistry.registerModules([AllCommunityModule]);

const gridTheme = themeQuartz.withPart(colorSchemeVariable);

const router = useRouter();
const query = ref("");
const rows = ref<SearchRow[]>([]);
const total = ref(0);
const loading = ref(false);
const error = ref<string | null>(null);
const health = ref<Health | null>(null);
const accounts = ref<AccountsMap>({});

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
    const r = await fetchSearch(q, 1000, inflight.signal);
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
  runSearch("");
});

function openRow(row: SearchRow) {
  router.push({
    name: "chat",
    params: { conversationUuid: row.conversation_uuid },
    hash: row.message_index != null ? `#m${row.message_index}` : undefined,
  });
}

const columnDefs = computed<ColDef<SearchRow>[]>(() => [
  { field: "source", headerName: "Source", width: 110 },
  { field: "kind", headerName: "Type", width: 130 },
  {
    field: "when",
    headerName: "Time",
    width: 180,
    sort: "desc",
  },
  {
    field: "snippet",
    headerName: "Contents",
    flex: 1,
    minWidth: 280,
    wrapText: true,
    autoHeight: true,
    cellStyle: { whiteSpace: "normal", lineHeight: "1.3em" },
  },
  {
    field: "author",
    headerName: "Author",
    width: 160,
    valueFormatter: (p) => {
      const v = p.value as string | undefined;
      if (!v) return "";
      return accounts.value[v]?.label ?? v;
    },
  },
  {
    field: "account",
    headerName: "Account",
    width: 200,
    valueFormatter: (p) => accountLabel(p.value as string),
  },
  {
    headerName: "Open",
    width: 80,
    sortable: false,
    filter: false,
    cellRenderer: (params: ICellRendererParams<SearchRow>) => {
      const btn = document.createElement("button");
      btn.className = "open-btn";
      btn.title = "Open";
      btn.textContent = "→";
      btn.addEventListener("click", (ev) => {
        ev.stopPropagation();
        if (params.data) openRow(params.data);
      });
      return btn;
    },
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
  // Allow click-and-drag selection of cell text so users can Cmd+C the
  // contents of a row. Without this, AG Grid treats clicks as row
  // selection and the browser never gets a text selection to copy.
  enableCellTextSelection: true,
  ensureDomOrder: true,
  onRowDoubleClicked: (e) => {
    if (e.data) openRow(e.data);
  },
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
    <p v-else-if="!loading && rows.length === 0 && !error" class="empty">no matches.</p>
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
.grid-wrap {
  flex: 1 1 auto;
  min-height: 300px;
}
.grid {
  width: 100%;
  height: 100%;
}
:deep(.open-btn) {
  background: transparent;
  color: inherit;
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  padding: 0.1rem 0.5rem;
  cursor: pointer;
  font-size: 1rem;
}
:deep(.open-btn:hover) {
  background: var(--fw-hover);
}
</style>
