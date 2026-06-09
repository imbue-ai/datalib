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
import CardColumn from "@/components/CardColumn.vue";
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

// Per-column widths (px), indexed by position in `columns`. Entries
// stay `null` until the user resizes that slot; a null slot renders at
// the kind's default. Not persisted across reloads — widths live in
// memory only.
const DEFAULT_WIDTH: Record<Column["kind"], number> = {
  grid: 720,
  doc: 560,
  card: 560,
};
const MIN_WIDTH = 240;
const widths = ref<(number | null)[]>(columns.value.map(() => null));

function widthFor(i: number): number {
  const w = widths.value[i];
  if (w != null) return w;
  const col = columns.value[i];
  return col ? DEFAULT_WIDTH[col.kind] : DEFAULT_WIDTH.grid;
}

// Drag a column's right edge to set its width. Captures pointer so
// the move tracks even when the cursor crosses other columns; clamps
// to `MIN_WIDTH` so columns can't collapse to nothing.
function onResizeStart(i: number, ev: PointerEvent) {
  ev.preventDefault();
  const startX = ev.clientX;
  const startWidth = widthFor(i);
  const target = ev.currentTarget as HTMLElement;
  target.setPointerCapture(ev.pointerId);

  const onMove = (e: PointerEvent) => {
    const next = Math.max(MIN_WIDTH, startWidth + (e.clientX - startX));
    const arr = widths.value.slice();
    arr[i] = next;
    widths.value = arr;
  };
  const onUp = (e: PointerEvent) => {
    target.releasePointerCapture(e.pointerId);
    target.removeEventListener("pointermove", onMove);
    target.removeEventListener("pointerup", onUp);
    target.removeEventListener("pointercancel", onUp);
  };
  target.addEventListener("pointermove", onMove);
  target.addEventListener("pointerup", onUp);
  target.addEventListener("pointercancel", onUp);
}

// The currently-selected grid row, when a GridColumn is present. Drives
// section highlighting in the doc column directly to its right.
const selectedGridRow = ref<SearchRow | null>(null);

// The active edge-hover target. Set whenever the user's cursor sits
// on an `.edge-source` span in a `ChatBody` or on a doc-level
// outgoing-edge link in any `DocColumn`. We forward this state to
// every column so the one containing the destination can light up
// (border for whole-doc destinations, span fill for anchor
// destinations). Null when no hover is active.
const hoverEdgeTarget = ref<{ md: string; anchor: string | null } | null>(
  null,
);

function onHoverEdge(target: { md: string; anchor: string | null } | null) {
  hoverEdgeTarget.value = target;
}

function isHoverTarget(col: Column): boolean {
  const t = hoverEdgeTarget.value;
  if (!t) return false;
  if (col.kind !== "doc") return false;
  return col.md === t.md;
}

function hoverAnchorFor(col: Column): string | null {
  const t = hoverEdgeTarget.value;
  if (!t) return null;
  if (col.kind !== "doc") return null;
  if (col.md !== t.md) return null;
  return t.anchor;
}

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
// `anchor` is the doc-level `data-section-uuid` to scroll-and-
// highlight inside the new column. Null for grid-driven navigation
// (the grid's selected row already drives section selection in that
// path); non-null only when an edge click brought us here.
function pushColumn(parentIndex: number, md: string, anchor: string | null) {
  const next = columns.value.slice(0, parentIndex + 1);
  next.push({ kind: "doc", md, anchor });
  columns.value = next;
  syncUrl();
}

// "+ Card" rail on the right edge of column `parentIndex`: truncate
// any deeper columns and push a fresh card. The seed query is whatever
// the parent column knows about — grid and card own a `q`; doc columns
// don't carry one and the card starts blank.
function pushCard(parentIndex: number) {
  const parent = columns.value[parentIndex];
  if (!parent) return;
  const q =
    parent.kind === "grid" || parent.kind === "card" ? parent.q : "";
  const next = columns.value.slice(0, parentIndex + 1);
  next.push({ kind: "card", q, js: null });
  columns.value = next;
  syncUrl();
}

function onUpdateCardQ(index: number, q: string) {
  updateColumn(index, (c) => {
    if (c.kind !== "card") return c;
    if (c.q === q) return c;
    return { ...c, q };
  });
}

function onUpdateCardJs(index: number, js: string | null) {
  updateColumn(index, (c) => {
    if (c.kind !== "card") return c;
    if (c.js === js) return c;
    return { ...c, js };
  });
}

// Pop the rightmost column. Disabled in the template when only one
// column remains — the path watcher would re-seed `[emptyGrid()]` from
// the empty path, which is technically benign but reads as a "delete
// resets to grid" surprise, so we just don't offer the button there.
function popRightmostColumn() {
  if (columns.value.length <= 1) return;
  columns.value = columns.value.slice(0, -1);
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
  columns.value = md ? [...head, { kind: "doc", md, anchor: null }] : head;
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

// Section highlighting fires in either of two paths:
//   1. An edge click landed here — `col.anchor` was seeded by
//      `pushColumn` with the destination edge's `dst_anchor_uuid`.
//   2. The column was opened from a grid row directly to its left
//      and that row points at a non-Chat section inside this doc.
// Path 1 takes precedence because the URL fully describes it.
function selectedSectionUuidFor(col: Column, index: number): string | null {
  if (col.kind !== "doc") return null;
  if (col.anchor) return col.anchor;
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

// Keep `widths` length in lockstep with `columns`. Overlapping prefix
// keeps its user-set widths; truncated slots are dropped; new slots
// start as null (i.e. fall back to the kind's default).
watch(
  () => columns.value.length,
  (len) => {
    if (widths.value.length === len) return;
    const next = widths.value.slice(0, len);
    while (next.length < len) next.push(null);
    widths.value = next;
  },
);
</script>

<template>
  <div class="miller-columns">
    <section
      v-for="(col, i) in renderedColumns"
      :key="
        col.kind === 'grid'
          ? `grid:${i}`
          : col.kind === 'card'
            ? `card:${i}`
            : `doc:${i}:${col.md}`
      "
      class="col"
      :class="
        col.kind === 'grid'
          ? 'col--grid'
          : col.kind === 'card'
            ? 'col--card'
            : 'col--doc'
      "
      :style="{ flexBasis: widthFor(i) + 'px' }"
    >
      <div
        v-if="i === renderedColumns.length - 1 && renderedColumns.length > 1"
        class="col-actions-bar"
      >
        <button
          type="button"
          class="col-delete-btn"
          title="Remove this column"
          data-testid="delete-column"
          @click="popRightmostColumn"
        >
          × Delete
        </button>
      </div>
      <GridColumn
        v-if="col.kind === 'grid'"
        :q="col.q"
        :sel="col.sel"
        :ag-cols="col.agCols"
        @select-row="(row, restoring) => onSelectRow(i, row, restoring)"
        @update:q="(q) => onUpdateQ(i, q)"
        @update:ag-cols="(c) => onUpdateAgCols(i, c)"
      />
      <CardColumn
        v-else-if="col.kind === 'card'"
        :q="col.q"
        :js="col.js"
        @update:q="(q) => onUpdateCardQ(i, q)"
        @update:js="(js) => onUpdateCardJs(i, js)"
      />
      <DocColumn
        v-else
        :markdown-uuid="col.md"
        :selected-section-uuid="selectedSectionUuidFor(col, i)"
        :is-hover-target="isHoverTarget(col)"
        :hover-anchor="hoverAnchorFor(col)"
        @open-chat="(md, anchor) => pushColumn(i, md, anchor)"
        @hover-edge="onHoverEdge"
      />
      <div
        class="col-resize"
        role="separator"
        aria-orientation="vertical"
        @pointerdown="(e) => onResizeStart(i, e)"
      />
    </section>
    <button
      v-if="columns.length > 0"
      type="button"
      class="col-add-card"
      title="Add a card column"
      data-testid="add-card"
      @click="pushCard(columns.length - 1)"
    >
      + Card
    </button>
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
  position: relative;
  display: flex;
  flex-direction: column;
  flex-grow: 0;
  flex-shrink: 0;
  min-width: 0;
  background: var(--fw-bg);
}
.col + .col {
  border-left: 1px solid var(--fw-border);
}
/* The grab strip sits on the column's right edge, slightly wider than
   the 1px visual border so the hit target is comfortable without
   shifting layout. The strip itself stays transparent; a centered
   ::before draws the always-visible grip bars so the affordance is
   obvious without painting the full divider. */
.col-resize {
  position: absolute;
  top: 0;
  right: -3px;
  width: 6px;
  height: 100%;
  cursor: col-resize;
  z-index: 1;
}
.col-resize::before {
  content: "";
  position: absolute;
  left: 50%;
  top: 50%;
  width: 4px;
  height: 28px;
  transform: translate(-50%, -50%);
  /* Two thin parallel bars — the convention for "drag to resize this
     column" in spreadsheets and file managers. */
  border-left: 1px solid var(--fw-border);
  border-right: 1px solid var(--fw-border);
}
.col-resize:hover::before,
.col-resize:active::before {
  border-color: var(--fw-fg, #888);
}
/* Thin actions strip at the top of the rightmost column. Hosts the
   delete button. Sits above whichever column-type component this is,
   which means we never have to teach grid/card/doc about it. */
.col-actions-bar {
  flex: 0 0 auto;
  display: flex;
  justify-content: flex-end;
  gap: 0.4rem;
  padding: 0.25rem 0.5rem;
  border-bottom: 1px solid var(--fw-border);
  background: var(--fw-card-bg);
}
.col-delete-btn {
  padding: 0.2rem 0.55rem;
  font-size: 0.8rem;
  color: var(--fw-muted);
  background: var(--fw-input-bg);
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  cursor: pointer;
}
.col-delete-btn:hover {
  color: #e35d6a;
  border-color: #e35d6a;
  background: var(--fw-hover);
}

/* Placeholder for the next column. Rendered once, immediately to the
   right of the rightmost column — clicking it materializes a card
   column there. Dashed border + muted text reads as "drop a column
   here" rather than as a normal control. Width matches the card
   column default so the layout doesn't jump when the user clicks. */
.col-add-card {
  flex: 0 0 auto;
  width: 240px;
  margin: 0.5rem 1rem 0.5rem 0.5rem;
  padding: 1rem;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 1rem;
  color: var(--fw-muted);
  background: transparent;
  border: 2px dashed var(--fw-border);
  border-radius: 8px;
  cursor: pointer;
}
.col-add-card:hover {
  color: var(--fw-fg);
  border-color: var(--fw-fg);
  background: var(--fw-hover);
}
</style>
