<script setup lang="ts">
// Miller-columns layout host. Every column IS a card: a slot holds
// the card's source — a JS expression like `gridView()` or
// `documentView("abcd…")` — which is shown (and editable) in the
// column's header bar and evaluated (cardSource.ts) to render the
// column inside a Shadow DOM via ShadowCard. Edit the source and
// press Enter to re-run the card.
//
// URL: the path is a /-separated list of `code:state` segments, one
// per column (see url.ts). `state` is an opaque per-card string —
// cards persist whatever they want through ctx.host.setState and get
// it back via ctx.initialState; the host just round-trips it.
//
// Structural operations are host commands, not bus messages: a card
// calls `ctx.host.openCards(source)` to open a column to its right
// (replacing everything further right — Miller semantics), or
// `openCards(a, b, …)` to open a run of columns at once. The bus
// carries ambient cross-card events only (e.g. edge hover).
//
// Invariant: the stack always ends in exactly one blank column — the
// place the user types new card source. As soon as it gains code a
// fresh blank appears after it; a run of several trailing blanks
// collapses to one. Blank columns are not part of the URL.
import { computed, ref, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import ShadowCard from "@/components/ShadowCard.vue";
import CardControls from "@/components/CardControls.vue";
import { growSourceBox, vAutoGrow } from "@/components/autoGrow";
import { createBus } from "@/cards/bus";
import { decodeColumns, encodeColumns, type ColumnSpec } from "@/router/columns";
import { displayTitle } from "@/cards/title";
import { devMode } from "@/devMode";
import type { CardCtx, HostCommands } from "@/cards/types";

const route = useRoute();
const router = useRouter();
const bus = createBus();

type Slot = {
  id: string;
  source: string;
  // Opaque per-card state string (see HostCommands.setState).
  state: string;
  // Column width in px; null renders at DEFAULT_WIDTH until the user
  // drags the column's right edge. Persisted in the URL as a ratio of
  // DEFAULT_WIDTH (see specsOf / slotsFromSpecs).
  width: number | null;
  // Human-readable title the compiled card declared (ShadowCard's
  // `title` event), shown instead of the source box when dev mode is
  // off; null until compiled or when the card declares none.
  title: string | null;
};

const DEFAULT_WIDTH = 640;
const MIN_WIDTH = 240;

// Round a width ratio to two decimals for a terse, stable URL.
function sizeRatio(width: number | null): number | null {
  return width == null ? null : Math.round((width / DEFAULT_WIDTH) * 100) / 100;
}

let nextId = 1;
function freshId(): string {
  return `card-${nextId++}`;
}

function newSlot(source: string, state = "", width: number | null = null): Slot {
  return { id: freshId(), source, state, width, title: null };
}

function isBlankSource(source: string): boolean {
  return source.trim() === "";
}

// Re-establish the trailing-blank invariant: drop all but the first
// blank in the trailing blank run (the first is the one the user may
// be mid-edit in), or append a fresh blank when the last column has
// code.
function withTrailingBlank(list: Slot[]): Slot[] {
  let firstBlank = list.length;
  while (firstBlank > 0 && isBlankSource(list[firstBlank - 1].source)) {
    firstBlank--;
  }
  const next = list.slice(0, firstBlank);
  next.push(firstBlank < list.length ? list[firstBlank] : newSlot(""));
  return next;
}

const slots = ref<Slot[]>([]);

// What actually renders. Creating a card means typing source — a dev
// gesture — so outside dev mode the trailing blank column (the place
// you type new source) is hidden. It stays in `slots`, so the
// trailing-blank invariant holds across the toggle (uncommitted text
// in the box is dropped with the textarea, like any unsaved edit).
const visibleSlots = computed(() =>
  devMode.value ? slots.value : slots.value.filter((s) => !isBlankSource(s.source)),
);

// One CardCtx per slot (declared before the initial setSlots call,
// which prunes it). See ctxFor below.
const ctxCache = new Map<string, CardCtx>();

function setSlots(list: Slot[]) {
  const next = withTrailingBlank(list);
  const keep = new Set(next.map((s) => s.id));
  for (const id of [...ctxCache.keys()]) {
    if (!keep.has(id)) ctxCache.delete(id);
  }
  slots.value = next;
}

// ---- URL sync ----

function specsOf(list: Slot[]): ColumnSpec[] {
  return list
    .filter((s) => !isBlankSource(s.source))
    .map((s) => ({ code: s.source, size: sizeRatio(s.width), state: s.state }));
}

function sameSpecs(a: ColumnSpec[], b: ColumnSpec[]): boolean {
  return (
    a.length === b.length &&
    a.every(
      (x, i) =>
        x.code === b[i].code &&
        (x.size ?? null) === (b[i].size ?? null) &&
        x.state === b[i].state,
    )
  );
}

// The stack "/" renders when the URL carries no columns.
const DEFAULT_SPECS: ColumnSpec[] = [{ code: "gridView()", size: null, state: "" }];

function syncUrl() {
  const specs = specsOf(slots.value);
  // Keep "/" for the pristine default stack instead of writing it out.
  const target = sameSpecs(specs, DEFAULT_SPECS) ? "/" : encodeColumns(specs);
  if (route.path !== target) void router.replace(target);
}

function slotsFromSpecs(specs: ColumnSpec[]): Slot[] {
  if (specs.length === 0) return [newSlot("gridView()")];
  return specs.map((c) =>
    newSlot(c.code, c.state, c.size != null ? c.size * DEFAULT_WIDTH : null),
  );
}

setSlots(slotsFromSpecs(decodeColumns(route.path)));

// Back/forward navigation (or a hand-edited URL): rebuild the stack
// when the path no longer describes what we're showing. Compared
// decoded-form to decoded-form so router re-encoding can't cause
// false rebuilds.
watch(
  () => route.path,
  (path) => {
    const incoming = decodeColumns(path);
    // An empty path means the default stack — normalize before
    // comparing, so our own collapse back to "/" (e.g. closing the
    // last document column next to a pristine grid) isn't mistaken
    // for a foreign navigation. Rebuilding would remount every card
    // and visibly flash the grid.
    const effective = incoming.length === 0 ? DEFAULT_SPECS : incoming;
    if (!sameSpecs(effective, specsOf(slots.value))) {
      setSlots(slotsFromSpecs(incoming));
    }
  },
);

// ---- host commands ----

function openColumnAfter(afterId: string, source: string): string {
  const idx = slots.value.findIndex((s) => s.id === afterId);
  const slot = newSlot(source);
  setSlots([...slots.value.slice(0, idx + 1), slot]);
  syncUrl();
  return slot.id;
}

// host.openCards: open a chain of columns. Each source opens to the
// right of the previous one, so the whole chain lands as consecutive
// columns after the caller — and because openColumnAfter truncates
// everything past its anchor, re-opening from the same card swaps the
// trailing panels out (Miller semantics). Drives the scaife control
// panel: one click opens one column per selected version.
function openColumnsAfter(afterId: string, sources: string[]): string[] {
  let prev = afterId;
  const ids: string[] = [];
  for (const source of sources) {
    prev = openColumnAfter(prev, source);
    ids.push(prev);
  }
  return ids;
}

function closeColumn(id: string) {
  setSlots(slots.value.filter((s) => s.id !== id));
  syncUrl();
}

function setColumnState(id: string, state: string) {
  const slot = slots.value.find((s) => s.id === id);
  if (!slot || slot.state === state) return;
  slot.state = state;
  syncUrl();
}

// One CardCtx per slot, with host commands pre-bound to that card's
// column. Memoized (not stored in slots[]) so the identity Vue passes
// to the child component is stable for the lifetime of the slot.
// `initialState` is a getter so a source re-run picks up the state
// the card saved most recently, not the page-load snapshot.
function ctxFor(slot: Slot): CardCtx {
  let ctx = ctxCache.get(slot.id);
  if (!ctx) {
    const cardId = slot.id;
    const host: HostCommands = {
      openCards: (...sources) => openColumnsAfter(cardId, sources),
      setSource: (source) => setColumnSource(cardId, source),
      close: () => closeColumn(cardId),
      setState: (state) => setColumnState(cardId, state),
    };
    ctx = {
      cardId,
      get initialState() {
        return slot.state;
      },
      bus,
      host,
    };
    ctxCache.set(cardId, ctx);
  }
  return ctx;
}

function commitSource(slot: Slot, e: Event) {
  const next = (e.target as HTMLTextAreaElement).value;
  if (next !== slot.source) {
    slot.source = next;
    // New code means the old card's state no longer applies.
    slot.state = "";
  }
  setSlots(slots.value);
  syncUrl();
}

// host.setSource: replace this column's own source (clearing state) —
// drives the agent hand-off (see cards/handoff.ts).
function setColumnSource(id: string, source: string) {
  const slot = slots.value.find((s) => s.id === id);
  if (!slot) return;
  slot.source = source;
  slot.state = "";
  setSlots(slots.value);
  syncUrl();
}

// Drag a column's right edge to set its width. Captures the pointer
// so the move tracks even when the cursor crosses other columns;
// clamps to MIN_WIDTH so columns can't collapse to nothing.
function onResizeStart(slot: Slot, ev: PointerEvent) {
  ev.preventDefault();
  const startX = ev.clientX;
  const startWidth = slot.width ?? DEFAULT_WIDTH;
  const target = ev.currentTarget as HTMLElement;
  target.setPointerCapture(ev.pointerId);

  const onMove = (e: PointerEvent) => {
    slot.width = Math.max(MIN_WIDTH, startWidth + (e.clientX - startX));
  };
  const onUp = (e: PointerEvent) => {
    target.releasePointerCapture(e.pointerId);
    target.removeEventListener("pointermove", onMove);
    target.removeEventListener("pointerup", onUp);
    target.removeEventListener("pointercancel", onUp);
    // Persist the new width as a size ratio in the URL.
    syncUrl();
  };
  target.addEventListener("pointermove", onMove);
  target.addEventListener("pointerup", onUp);
  target.addEventListener("pointercancel", onUp);
}
</script>

<template>
  <div class="miller-root">
    <div class="miller-columns">
      <section
        v-for="slot in visibleSlots"
        :key="slot.id"
        class="miller-col"
        :style="{ width: (slot.width ?? DEFAULT_WIDTH) + 'px' }"
      >
        <div class="miller-col-chrome">
          <textarea
            v-if="devMode"
            v-auto-grow
            class="miller-col-source"
            rows="1"
            :value="slot.source"
            spellcheck="false"
            @input="growSourceBox($event.target as HTMLTextAreaElement)"
            @keydown.enter.exact.prevent="commitSource(slot, $event)"
          />
          <div v-else class="miller-col-title">
            {{ displayTitle(slot.source, slot.title) }}
          </div>
          <CardControls :source="slot.source" :ctx="ctxFor(slot)" />
        </div>
        <ShadowCard
          class="miller-col-card"
          :source="slot.source"
          :ctx="ctxFor(slot)"
          @title="(t) => (slot.title = t)"
        />
        <div
          class="miller-col-resize"
          role="separator"
          aria-orientation="vertical"
          @pointerdown="(e) => onResizeStart(slot, e)"
        />
      </section>
    </div>
  </div>
</template>

<style scoped>
.miller-root {
  display: flex;
  flex-direction: column;
  /* Fill whatever the parent's flex layout gives us; basis 0 +
     min-height 0 so intrinsic content height can't stretch the
     page. */
  flex: 1 1 0;
  min-height: 0;
}
.miller-columns {
  flex: 1 1 auto;
  display: flex;
  overflow-x: auto;
  overflow-y: hidden;
  min-height: 0;
}
.miller-col {
  position: relative;
  flex: 0 0 auto;
  /* Fill the row's cross axis via flex stretch, not height: 100%.
     WebKit (Safari + Tauri's WKWebView) resolves percentage heights
     against the flex-sized .miller-columns as `auto`, collapsing every
     column to its chrome bar (~36px); stretch sizes it definitively in
     all engines. */
  align-self: stretch;
  border-right: 1px solid #888;
  min-width: 0;
  display: flex;
  flex-direction: column;
}
/* Invisible grab strip centered on the column's 1px divider —
   slightly wider than the border for a comfortable hit target; the
   col-resize cursor is the only affordance. */
.miller-col-resize {
  position: absolute;
  top: 0;
  right: -3px;
  width: 7px;
  height: 100%;
  cursor: col-resize;
  z-index: 1;
}
.miller-col-chrome {
  flex: 0 0 auto;
  display: flex;
  align-items: flex-start;
  gap: 0.4rem;
  padding: 0.3rem 0.5rem;
  border-bottom: 1px solid #888;
  background: rgba(0, 0, 0, 0.08);
}
/* Highlight the whole header while the source box is being edited —
   a tinted box inside the gray bar looks patchy. */
.miller-col-chrome:focus-within {
  background: rgba(99, 102, 241, 0.18);
}
.miller-col-source {
  flex: 1 1 auto;
  font: 12px/1.5 ui-monospace, Menlo, monospace;
  padding: 0.2rem 0.4rem;
  border: none;
  border-radius: 3px;
  background: transparent;
  color: inherit;
  min-width: 0;
  /* Soft-wrap multi-line source; height is managed by growSourceBox. */
  resize: none;
  overflow: hidden;
  white-space: pre-wrap;
  overflow-wrap: break-word;
  box-sizing: border-box;
  display: block;
}
.miller-col-source:focus {
  outline: none;
}
/* Non-dev chrome: the card's human-readable title where the source
   box would be. Styled as a heading (proportional, semibold) so it
   reads as a title, not code; the 18px line box matches the source
   box's 12px × 1.5 so toggling dev mode doesn't reflow the bar. */
.miller-col-title {
  flex: 1 1 auto;
  min-width: 0;
  font-size: 13px;
  font-weight: 600;
  line-height: 18px;
  padding: 0.2rem 0.4rem;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.miller-col-card {
  flex: 1 1 auto;
  min-height: 0;
}
</style>
