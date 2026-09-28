<script setup lang="ts">
// Miller-columns layout host. Every column IS a card: a slot holds
// the card's source — a JS expression like `gridView()` or
// `documentView("abcd…")` — which is shown (and editable) in the
// column's header bar and evaluated (cardSource.ts) to render the
// column inside a Shadow DOM via ShadowCard. Edit the source and
// press Enter to re-run the card.
//
// The stack is the URL, and the browser's history is the only history:
// opening, closing or repointing a column is a navigation (Back undoes
// it), a card's state or a column's width rewrites the current entry.
import { nextTick, onBeforeUnmount, ref, useTemplateRef, watch, watchEffect } from "vue";
import { useRoute, useRouter } from "vue-router";
import ShadowCard from "@/components/ShadowCard.vue";
import CardControls from "@/components/CardControls.vue";
import { growSourceBox, vAutoGrow } from "@/components/autoGrow";
import { createBus } from "@/cards/bus";
import { decodeColumns, type ColumnSpec } from "@/router/columns";
import { cardType, newCardId } from "@/cards/cardId";
import { displayTitle } from "@/cards/title";
import { devMode } from "@/devMode";
import { setCardHelp } from "@/cards/help";
import { revealScrollLeft } from "@/views/millerReveal";
import {
  DEFAULT_SPECS,
  DEFAULT_WIDTH,
  pageTitle,
  pathFor,
  reconcile,
  sameSpecs,
  specOf,
  specsOf,
  widthOf,
  type Slot,
} from "@/views/millerStack";
import type { CardCtx, HostCommands } from "@/cards/types";

const props = withDefaults(
  defineProps<{
    // Whether this layout is on screen. Only the layout on screen owns
    // the URL; this one keeps its stack while another does.
    active?: boolean;
  }>(),
  { active: true },
);

const route = useRoute();
const router = useRouter();
const bus = createBus();

const MIN_WIDTH = 240;

function newSlot(source: string, state = "", width: number | null = null): Slot {
  return { id: newCardId(), source, state, width, title: null };
}

function slotFor(spec: ColumnSpec): Slot {
  return newSlot(spec.code, spec.state, widthOf(spec));
}

const slots = ref<Slot[]>([]);

// One CardCtx per slot (declared before the initial setSlots call,
// which prunes it). See ctxFor below.
const ctxCache = new Map<string, CardCtx>();

function setSlots(list: Slot[]) {
  const keep = new Set(list.map((s) => s.id));
  for (const id of [...ctxCache.keys()]) {
    if (!keep.has(id)) ctxCache.delete(id);
  }
  slots.value = list;
}

// ---- URL sync ----

function effectiveSpecs(path: string): ColumnSpec[] {
  const incoming = decodeColumns(path);
  return incoming.length === 0 ? DEFAULT_SPECS : incoming;
}

// What the URL last said, decoded, so a write that would change
// nothing is skipped and our own writes are told apart from a foreign
// navigation.
let written: ColumnSpec[] = effectiveSpecs(route.path);
setSlots(written.map(slotFor));

// Writes are queued: the router cancels a navigation another one
// overtakes, and a row click is two writes in one tick — the grid's
// selection (replace), then the document it opens (push).
let queue: Promise<unknown> = Promise.resolve();
let inFlight = 0;

function writeUrl(mode: "push" | "replace", specs: ColumnSpec[]) {
  if (!props.active || sameSpecs(specs, written)) return;
  written = specs;
  const target = pathFor(specs);
  inFlight++;
  queue = queue
    .then(() => router[mode](target))
    .catch((e: unknown) => console.warn("url write failed", e))
    .finally(() => {
      inFlight--;
      if (inFlight === 0) adoptRoute(route.path);
    });
}

// A card's state or a column's width: the page's own scroll position,
// not somewhere Back should return to.
function syncState() {
  writeUrl("replace", specsOf(slots.value));
}

// A structural change: the new stack is a new entry.
function navigate(change: () => void) {
  change();
  writeUrl("push", specsOf(slots.value));
}

// The URL changed under us — Back, Forward, a link, a hand-edited
// address: keep what still matches, mount what doesn't, show what is
// new. Compared decoded-form to decoded-form so router re-encoding
// can't cause false rebuilds.
function adoptRoute(path: string) {
  const specs = effectiveSpecs(path);
  if (sameSpecs(specs, specsOf(slots.value))) return;
  written = specs;
  const before = new Set(slots.value.map((s) => s.id));
  const next = reconcile(slots.value, specs, slotFor);
  setSlots(next);
  const added = next.filter((s) => !before.has(s.id));
  if (added.length > 0) revealColumn(added[added.length - 1].id);
}

watch(
  () => route.path,
  (path) => {
    // Our own writes settle through the queue's tail, which adopts
    // the route once; a foreign navigation in between is picked up
    // there too.
    if (inFlight === 0 && props.active) adoptRoute(path);
  },
);

// Back on screen: the URL says what another layout showed, so put this
// stack back in it.
watch(
  () => props.active,
  (on) => {
    if (!on) return;
    written = effectiveSpecs(route.path);
    writeUrl("replace", specsOf(slots.value));
  },
);

// The browser's name for the page — its tab, its history menu, a
// bookmark of it.
watchEffect(() => {
  if (!props.active) return;
  document.title = pageTitle(slots.value.map((s) => displayTitle(s.source, s.title)));
});
onBeforeUnmount(() => {
  if (props.active) document.title = pageTitle([]);
});

// ---- host commands ----

function indexOf(id: string): number {
  return slots.value.findIndex((s) => s.id === id);
}

function openColumnAfter(afterId: string, source: string): string {
  const idx = indexOf(afterId);
  const slot = newSlot(source);
  setSlots([...slots.value.slice(0, idx + 1), slot]);
  return slot.id;
}

// host.openCards: open a chain of columns. Each source opens to the
// right of the previous one, so the whole chain lands as consecutive
// columns after the caller — and because openColumnAfter truncates
// everything past its anchor, re-opening from the same card swaps the
// trailing panels out (Miller semantics). Drives the scaife control
// panel: one click opens one column per selected version.
function openColumnsAfter(afterId: string, sources: string[]): string[] {
  const ids: string[] = [];
  navigate(() => {
    let prev = afterId;
    for (const source of sources) {
      prev = openColumnAfter(prev, source);
      ids.push(prev);
    }
  });
  // The end of the chain is what the click was for; showing it keeps
  // as much of the chain (and the caller) on screen as fits.
  if (ids.length > 0) revealColumn(ids[ids.length - 1]);
  return ids;
}

// host.hrefFor: the URL openCards would land on, for a real link.
function hrefAfter(afterId: string, sources: string[]): string {
  const idx = indexOf(afterId);
  return pathFor([...specsOf(slots.value.slice(0, idx + 1)), ...sources.map(specOf)]);
}

function closeColumn(id: string) {
  navigate(() => setSlots(slots.value.filter((s) => s.id !== id)));
}

function setColumnState(id: string, state: string) {
  const slot = slots.value.find((s) => s.id === id);
  if (!slot || slot.state === state) return;
  slot.state = state;
  syncState();
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
      hrefFor: (...sources) => hrefAfter(cardId, sources),
      setSource: (source) => setColumnSource(cardId, source),
      close: () => closeColumn(cardId),
      setState: (state) => setColumnState(cardId, state),
    };
    ctx = {
      cardId,
      get cardType() {
        return cardType(slot.source);
      },
      get initialState() {
        return slot.state;
      },
      setTitle: (title) => {
        slot.title = title;
      },
      setHelp: (html) => setCardHelp(cardId, html),
      bus,
      host,
    };
    ctxCache.set(cardId, ctx);
  }
  return ctx;
}

function commitSource(slot: Slot, e: Event) {
  const next = (e.target as HTMLTextAreaElement).value;
  if (next === slot.source) return;
  navigate(() => {
    slot.source = next;
    // New code means the old card's state no longer applies.
    slot.state = "";
  });
}

// host.setSource: replace this column's own source (clearing state) —
// drives the gallery's pick and the agent hand-off (see handoff.ts).
function setColumnSource(id: string, source: string) {
  const slot = slots.value.find((s) => s.id === id);
  if (!slot) return;
  navigate(() => {
    slot.source = source;
    slot.state = "";
  });
}

// The "+" strip after the last column appends a gallery column (both
// modes), which the user resolves by picking a component (it replaces
// itself via host.setSource).
function addCard() {
  const slot = newSlot("galleryView()");
  navigate(() => setSlots([...slots.value, slot]));
  revealColumn(slot.id);
}

// The toolbar's "Data sources": the column already showing that
// source, or a new one at the end of the stack.
function showCard(source: string) {
  const existing = slots.value.find((s) => s.source === source);
  if (existing) {
    revealColumn(existing.id);
    return;
  }
  const slot = newSlot(source);
  navigate(() => setSlots([...slots.value, slot]));
  revealColumn(slot.id);
}

const columnsEl = useTemplateRef<HTMLDivElement>("columnsEl");

// Scroll the row to show a column once it has rendered (millerReveal.ts
// says where). Not scrollIntoView: its "nearest" never moves a column
// wider than the row, and its "start" would push the caller off screen.
function revealColumn(id: string) {
  void nextTick(() => {
    const row = columnsEl.value;
    const el = row?.querySelector<HTMLElement>(`[data-slot-id="${id}"]`);
    if (!row || !el) return;
    const start =
      el.getBoundingClientRect().left - row.getBoundingClientRect().left + row.scrollLeft;
    const left = revealScrollLeft(
      { start: row.scrollLeft, width: row.clientWidth },
      { start, width: el.offsetWidth },
    );
    row.scrollTo({ left, behavior: "smooth" });
  });
}

defineExpose({ addCard, showCard });

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
    syncState();
  };
  target.addEventListener("pointermove", onMove);
  target.addEventListener("pointerup", onUp);
  target.addEventListener("pointercancel", onUp);
}
</script>

<template>
  <div class="miller-root">
    <div ref="columnsEl" class="miller-columns">
      <section
        v-for="slot in slots"
        :key="slot.id"
        class="miller-col"
        :data-slot-id="slot.id"
        :style="{ width: (slot.width ?? DEFAULT_WIDTH) + 'px' }"
      >
        <div class="miller-col-chrome card-chrome" :class="{ 'card-chrome--title': !devMode }">
          <textarea
            v-if="devMode"
            v-auto-grow
            class="miller-col-source card-source"
            rows="1"
            :value="slot.source"
            spellcheck="false"
            @input="growSourceBox($event.target as HTMLTextAreaElement)"
            @keydown.enter.exact.prevent="commitSource(slot, $event)"
          />
          <div v-else class="miller-col-title card-title">
            {{ displayTitle(slot.source, slot.title) }}
          </div>
          <CardControls :source="slot.source" :ctx="ctxFor(slot)" />
        </div>
        <ShadowCard class="miller-col-card" :source="slot.source" :ctx="ctxFor(slot)" />
        <div
          class="miller-col-resize"
          role="separator"
          aria-orientation="vertical"
          @pointerdown="(e) => onResizeStart(slot, e)"
        />
      </section>
      <button class="miller-add" title="add card" @click="addCard">＋</button>
    </div>
  </div>
</template>

<style scoped src="./cardChrome.css"></style>
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
.miller-col-card {
  flex: 1 1 auto;
  min-height: 0;
}
/* "New card" strip after the last column — same dashed-affordance
   family as the tiling layout's add areas. */
.miller-add {
  flex: 0 0 auto;
  align-self: stretch;
  width: 28px;
  margin: 0.5rem;
  cursor: pointer;
  font-size: 1rem;
  line-height: 1;
  color: color-mix(in srgb, var(--datalib-fg) 45%, transparent);
  background: transparent;
  border: 1px dashed color-mix(in srgb, var(--datalib-fg) 22%, transparent);
  border-radius: 4px;
}
.miller-add:hover {
  color: var(--datalib-fg);
  background: var(--datalib-hover);
}
</style>
