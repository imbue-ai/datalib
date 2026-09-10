<script setup lang="ts">
// The picker behind a filter field whose values a probe can enumerate:
// an account's mailboxes, an account's conversations. One scrollable
// grid with a checkbox per row, and the checked set *is* the field's
// value — the text box beside it edits the same array.
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

/// A conversation is named by its title and dated; a mailbox is named
/// by the very string the filter matches, so it needs no second column
/// for it.
const isConversations = computed(() => props.items.some((i) => i.kind === "conversation"));

const columnDefs = computed<ColDef<ProbeItem>[]>(() =>
  isConversations.value
    ? [
        {
          headerName: "Conversation",
          flex: 1,
          minWidth: 220,
          tooltipValueGetter: (p) => p.data?.path ?? "",
          valueGetter: (p: ValueGetterParams<ProbeItem>) => p.data?.title || p.data?.path || "",
        },
        {
          headerName: "Updated",
          width: 118,
          // The date the source stamped, not one re-derived in this
          // browser's zone — see AGENTS.md's timestamp convention.
          valueGetter: (p: ValueGetterParams<ProbeItem>) => p.data?.updated_at?.slice(0, 10) ?? "",
        },
      ]
    : [
        { headerName: "Label", field: "path", flex: 1, minWidth: 200 },
        { headerName: "Role", field: "role", width: 110 },
        { headerName: "Messages", field: "messages", width: 110, type: "numericColumn" },
      ],
);

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
      :placeholder="isConversations ? 'Search these conversations…' : 'Search these labels…'"
    />
    <AgGridVue
      class="pick-grid"
      :theme="gridTheme"
      :columnDefs="columnDefs"
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
