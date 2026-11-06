<script setup lang="ts">
// The search box in the toolbar: the search box of the search card on screen
// (`search/searchCards.ts`). What is typed here is only a draft until Enter
// runs it. With no search card on screen, Enter shows the Search card and runs
// it there; ⌘Enter (Ctrl+Enter elsewhere) opens a card of its own. ⌘K puts the
// caret here from anywhere in the app. The pull-down is the advanced panel.
import { computed, nextTick, onBeforeUnmount, onMounted, ref, useTemplateRef, watch } from "vue";
import { SEARCH } from "@/api";
import "@/cards/chip.css";
import { withToken } from "@/grid/query";
import SearchField from "@/search/SearchField.vue";
import { activeSearch, draftAfterMove } from "@/search/searchCards";
import { searchFor, showCard, SEARCH_CARD } from "@/surface";
import SearchPanel from "./SearchPanel.vue";

const draft = ref("");
const input = useTemplateRef<InstanceType<typeof SearchField>>("input");
const box = useTemplateRef<HTMLFormElement>("box");
const panelOpen = ref(false);

/// A search asked for with no card on screen to run it, until one shows.
let waiting: string | null = null;
/// What the cards last gave the box, or what it last asked of one.
let shown = "";
function run(q: string) {
  const card = activeSearch.value;
  if (card) card.run(q);
  else {
    waiting = q;
    shown = draft.value;
    showCard(SEARCH_CARD);
  }
}

watch(
  [activeSearch, () => activeSearch.value?.query.value],
  ([card], [before]) => {
    if (card && waiting !== null) {
      const q = waiting;
      waiting = null;
      card.run(q);
    }
    const query = card?.query.value ?? "";
    const inBox = box.value?.contains(document.activeElement) ?? false;
    const next = draftAfterMove({
      draft: draft.value,
      shown,
      query,
      hadCard: before != null,
      inBox,
    });
    shown = query;
    if (next === draft.value) return;
    draft.value = next;
    // Whoever is in the box and has not typed yet types over this, not after it.
    if (inBox) void nextTick(() => input.value?.selectAll());
  },
  { immediate: true },
);

/// Hands the keyboard back to the page. Blurring alone does not: the page's
/// text selection stays in the field, and a grid puts back the selection it
/// finds after each click, which takes the focus back here too, so Enter on a
/// row just picked would search again rather than open it.
function letGo() {
  input.value?.blur();
  const selection = window.getSelection();
  if (selection?.anchorNode && box.value?.contains(selection.anchorNode)) {
    selection.removeAllRanges();
  }
}

function submit() {
  run(draft.value.trim());
  letGo();
}

function submitNew() {
  const q = draft.value.trim();
  if (q) searchFor(q);
}

function apply(q: string) {
  draft.value = q;
  run(q);
}

function addKey(key: string) {
  draft.value = withToken(draft.value, `${key}:`);
  void nextTick(() => input.value?.suggestAtEnd());
}

function clear() {
  apply("");
  input.value?.focus();
}

/// Escape closes the panel, then puts back what the card is showing.
function escape() {
  if (panelOpen.value) {
    panelOpen.value = false;
    return;
  }
  draft.value = activeSearch.value?.query.value ?? "";
  letGo();
}

function onKey(ev: KeyboardEvent) {
  if ((ev.metaKey || ev.ctrlKey) && !ev.shiftKey && !ev.altKey && ev.key.toLowerCase() === "k") {
    ev.preventDefault();
    input.value?.focus();
    input.value?.selectAll();
  }
}
/// A click elsewhere takes the panel away, and the caret.
function onPointerDown(ev: PointerEvent) {
  if (box.value && ev.composedPath().includes(box.value)) return;
  panelOpen.value = false;
  letGo();
}
onMounted(() => {
  window.addEventListener("keydown", onKey);
  window.addEventListener("pointerdown", onPointerDown);
});
onBeforeUnmount(() => {
  window.removeEventListener("keydown", onKey);
  window.removeEventListener("pointerdown", onPointerDown);
});

const settled = computed(() => activeSearch.value?.settled.value ?? draft.value);
const shortcut = /Mac|iPhone|iPad/.test(navigator.platform) ? "⌘K" : "Ctrl K";
</script>

<template>
  <form
    ref="box"
    class="command-box"
    role="search"
    :data-on-search-card="activeSearch !== null"
    @submit.prevent="submit"
    @keydown.esc="panelOpen = false"
    @contextmenu="activeSearch?.openFeedback($event)"
  >
    <svg class="command-icon" viewBox="0 0 24 24" aria-hidden="true">
      <path
        fill="currentColor"
        d="M15.5 14h-.79l-.28-.27C15.41 12.59 16 11.11 16 9.5 16 5.91 13.09 3 9.5 3S3 5.91 3 9.5 5.91 16 9.5 16c1.61 0 3.09-.59 4.23-1.57l.27.28v.79l5 4.99L20.49 19l-4.99-5zm-6 0C7.01 14 5 11.99 5 9.5S7.01 5 9.5 5 14 7.01 14 9.5 11.99 14 9.5 14z"
      />
    </svg>
    <SearchField
      ref="input"
      v-model="draft"
      :base="SEARCH"
      label="Search your data"
      placeholder="Search your data"
      testid="search-input"
      @submit="submit"
      @submit-new="submitNew"
      @escape="escape"
    />
    <button
      v-if="draft.length > 0"
      type="button"
      class="command-clear"
      aria-label="Clear search"
      title="Clear search"
      data-testid="search-clear"
      @click="clear"
    >
      ×
    </button>
    <kbd class="command-kbd">{{ shortcut }}</kbd>
    <button
      type="button"
      class="command-more"
      :class="{ 'is-on': panelOpen }"
      :aria-expanded="panelOpen"
      aria-label="Advanced search"
      title="Advanced search"
      data-testid="search-advanced"
      @click="panelOpen = !panelOpen"
    >
      <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 9l6 6 6-6" /></svg>
    </button>
    <SearchPanel
      v-if="panelOpen"
      :query="draft"
      :settled="settled"
      @apply="apply"
      @add-key="addKey"
    />
  </form>
</template>

<style scoped>
.command-box {
  position: relative;
  display: flex;
  align-items: center;
  gap: 6px;
  width: 100%;
  height: calc(var(--datalib-control-h) + 2px);
  box-sizing: border-box;
  padding: 0 8px;
  background: var(--datalib-input-bg);
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  color: var(--datalib-faint);
}
.command-box:focus-within {
  border-color: var(--datalib-accent);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--datalib-accent) 18%, transparent);
}
.command-icon {
  flex: 0 0 auto;
  width: var(--datalib-icon-size);
  height: var(--datalib-icon-size);
}
.command-box :deep(.search-field) {
  color: var(--datalib-fg);
}
.command-kbd {
  flex: 0 0 auto;
  font: inherit;
  font-size: var(--datalib-font-size-small);
  padding: 0 5px;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
}
.command-clear,
.command-more {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 0 2px;
  font: inherit;
  color: var(--datalib-muted);
  background: transparent;
  border: 0;
  border-radius: 4px;
  cursor: pointer;
}
.command-clear:hover,
.command-more:hover,
.command-more.is-on {
  color: var(--datalib-fg);
  background: var(--datalib-hover);
}
.command-more svg {
  width: var(--datalib-icon-size);
  height: var(--datalib-icon-size);
  fill: none;
  stroke: currentColor;
  stroke-width: 2;
  stroke-linecap: round;
  stroke-linejoin: round;
}
</style>
