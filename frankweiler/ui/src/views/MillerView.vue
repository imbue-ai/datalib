<script setup lang="ts">
// The one and only top-level view. Hosts a horizontally-scrolling
// stack of "columns", each of which is either a `GridColumn` (the
// search bar + AG Grid) or a `DocColumn` (one rendered markdown
// document). Both column kinds are first-class components instantiated
// here; the grid no longer has special-cased layout above the
// columns.
//
// The URL path *is* the column stack — `/` is empty (rendered as a
// default `[grid]`), `/grid:q=foo/doc:abc` is the corresponding two-
// column stack, and so on. See `router/columns.ts` for the encoding.
// MillerView is the single `router.replace` writer; `GridColumn`
// exposes its own state (q / sel / agCols) via v-model-style emits
// that we fold into the corresponding column descriptor and re-emit
// as a URL write.

import { ref, computed, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import { type SearchRow } from "@/api";
import GridColumn from "@/components/GridColumn.vue";
import DocColumn from "@/components/DocColumn.vue";
import {
  type Column,
  emptyGrid,
  encodeStack,
  decodeStack,
  stacksEqual,
} from "@/router/columns";

const route = useRoute();
const router = useRouter();

// Source of truth for the column stack. Initialized from the path
// (defaulting to `[grid]` when the path is empty / `/`) and kept in
// sync with the URL by `syncUrl()` for outbound writes and the
// `route.path` watcher for inbound (back/forward) navigation.
const columns = ref<Column[]>(initialColumns());

function initialColumns(): Column[] {
  const parsed = decodeStack(route.path);
  return parsed.length === 0 ? [emptyGrid()] : parsed;
}

// The currently-selected grid row, when a GridColumn is present. Drives
// section highlighting in the doc column directly to its right.
const selectedGridRow = ref<SearchRow | null>(null);

// Map our `columns` array into what the template renders. Plain ref
// is the SOT; this computed exists only so the template's `:key`
// expression can derive a stable identity per slot.
const renderedColumns = computed(() => columns.value);

function syncUrl() {
  const path = encodeStack(columns.value);
  if (path !== route.path) {
    router.replace(path);
  }
}

// Update one column in-place and write the URL. `mutator` returns
// the new column descriptor; if it's reference-equal to the existing
// entry we skip the write.
function updateColumn(index: number, mutator: (c: Column) => Column) {
  const cur = columns.value[index];
  if (!cur) return;
  const next = mutator(cur);
  if (next === cur) return;
  const arr = columns.value.slice();
  arr[index] = next;
  columns.value = arr;
  syncUrl();
}

// Truncate-and-push: a click in column `parentIndex` opens `uuid` as
// column `parentIndex + 1`, discarding any columns past that point.
function pushColumn(parentIndex: number, md: string) {
  const next = columns.value.slice(0, parentIndex + 1);
  next.push({ kind: "doc", md });
  columns.value = next;
  syncUrl();
}

function onSelectRow(index: number, row: SearchRow, restoring: boolean) {
  selectedGridRow.value = row;
  const sel = row.uuid;
  // Update the grid column's `sel` field in-place.
  updateColumn(index, (c) => {
    if (c.kind !== "grid") return c;
    if (c.sel === sel) return c;
    return { ...c, sel };
  });
  if (restoring) {
    // URL-driven restoration: deeper columns were already seeded
    // from the path. Don't clobber them.
    return;
  }
  // User-driven selection: the row's markdown becomes the column
  // immediately to the right; everything further right is discarded
  // (classic miller truncate-and-push).
  const md = row.markdown_uuid;
  const head = columns.value.slice(0, index + 1);
  columns.value = md ? [...head, { kind: "doc", md }] : head;
  syncUrl();
}

function onUpdateQ(index: number, q: string) {
  updateColumn(index, (c) => {
    if (c.kind !== "grid") return c;
    if (c.q === q) return c;
    return { ...c, q };
  });
}

function onUpdateAgCols(index: number, agCols: string | null) {
  updateColumn(index, (c) => {
    if (c.kind !== "grid") return c;
    if (c.agCols === agCols) return c;
    return { ...c, agCols };
  });
}

// Section highlighting only fires in the doc column immediately to
// the right of a grid column whose selected row actually points at
// this doc. (Selecting a different row would already have truncated
// any docs further right.)
function selectedSectionUuidFor(col: Column, index: number): string | null {
  if (col.kind !== "doc") return null;
  if (index === 0) return null;
  const prev = columns.value[index - 1];
  if (!prev || prev.kind !== "grid") return null;
  const row = selectedGridRow.value;
  if (!row || row.kind === "Chat") return null;
  if (row.markdown_uuid !== col.md) return null;
  return row.uuid;
}

// Reflect external path changes (back/forward, parent-driven
// rewrites of our own URL) back into `columns`. Compared via
// `stacksEqual` so our own writes don't loop.
watch(
  () => route.path,
  (path) => {
    const parsed = decodeStack(path);
    const next = parsed.length === 0 ? [emptyGrid()] : parsed;
    if (!stacksEqual(next, columns.value)) {
      columns.value = next;
    }
  },
);
</script>

<template>
  <div class="miller-columns">
    <section
      v-for="(col, i) in renderedColumns"
      :key="
        col.kind === 'grid' ? `grid:${i}` : `doc:${i}:${col.md}`
      "
      class="col"
      :class="col.kind === 'grid' ? 'col--grid' : 'col--doc'"
    >
      <GridColumn
        v-if="col.kind === 'grid'"
        :q="col.q"
        :sel="col.sel"
        :ag-cols="col.agCols"
        @select-row="(row, restoring) => onSelectRow(i, row, restoring)"
        @update:q="(q) => onUpdateQ(i, q)"
        @update:ag-cols="(c) => onUpdateAgCols(i, c)"
      />
      <DocColumn
        v-else
        :markdown-uuid="col.md"
        :selected-section-uuid="selectedSectionUuidFor(col, i)"
        @open-chat="(md) => pushColumn(i, md)"
      />
    </section>
  </div>
</template>

<style scoped>
.miller-columns {
  display: flex;
  /* Horizontal scroll once the stack overflows. Columns don't shrink
     — adding one pushes the right edge out and surfaces a horizontal
     scrollbar. */
  overflow-x: auto;
  overflow-y: hidden;
  height: calc(100vh - 6rem);
}
.col {
  display: flex;
  flex-direction: column;
  min-width: 0;
  background: var(--fw-bg);
}
.col + .col {
  border-left: 1px solid var(--fw-border);
}
.col--grid {
  flex: 0 0 720px;
}
.col--doc {
  flex: 0 0 560px;
}
</style>
