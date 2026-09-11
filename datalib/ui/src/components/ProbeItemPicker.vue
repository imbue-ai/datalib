<script setup lang="ts">
// The picker behind a filter field whose values a probe can enumerate:
// an account's mailboxes, its conversations, a workspace's channels,
// the people behind its DMs. One scrollable grid with a checkbox per
// row, and the checked set *is* the field's value — the text box
// beside it edits the same array. What the columns are follows from
// what kind of item the list holds; nothing else differs per source.
//
// A value the probe has never heard of is kept rather than unchecked
// away: it is how a hand-typed label survives touching this grid, and
// the field says separately that the account does not have it.
import { computed, ref, watch } from "vue";
import { AgGridVue } from "ag-grid-vue3";
import {
  AllCommunityModule,
  ModuleRegistry,
  colorSchemeVariable,
  themeQuartz,
  type ColDef,
  type GridApi,
  type GridReadyEvent,
  type ValueGetterParams,
} from "ag-grid-community";
import type { ProbeItem } from "@/api";

ModuleRegistry.registerModules([AllCommunityModule]);
const gridTheme = themeQuartz.withPart(colorSchemeVariable);

const props = defineProps<{
  items: ProbeItem[];
  /// The field's array: every value chosen, probed or typed.
  modelValue: string[];
}>();
const emit = defineEmits<{ (e: "update:modelValue", value: string[]): void }>();

const api = ref<GridApi<ProbeItem> | null>(null);
const query = ref("");

/// The item's name as a person reads it: the title where the path is
/// an opaque id, the path itself where it is its own name.
const byTitle = (p: ValueGetterParams<ProbeItem>) => p.data?.title || p.data?.path || "";
/// The date the source stamped, not one re-derived in this browser's
/// zone — see AGENTS.md's timestamp convention.
const byDate = (p: ValueGetterParams<ProbeItem>) => p.data?.updated_at?.slice(0, 10) ?? "";
const idTooltip = (p: { data?: ProbeItem }) => p.data?.path ?? "";

/// One column set per item kind. A mailbox is named by the very string
/// the filter matches, so it needs no second column for it; a
/// conversation or a person is named by a title over an opaque id.
const COLUMNS: Record<string, { placeholder: string; columns: ColDef<ProbeItem>[] }> = {
  mailbox: {
    placeholder: "Search these labels…",
    columns: [
      { headerName: "Label", field: "path", flex: 1, minWidth: 200 },
      { headerName: "Role", field: "role", width: 110 },
      { headerName: "Messages", field: "messages", width: 110, type: "numericColumn" },
    ],
  },
  conversation: {
    placeholder: "Search these conversations…",
    columns: [
      {
        headerName: "Conversation",
        flex: 1,
        minWidth: 220,
        tooltipValueGetter: idTooltip,
        valueGetter: byTitle,
      },
      { headerName: "Updated", width: 118, valueGetter: byDate },
    ],
  },
  channel: {
    placeholder: "Search these channels…",
    columns: [
      {
        headerName: "Channel",
        flex: 1,
        minWidth: 200,
        valueGetter: (p: ValueGetterParams<ProbeItem>) => (p.data ? `#${p.data.path}` : ""),
      },
      { headerName: "Notes", field: "role", width: 180 },
      { headerName: "Members", field: "members", width: 110, type: "numericColumn" },
    ],
  },
  person: {
    placeholder: "Search these people…",
    columns: [
      {
        headerName: "Person",
        flex: 1,
        minWidth: 200,
        tooltipValueGetter: idTooltip,
        valueGetter: byTitle,
      },
      { headerName: "Handle", field: "role", width: 150 },
    ],
  },
};

/// Every list a probe returns is one kind throughout, `labels` being
/// the exception: it mixes mailboxes with keywords, and both read as a
/// label. So the first item's kind decides, and a keyword reads as a
/// mailbox.
const layout = computed(() => {
  const kind = props.items[0]?.kind ?? "mailbox";
  return COLUMNS[kind === "keyword" ? "mailbox" : kind] ?? COLUMNS.mailbox;
});

/// Set while this component is writing the grid's selection from
/// `modelValue`, so the resulting event doesn't echo straight back.
let applying = false;

function applySelection() {
  const grid = api.value;
  if (!grid) return;
  const chosen = new Set(props.modelValue);
  applying = true;
  grid.forEachNode((node) => node.setSelected(!!node.data && chosen.has(node.data.path)));
  applying = false;
}

function onSelectionChanged() {
  const grid = api.value;
  if (!grid || applying) return;
  const listed = new Set(props.items.map((i) => i.path));
  const picked = grid.getSelectedRows().map((r) => r.path);
  // Values from outside this list ride along untouched.
  const kept = props.modelValue.filter((v) => !listed.has(v));
  emit("update:modelValue", [...picked, ...kept]);
}

function onGridReady(e: GridReadyEvent<ProbeItem>) {
  api.value = e.api;
  applySelection();
}

watch(() => [props.items, props.modelValue], applySelection, { deep: true });
</script>

<template>
  <div class="pick">
    <input
      v-model="query"
      class="pick-filter"
      type="search"
      :placeholder="layout.placeholder"
    />
    <AgGridVue
      class="pick-grid"
      :theme="gridTheme"
      :columnDefs="layout.columns"
      :rowData="items"
      :getRowId="(p: { data: ProbeItem }) => p.data.path"
      :quickFilterText="query"
      :rowSelection="{ mode: 'multiRow', checkboxes: true, headerCheckbox: true }"
      :rowHeight="28"
      :headerHeight="30"
      :suppressCellFocus="true"
      @grid-ready="onGridReady"
      @selection-changed="onSelectionChanged"
    />
  </div>
</template>

<style scoped>
.pick { display: flex; flex-direction: column; gap: 6px; }
.pick-filter {
  width: 100%;
  padding: 6px 8px;
  border: 1px solid var(--datalib-border);
  border-radius: 5px;
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
  font: inherit;
  box-sizing: border-box;
}
/* Tall enough to show that the account really is being read, short
   enough that the rest of the form stays on screen. The grid scrolls
   inside it. */
.pick-grid { height: 260px; width: 100%; }
</style>
