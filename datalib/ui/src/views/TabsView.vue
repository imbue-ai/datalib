<script setup lang="ts">
// Tabs layout host: one card at a time, full size, and a sidebar
// listing every open card as a tree — each tab indented under the tab
// that opened it, as in Firefox's Tree Style Tab. The tree is kept in
// this browser's localStorage; the URL names only the selected tab, as
// a one-column stack, so a copied link opens that card alone anywhere.
// The decisions live in tabTree.ts; this file applies them.
import { computed, onBeforeUnmount, reactive, ref, watch, watchEffect } from "vue";
import { useRoute, useRouter } from "vue-router";
import ShadowCard from "@/components/ShadowCard.vue";
import CardControls from "@/components/CardControls.vue";
import { growSourceBox, vAutoGrow } from "@/components/autoGrow";
import { createBus } from "@/cards/bus";
import { chainHref } from "@/cards/chainHref";
import { setCardHelp } from "@/cards/help";
import { displayTitle } from "@/cards/title";
import { devMode } from "@/devMode";
import { decodeColumns, type ColumnSpec } from "@/router/columns";
import { DEFAULT_SPECS, pageTitle, pathFor, sameSpecs } from "@/views/millerStack";
import {
  closeTab,
  newTab,
  nextCounter,
  openChain,
  openStack,
  parseStored,
  rows,
  selectAfterClose,
  serialize,
  subtree,
  tabForSpec,
  type Tab,
} from "@/views/tabTree";
import type { CardCtx, HostCommands } from "@/cards/types";

const props = defineProps<{
  // Whether this layout is on screen, and so owns the URL.
  active: boolean;
  // Page load or a navigation into the card surface: the URL is what
  // the person asked for, so open it rather than overwrite it.
  openUrlOnMount: boolean;
}>();

const route = useRoute();
const router = useRouter();
const bus = createBus();

const STORAGE_KEY = "datalib-tabs";

function load(): { tabs: Tab[]; selectedId: string | null } | null {
  try {
    return parseStored(localStorage.getItem(STORAGE_KEY));
  } catch {
    return null;
  }
}

const stored = load();
let counter = nextCounter(stored?.tabs ?? []);
const freshId = () => `t${counter++}`;

const defaultTab = () => newTab(freshId(), DEFAULT_SPECS[0].code, null);
const tabs = ref<Tab[]>(stored && stored.tabs.length > 0 ? stored.tabs : [defaultTab()]);
const selectedId = ref<string>(
  tabs.value.find((t) => t.id === stored?.selectedId)?.id ?? tabs.value[0].id,
);

watch(
  [tabs, selectedId],
  () => {
    try {
      localStorage.setItem(
        STORAGE_KEY,
        serialize({ tabs: tabs.value, selectedId: selectedId.value }),
      );
    } catch {
      // Private window or blocked storage: the tabs last as long as the page.
    }
  },
  { deep: true },
);

function tabById(id: string): Tab | undefined {
  return tabs.value.find((t) => t.id === id);
}

const selected = computed(() => tabById(selectedId.value));
const parentOfSelected = computed(() => {
  const p = selected.value?.parentId;
  return p ? tabById(p) : undefined;
});
const sidebarRows = computed(() => rows(tabs.value));

// A card mounts the first time its tab is shown and stays mounted
// after, so switching back finds it as it was.
const shown = reactive(new Set<string>());
watch(selectedId, (id) => shown.add(id), { immediate: true });

// ---- URL sync ----

function effectiveSpecs(path: string): ColumnSpec[] {
  const incoming = decodeColumns(path);
  return incoming.length === 0 ? DEFAULT_SPECS : incoming;
}

function specOfTab(tab: Tab): ColumnSpec {
  return { code: tab.source, size: null, state: tab.state };
}

// What the URL last said, and which tab its history entry belongs to,
// so a write that would change nothing is skipped.
let written: { tabId: string | null; specs: ColumnSpec[] } = { tabId: null, specs: [] };
let queue: Promise<unknown> = Promise.resolve();
let inFlight = 0;

// The history entry carries the tab's id, so Back to an entry whose
// state has since moved on finds the tab rather than opening a twin.
function entryTabId(): string | null {
  const id = (window.history.state as { tabId?: unknown } | null)?.tabId;
  return typeof id === "string" ? id : null;
}

function writeUrl(mode: "push" | "replace") {
  const tab = selected.value;
  if (!props.active || !tab) return;
  const specs = [specOfTab(tab)];
  if (written.tabId === tab.id && sameSpecs(specs, written.specs)) return;
  written = { tabId: tab.id, specs };
  const location = { path: pathFor(specs), state: { tabId: tab.id }, force: true };
  inFlight++;
  queue = queue
    .then(() => router[mode](location))
    .catch((e: unknown) => console.warn("url write failed", e))
    .finally(() => {
      inFlight--;
      if (inFlight === 0) adoptRoute();
    });
}

// The URL changed under us — Back, Forward, a link, a hand-edited
// address: show the tab it names, opening it if we have none.
function adoptRoute() {
  if (!props.active) return;
  const specs = effectiveSpecs(route.path);
  const entryId = entryTabId();
  written = { tabId: entryId, specs };
  const byEntry = entryId ? tabById(entryId) : undefined;
  const bySpec = specs.length === 1 ? tabForSpec(tabs.value, selectedId.value, specs[0]) : null;
  if (byEntry && specs.length === 1 && byEntry.source === specs[0].code) {
    selectedId.value = byEntry.id;
  } else if (bySpec) {
    selectedId.value = bySpec.id;
  } else {
    const opened = openStack(tabs.value, specs, freshId);
    tabs.value = opened.tabs;
    selectedId.value = opened.lastId;
  }
  // The URL now names the selected tab at its current state, and one tab only.
  writeUrl("replace");
}

watch(
  () => route.path,
  () => {
    if (inFlight === 0) adoptRoute();
  },
);

if (props.openUrlOnMount) adoptRoute();

watch(
  () => props.active,
  (on) => {
    if (!on) return;
    written = { tabId: entryTabId(), specs: effectiveSpecs(route.path) };
    writeUrl("replace");
  },
  { immediate: !props.openUrlOnMount },
);

watchEffect(() => {
  if (!props.active) return;
  const tab = selected.value;
  document.title = pageTitle(tab ? [displayTitle(tab.source, tab.title)] : []);
});
onBeforeUnmount(() => {
  if (props.active) document.title = pageTitle([]);
});

// ---- tab operations ----

function select(id: string) {
  if (id === selectedId.value) return;
  selectedId.value = id;
  writeUrl("push");
}

// A tab the person picked out of the sidebar is one they meant to keep.
function visit(tab: Tab) {
  tab.preview = false;
  select(tab.id);
}

function openFrom(parentId: string, sources: string[]): string[] {
  if (sources.length === 0) return [];
  const caller = tabById(parentId);
  if (caller) caller.preview = false;
  const { tabs: next, ids } = openChain(tabs.value, parentId, sources, freshId);
  tabs.value = next;
  select(ids[ids.length - 1]);
  return ids;
}

function openRoot(source: string) {
  const tab = newTab(freshId(), source, null);
  tabs.value = [...tabs.value, tab];
  select(tab.id);
}

function close(id: string) {
  const before = tabs.value;
  const { tabs: after, closed } = closeTab(before, id);
  if (closed.size === 0) return;
  const next = after.length > 0 ? after : [defaultTab()];
  tabs.value = next;
  for (const gone of closed) {
    ctxCache.delete(gone);
    shown.delete(gone);
  }
  if (closed.has(selectedId.value)) {
    select(selectAfterClose(before, next, id) ?? next[0].id);
  }
}

function toggle(tab: Tab) {
  tab.collapsed = !tab.collapsed;
  // Folding away the selected tab selects the branch it went into.
  if (
    tab.collapsed &&
    selectedId.value !== tab.id &&
    subtree(tabs.value, tab.id).has(selectedId.value)
  ) {
    select(tab.id);
  }
}

function setSource(id: string, source: string) {
  const tab = tabById(id);
  if (!tab) return;
  tab.source = source;
  tab.state = "";
  if (id === selectedId.value) writeUrl("push");
}

function setState(id: string, state: string) {
  const tab = tabById(id);
  if (!tab || tab.state === state) return;
  tab.state = state;
  if (id === selectedId.value) writeUrl("replace");
}

function commitSource(tab: Tab, e: Event) {
  const next = (e.target as HTMLTextAreaElement).value;
  if (next !== tab.source) setSource(tab.id, next);
}

// The toolbar's "New card" and "Logs".
function addCard() {
  openRoot("galleryView()");
}

function showCard(source: string) {
  const existing = tabs.value.find((t) => t.source === source);
  if (existing) visit(existing);
  else openRoot(source);
}

defineExpose({ addCard, showCard });

// One CardCtx per tab, stable for the tab's life (see MillerView's ctxFor).
const ctxCache = new Map<string, CardCtx>();

function ctxFor(tab: Tab): CardCtx {
  let ctx = ctxCache.get(tab.id);
  if (!ctx) {
    const cardId = tab.id;
    const host: HostCommands = {
      openCards: (...sources) => openFrom(cardId, sources),
      hrefFor: (...sources) => chainHref(sources),
      setSource: (source) => setSource(cardId, source),
      close: () => close(cardId),
      setState: (state) => setState(cardId, state),
    };
    ctx = {
      cardId,
      get initialState() {
        return tabById(cardId)?.state ?? "";
      },
      setTitle: (title) => {
        const t = tabById(cardId);
        if (t) t.title = title;
      },
      setHelp: (html) => setCardHelp(cardId, html),
      bus,
      host,
    };
    ctxCache.set(cardId, ctx);
  }
  return ctx;
}

function titleOf(tab: Tab): string {
  return displayTitle(tab.source, tab.title);
}

// ---- sidebar width ----

const WIDTH_KEY = "datalib-tabs-sidebar-width";
const DEFAULT_SIDEBAR = 240;
const MIN_SIDEBAR = 140;

// Never so wide that the card is squeezed out: at most 60% of the window.
function clampSidebar(px: number): number {
  const max = Math.max(MIN_SIDEBAR, window.innerWidth * 0.6);
  return Math.round(Math.min(max, Math.max(MIN_SIDEBAR, px)));
}

function loadSidebarWidth(): number {
  try {
    const n = Number(localStorage.getItem(WIDTH_KEY));
    return n > 0 ? clampSidebar(n) : DEFAULT_SIDEBAR;
  } catch {
    return DEFAULT_SIDEBAR;
  }
}

const sidebarWidth = ref(loadSidebarWidth());

function saveSidebarWidth() {
  try {
    localStorage.setItem(WIDTH_KEY, String(sidebarWidth.value));
  } catch {
    // Blocked storage: the width lasts as long as the page.
  }
}

// Drag the sidebar's right edge; the pointer is captured so the drag
// keeps tracking over the card's shadow DOM and iframes.
function onSidebarResize(ev: PointerEvent) {
  ev.preventDefault();
  const startX = ev.clientX;
  const startWidth = sidebarWidth.value;
  const target = ev.currentTarget as HTMLElement;
  target.setPointerCapture(ev.pointerId);
  const onMove = (e: PointerEvent) => {
    sidebarWidth.value = clampSidebar(startWidth + e.clientX - startX);
  };
  const onUp = (e: PointerEvent) => {
    target.releasePointerCapture(e.pointerId);
    target.removeEventListener("pointermove", onMove);
    target.removeEventListener("pointerup", onUp);
    target.removeEventListener("pointercancel", onUp);
    saveSidebarWidth();
  };
  target.addEventListener("pointermove", onMove);
  target.addEventListener("pointerup", onUp);
  target.addEventListener("pointercancel", onUp);
}

function resetSidebarWidth() {
  sidebarWidth.value = DEFAULT_SIDEBAR;
  saveSidebarWidth();
}
</script>

<template>
  <div class="tabs-root">
    <nav class="tabs-sidebar" aria-label="open cards" :style="{ flexBasis: sidebarWidth + 'px' }">
      <button class="tabs-add" title="add card" @click="addCard">＋ new card</button>
      <ul class="tabs-list" role="tree">
        <li
          v-for="row in sidebarRows"
          :key="row.tab.id"
          class="tabs-row"
          :class="{
            'is-selected': row.tab.id === selectedId,
            'is-preview': row.tab.preview,
          }"
          role="treeitem"
          :aria-selected="row.tab.id === selectedId"
          :aria-expanded="row.hasChildren ? !row.tab.collapsed : undefined"
          :data-tab-id="row.tab.id"
          :style="{ paddingLeft: 0.3 + row.depth * 0.9 + 'rem' }"
          :title="titleOf(row.tab)"
          @click="visit(row.tab)"
          @auxclick.prevent="(e: MouseEvent) => e.button === 1 && close(row.tab.id)"
        >
          <button
            v-if="row.hasChildren"
            class="tabs-twisty"
            :title="row.tab.collapsed ? 'expand' : 'collapse'"
            @click.stop="toggle(row.tab)"
          >
            {{ row.tab.collapsed ? "▸" : "▾" }}
          </button>
          <span v-else class="tabs-twisty" />
          <span class="tabs-label">{{ titleOf(row.tab) }}</span>
          <button
            class="tabs-close"
            :title="row.tab.collapsed && row.hasChildren ? 'close this branch' : 'close'"
            @click.stop="close(row.tab.id)"
          >
            ✕
          </button>
        </li>
      </ul>
      <div
        class="tabs-sidebar-resize"
        role="separator"
        aria-orientation="vertical"
        title="drag to resize; double-click to reset"
        @pointerdown="onSidebarResize"
        @dblclick="resetSidebarWidth"
      />
    </nav>
    <section v-if="selected" class="tabs-main">
      <div class="tabs-chrome" :class="{ 'tabs-chrome--title': !devMode }">
        <button
          v-if="parentOfSelected"
          class="tabs-from"
          :title="`opened from ${titleOf(parentOfSelected)}`"
          @click="visit(parentOfSelected)"
        >
          ↰ {{ titleOf(parentOfSelected) }}
        </button>
        <textarea
          v-if="devMode"
          :key="selected.id"
          v-auto-grow
          class="tabs-source"
          rows="1"
          :value="selected.source"
          spellcheck="false"
          @input="growSourceBox($event.target as HTMLTextAreaElement)"
          @keydown.enter.exact.prevent="commitSource(selected, $event)"
        />
        <div v-else class="tabs-title">{{ titleOf(selected) }}</div>
        <CardControls :key="selected.id" :source="selected.source" :ctx="ctxFor(selected)" />
      </div>
      <template v-for="tab in tabs" :key="tab.id">
        <ShadowCard
          v-if="shown.has(tab.id)"
          v-show="tab.id === selectedId"
          class="tabs-card"
          :source="tab.source"
          :ctx="ctxFor(tab)"
        />
      </template>
    </section>
  </div>
</template>

<style scoped>
.tabs-root {
  display: flex;
  flex: 1 1 0;
  min-height: 0;
}
.tabs-sidebar {
  position: relative;
  /* The basis is the dragged width, bound in the template. */
  flex: 0 0 auto;
  display: flex;
  flex-direction: column;
  min-height: 0;
  border-right: 1px solid #888;
  font-size: 13px;
}
/* Invisible grab strip centered on the sidebar's border, as on a
   miller column's edge. */
.tabs-sidebar-resize {
  position: absolute;
  top: 0;
  right: -3px;
  width: 7px;
  height: 100%;
  cursor: col-resize;
  z-index: 1;
}
.tabs-add {
  flex: 0 0 auto;
  margin: 0.4rem;
  padding: 0.2rem 0.4rem;
  cursor: pointer;
  text-align: left;
  color: color-mix(in srgb, var(--datalib-fg) 60%, transparent);
  background: transparent;
  border: 1px dashed color-mix(in srgb, var(--datalib-fg) 22%, transparent);
  border-radius: 4px;
}
.tabs-add:hover {
  color: var(--datalib-fg);
  background: var(--datalib-hover);
}
.tabs-list {
  flex: 1 1 auto;
  min-height: 0;
  overflow-y: auto;
  margin: 0;
  padding: 0 0 0.5rem;
  list-style: none;
}
.tabs-row {
  display: flex;
  align-items: center;
  gap: 0.2rem;
  padding-right: 0.3rem;
  line-height: 1.7;
  cursor: pointer;
  border-radius: 3px;
  margin: 0 0.3rem;
}
.tabs-row:hover {
  background: var(--datalib-hover);
}
.tabs-row.is-selected {
  background: color-mix(in srgb, var(--datalib-accent) 18%, transparent);
  color: color-mix(in srgb, var(--datalib-accent) 70%, var(--datalib-fg));
  font-weight: 600;
}
/* Opened by a card and not visited yet: the next card its opener
   opens takes its place (tabTree.ts openChain). */
.tabs-row.is-preview .tabs-label {
  font-style: italic;
}
.tabs-twisty {
  flex: 0 0 1rem;
  width: 1rem;
  padding: 0;
  border: none;
  background: transparent;
  color: inherit;
  cursor: pointer;
  font-size: 11px;
}
.tabs-label {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.tabs-close {
  flex: 0 0 auto;
  visibility: hidden;
  padding: 0 0.2rem;
  border: none;
  border-radius: 3px;
  background: transparent;
  color: inherit;
  cursor: pointer;
  font-size: 11px;
}
.tabs-row:hover .tabs-close,
.tabs-row.is-selected .tabs-close {
  visibility: visible;
}
.tabs-close:hover {
  background: var(--datalib-hover);
}
.tabs-main {
  flex: 1 1 auto;
  min-width: 0;
  display: flex;
  flex-direction: column;
}
.tabs-chrome {
  flex: 0 0 auto;
  display: flex;
  align-items: flex-start;
  gap: 0.4rem;
  padding: 0.3rem 0.5rem;
  border-bottom: 1px solid #888;
  background: rgba(0, 0, 0, 0.08);
}
.tabs-chrome:focus-within {
  background: rgba(99, 102, 241, 0.18);
}
/* The same accent-washed title bar as the miller columns. */
.tabs-chrome--title {
  background: color-mix(in srgb, var(--datalib-accent) 16%, transparent);
  border-bottom-color: color-mix(in srgb, var(--datalib-accent) 55%, transparent);
  color: color-mix(in srgb, var(--datalib-accent) 70%, var(--datalib-fg));
}
.tabs-from {
  flex: 0 1 auto;
  max-width: 14rem;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 12px;
  line-height: 18px;
  padding: 0.2rem 0.4rem;
  border: 1px solid color-mix(in srgb, currentColor 30%, transparent);
  border-radius: 3px;
  background: transparent;
  color: inherit;
  cursor: pointer;
}
.tabs-from:hover {
  background: var(--datalib-hover);
}
.tabs-source {
  flex: 1 1 auto;
  font:
    12px/1.5 ui-monospace,
    Menlo,
    monospace;
  padding: 0.2rem 0.4rem;
  border: none;
  border-radius: 3px;
  background: transparent;
  color: inherit;
  min-width: 0;
  resize: none;
  overflow: hidden;
  white-space: pre-wrap;
  overflow-wrap: break-word;
  box-sizing: border-box;
  display: block;
}
.tabs-source:focus {
  outline: none;
}
.tabs-title {
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
.tabs-card {
  flex: 1 1 auto;
  min-height: 0;
}
</style>
