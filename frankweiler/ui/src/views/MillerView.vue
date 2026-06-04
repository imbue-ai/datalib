<script setup lang="ts">
// The one and only top-level view. Hosts a horizontally-scrolling
// stack of "columns", each of which is either a `GridColumn` (the
// search bar + AG Grid) or a `DocColumn` (one rendered markdown
// document). Both column kinds are first-class components instantiated
// here; the grid no longer has special-cased layout above the
// columns.
//
// The default route (`search`) seeds the stack with `[grid]`. The
// `chat` route — used by the "view this column alone ↗" affordance —
// seeds the stack with a single `doc:<uuid>` column, no grid. From
// either starting point the stack can grow arbitrarily deep as the
// user clicks chat-links inside documents.

import { ref, computed, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import { type SearchRow } from "@/api";
import GridColumn, { type GridUrlState } from "@/components/GridColumn.vue";
import DocColumn from "@/components/DocColumn.vue";

type Column =
  | { kind: "grid" }
  | { kind: "doc"; markdownUuid: string };

const route = useRoute();
const router = useRouter();

// The first column comes from the route name. Everything past it
// lives in the URL hash as `?docs=u1,u2,...` (additional documents
// pushed by inner link clicks). Grid-row selection rewrites only the
// first doc entry, leaving the route-driven head untouched.
function initialHead(): Column {
  if (route.name === "chat") {
    const md = route.params.markdownUuid;
    if (typeof md === "string" && md.length > 0) {
      return { kind: "doc", markdownUuid: md };
    }
  }
  return { kind: "grid" };
}

function parseDocsParam(raw: unknown): string[] {
  if (typeof raw !== "string" || raw.length === 0) return [];
  return raw.split(",").filter((s) => s.length > 0);
}

// Source of truth for additional document columns past the
// route-driven head. Length 0 = only the head column is visible.
const extraDocs = ref<string[]>(parseDocsParam(route.query.docs));

// The currently-selected grid row, when a GridColumn is present. Drives
// section highlighting in the doc column directly to its right.
const selectedGridRow = ref<SearchRow | null>(null);

// Mirror of GridColumn's URL-persisted state. The grid pushes this
// up via `update-url-state`; we merge it with our own `docs` and
// write the URL atomically. Single writer avoids the race two
// independent `router.replace` callers would cause (only the second
// wins, dropping the first's keys).
const gridUrlState = ref<GridUrlState>({
  q: typeof route.query.q === "string" ? route.query.q : null,
  sel: typeof route.query.sel === "string" ? route.query.sel : null,
  cols: typeof route.query.cols === "string" ? route.query.cols : null,
});

// Flatten the head + extras into a single Column[] for rendering.
const columns = computed<Column[]>(() => {
  const head = initialHead();
  const tail: Column[] = extraDocs.value.map((md) => ({
    kind: "doc",
    markdownUuid: md,
  }));
  return [head, ...tail];
});

// Write everything we know about the URL state in one atomic
// `router.replace`. Combines `extraDocs` (our own field) with
// whatever `GridColumn` last told us about `q`/`sel`/`cols`.
function syncUrl() {
  const merged: Record<string, string> = {};
  const g = gridUrlState.value;
  if (g.q) merged.q = g.q;
  if (g.sel) merged.sel = g.sel;
  if (g.cols) merged.cols = g.cols;
  if (extraDocs.value.length > 0) {
    merged.docs = extraDocs.value.join(",");
  }
  router.replace({
    name: String(route.name ?? "search"),
    params: route.params,
    query: merged,
  });
}

function onGridUrlState(state: GridUrlState) {
  gridUrlState.value = state;
  syncUrl();
}

// Truncate-and-push: a click in column `parentIndex` opens `uuid` as
// column `parentIndex + 1`, discarding any columns past that point.
// `parentIndex` is 0-indexed across all columns (head + extras). The
// head lives at extraDocs index -1, so the slice point into
// `extraDocs` is `parentIndex` itself (anything at index >=
// parentIndex is discarded; the new entry then lands at
// extraDocs[parentIndex]).
function pushColumn(parentIndex: number, uuid: string) {
  // The head column lives at parentIndex 0. extraDocs[i] lives at
  // parentIndex i+1. So pushing as a child of column `parentIndex`
  // means the new entry lands at extraDocs[parentIndex]; everything
  // at index >= parentIndex in extraDocs gets discarded.
  const next = extraDocs.value.slice(0, parentIndex);
  next.push(uuid);
  extraDocs.value = next;
  syncUrl();
}

function onSelectRow(row: SearchRow, restoring: boolean) {
  selectedGridRow.value = row;
  if (restoring) {
    // URL-driven restoration: keep the already-seeded extraDocs
    // intact. The grid selection event is just re-asserting state
    // we read from the hash. (The grid's own `update-url-state`
    // emit will still fire and we'll merge those into the URL.)
    return;
  }
  // User-driven selection: the row's markdown becomes column 1 and
  // everything past it is discarded (classic miller truncate-and-push
  // from the grid). The URL write happens via `update-url-state`
  // from GridColumn, which fires right after this emit returns.
  const md = row.markdown_uuid;
  extraDocs.value = md ? [md] : [];
}

// Section highlighting only fires in the doc column immediately to
// the right of the grid, and only when that doc actually corresponds
// to the selected grid row. (Selecting a row in a different
// conversation would have already truncated extras to `[newMd]`, so
// in practice this matches `extraDocs[0]`.)
function selectedSectionUuidFor(col: Column, index: number): string | null {
  if (col.kind !== "doc") return null;
  if (index !== 1) return null;
  const row = selectedGridRow.value;
  if (!row || row.kind === "Chat") return null;
  if (row.markdown_uuid !== col.markdownUuid) return null;
  return row.uuid;
}

// When the route changes (e.g. the in-app drawer navigates between
// `search` and `chat`), reset the column stack from the URL. Vue
// Router reuses the same component instance across these routes, so
// without this watch the old extraDocs would leak across.
watch(
  () => [route.name, route.params.markdownUuid, route.query.docs] as const,
  ([, , docs]) => {
    const parsed = parseDocsParam(docs);
    // Avoid setting if unchanged — otherwise we'd trigger a write-back
    // in syncUrl() and loop.
    if (
      parsed.length !== extraDocs.value.length ||
      parsed.some((v, i) => v !== extraDocs.value[i])
    ) {
      extraDocs.value = parsed;
    }
  },
);
</script>

<template>
  <div class="miller-columns">
    <section
      v-for="(col, i) in columns"
      :key="
        col.kind === 'grid' ? `grid:${i}` : `doc:${i}:${col.markdownUuid}`
      "
      class="col"
      :class="col.kind === 'grid' ? 'col--grid' : 'col--doc'"
    >
      <GridColumn
        v-if="col.kind === 'grid'"
        @select-row="onSelectRow"
        @update-url-state="onGridUrlState"
      />
      <DocColumn
        v-else
        :markdown-uuid="col.markdownUuid"
        :selected-section-uuid="selectedSectionUuidFor(col, i)"
        @open-chat="(uuid) => pushColumn(i, uuid)"
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
