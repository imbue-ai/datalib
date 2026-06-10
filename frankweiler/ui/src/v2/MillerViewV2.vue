<script setup lang="ts">
// Prototype for the "everything is a card" model. Every column IS a
// card: a slot holds the card's source — a JS expression like
// `gridView()` or `documentView("abcd…")` — which is shown in the
// column's header bar and evaluated (cardSource.ts) to render the
// column inside a Shadow DOM via ShadowCardColumn. Edit the source
// and press Enter to re-run the card.
//
// Opening a column is a host command, not a bus message: clicking a
// row in the grid card calls `ctx.host.openColumn('documentView(…)')`
// which replaces everything right of the grid with the new card. The
// bus exists on the ctx for ambient cross-card events but carries no
// structural operations. No URL state — that lands in a real
// refactor, not this prototype.
import { ref } from "vue";
import ShadowCardColumn from "./ShadowCardColumn.vue";
import { createBus } from "./bus";
import type { CardCtx, HostCommands } from "./types";

const bus = createBus();

type Slot = {
  id: string;
  source: string;
};

let nextId = 1;
function freshId(): string {
  return `card-${nextId++}`;
}

const slots = ref<Slot[]>([
  { id: freshId(), source: "gridView()" },
  { id: freshId(), source: "documentView()" },
]);

function openColumnAfter(afterId: string, source: string): string {
  const idx = slots.value.findIndex((s) => s.id === afterId);
  const id = freshId();
  const next = slots.value.slice(0, idx + 1);
  next.push({ id, source });
  slots.value = next;
  return id;
}

function closeColumn(id: string) {
  slots.value = slots.value.filter((s) => s.id !== id);
  ctxCache.delete(id);
}

// One CardCtx per slot, with host commands pre-bound to that card's
// column. Memoized (not stored in slots[]) so the identity Vue passes
// to the child component is stable for the lifetime of the slot.
const ctxCache = new Map<string, CardCtx>();
function ctxFor(slot: Slot): CardCtx {
  let ctx = ctxCache.get(slot.id);
  if (!ctx) {
    const cardId = slot.id;
    const host: HostCommands = {
      openColumn: (source) => openColumnAfter(cardId, source),
      close: () => closeColumn(cardId),
    };
    ctx = { cardId, bus, host };
    ctxCache.set(cardId, ctx);
  }
  return ctx;
}

function commitSource(slot: Slot, e: Event) {
  slot.source = (e.target as HTMLInputElement).value;
}
</script>

<template>
  <div class="v2-root">
    <div class="v2-banner">
      <strong>/v2 prototype</strong> — every column is a card defined by the
      source in its header; the grid opens a doc card via
      <code>host.openColumn('documentView(…)')</code>.
    </div>
    <div class="v2-columns">
      <section v-for="slot in slots" :key="slot.id" class="v2-col">
        <div class="v2-col-chrome">
          <input
            class="v2-col-source"
            :value="slot.source"
            spellcheck="false"
            @keydown.enter="commitSource(slot, $event)"
          />
          <button
            class="v2-col-close"
            title="close column"
            @click="closeColumn(slot.id)"
          >
            ✕
          </button>
        </div>
        <ShadowCardColumn
          class="v2-col-card"
          :source="slot.source"
          :ctx="ctxFor(slot)"
        />
      </section>
    </div>
  </div>
</template>

<style scoped>
.v2-root {
  display: flex;
  flex-direction: column;
  height: 100vh;
}
.v2-banner {
  flex: 0 0 auto;
  padding: 0.4rem 0.8rem;
  background: rgba(99, 102, 241, 0.15);
  border-bottom: 1px solid #888;
  font-size: 0.85rem;
}
.v2-banner code {
  font-family: ui-monospace, monospace;
  background: rgba(0, 0, 0, 0.15);
  padding: 0 0.25rem;
  border-radius: 2px;
}
.v2-columns {
  flex: 1 1 auto;
  display: flex;
  overflow-x: auto;
  overflow-y: hidden;
  min-height: 0;
}
.v2-col {
  flex: 0 0 auto;
  width: 640px;
  height: 100%;
  border-right: 1px solid #888;
  min-width: 0;
  display: flex;
  flex-direction: column;
}
.v2-col-chrome {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: 0.4rem;
  padding: 0.3rem 0.5rem;
  border-bottom: 1px solid #888;
  background: rgba(0, 0, 0, 0.08);
}
.v2-col-source {
  flex: 1 1 auto;
  font: 12px ui-monospace, Menlo, monospace;
  padding: 0.2rem 0.4rem;
  border: 1px solid transparent;
  border-radius: 3px;
  background: transparent;
  color: inherit;
  min-width: 0;
}
.v2-col-source:hover,
.v2-col-source:focus {
  border-color: #888;
  outline: none;
}
.v2-col-close {
  flex: 0 0 auto;
  border: none;
  background: transparent;
  color: inherit;
  opacity: 0.6;
  cursor: pointer;
  font-size: 0.8rem;
}
.v2-col-close:hover {
  opacity: 1;
}
.v2-col-card {
  flex: 1 1 auto;
  min-height: 0;
}
</style>
