<script setup lang="ts">
// Routed view for the card surface. Owns the chrome the layouts
// share — the status bar along the bottom: the data root and its
// size, the log, then the dev and layout toggles flush right — and
// keeps each layout host alive across toggles (v-show, not v-if) so
// switching back doesn't lose its cards. A grid card carries its own
// row count. Which layout is showing is remembered in this browser.
import { onBeforeUnmount, onMounted, ref, useTemplateRef } from "vue";
import MillerView from "@/views/MillerView.vue";
import TreeView from "@/views/TreeView.vue";
import TilingView from "@/views/TilingView.vue";
import TabsView from "@/views/TabsView.vue";
import RootStorageBar from "@/components/RootStorageBar.vue";
import { devMode } from "@/devMode";
import { LOG_CARD, surface, type SurfaceCommands } from "@/surface";

const LAYOUTS = ["columns", "tabs", "tree", "tiling"] as const;
type Layout = (typeof LAYOUTS)[number];
const LAYOUT_KEY = "datalib-layout";

function storedLayout(): Layout {
  try {
    const s = localStorage.getItem(LAYOUT_KEY);
    return LAYOUTS.find((l) => l === s) ?? "columns";
  } catch {
    return "columns";
  }
}

const layout = ref<Layout>("columns");
const tabsMounted = ref(false);
const treeMounted = ref(false);
const tilingMounted = ref(false);

function setLayout(next: Layout) {
  layout.value = next;
  if (next === "tabs") tabsMounted.value = true;
  if (next === "tree") treeMounted.value = true;
  if (next === "tiling") tilingMounted.value = true;
  try {
    localStorage.setItem(LAYOUT_KEY, next);
  } catch {
    // Blocked storage: the choice lasts as long as the page.
  }
}

// The tabs layout opens the URL it loads on; mounted later, it keeps
// the URL the columns layout wrote out of its tabs.
const initialLayout = storedLayout();
setLayout(initialLayout);

// The toolbar's commands go to whichever layout is showing.
const miller = useTemplateRef<SurfaceCommands>("miller");
const tabs = useTemplateRef<SurfaceCommands>("tabs");
const tree = useTemplateRef<SurfaceCommands>("tree");
const tiling = useTemplateRef<SurfaceCommands>("tiling");
function active(): SurfaceCommands | null {
  if (layout.value === "tabs") return tabs.value;
  if (layout.value === "tree") return tree.value;
  if (layout.value === "tiling") return tiling.value;
  return miller.value;
}
onMounted(() => {
  surface.value = {
    addCard: () => active()?.addCard(),
    showCard: (source) => active()?.showCard(source),
  };
});
onBeforeUnmount(() => {
  surface.value = null;
});
</script>

<template>
  <div class="cards-root">
    <MillerView ref="miller" v-show="layout === 'columns'" :active="layout === 'columns'" />
    <TabsView
      v-if="tabsMounted"
      ref="tabs"
      v-show="layout === 'tabs'"
      :active="layout === 'tabs'"
      :open-url-on-mount="initialLayout === 'tabs'"
    />
    <TreeView v-if="treeMounted" ref="tree" v-show="layout === 'tree'" />
    <TilingView v-if="tilingMounted" ref="tiling" v-show="layout === 'tiling'" />
    <div class="cards-statusbar">
      <RootStorageBar />
      <!-- What the system is doing belongs down here with the data
           root's size: the log over every run, revealed if a card
           already shows it. -->
      <button
        class="cards-logs"
        title="the run log: every line the runner, the steps and the server wrote"
        @click="active()?.showCard(LOG_CARD)"
      >
        Logs
      </button>
      <button
        class="cards-dev-toggle"
        :class="{ 'is-active': devMode }"
        :aria-pressed="devMode"
        title="dev mode: show and edit each card's source"
        @click="devMode = !devMode"
      >
        dev
      </button>
      <div class="cards-layout-toggle" role="group" aria-label="card layout">
        <button
          :class="{ 'is-active': layout === 'columns' }"
          title="miller columns (synced to the URL)"
          @click="setLayout('columns')"
        >
          columns
        </button>
        <button
          :class="{ 'is-active': layout === 'tabs' }"
          title="one card at a time, with a tree of every open card beside it, each under the card that opened it (kept in this browser)"
          @click="setLayout('tabs')"
        >
          tabs
        </button>
        <button
          :class="{ 'is-active': layout === 'tree' }"
          title="2D tree (in-memory only, not in the URL)"
          @click="setLayout('tree')"
        >
          tree
        </button>
        <button
          :class="{ 'is-active': layout === 'tiling' }"
          title="tiling window manager (in-memory only, not in the URL)"
          @click="setLayout('tiling')"
        >
          tiling
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.cards-root {
  display: flex;
  flex-direction: column;
  /* Fill whatever the shell's flex layout gives us (everything below
     the header); basis 0 + min-height 0 so intrinsic content height
     can't stretch the page. Negative margins bleed over the shell's
     1rem padding on the right and bottom so the status bar sits
     flush with the viewport bottom; the shell's left padding stays
     as a gutter. */
  flex: 1 1 0;
  min-height: 0;
  margin: 0 -1rem -1rem 0;
}
.cards-statusbar {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: 0.6rem;
  /* Bleed over the shell's left padding: the cards keep their gutter,
     but the status bar spans the full viewport width. */
  margin-left: -1rem;
  padding: 0.25rem 1rem;
  border-top: 1px solid var(--datalib-border);
  background: var(--datalib-card-bg);
  font-size: 12px;
  min-height: 1.5rem;
}
.cards-logs {
  flex: 0 0 auto;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
  background: transparent;
  color: inherit;
  cursor: pointer;
  font-size: 12px;
  padding: 0.1rem 0.5rem;
}
.cards-logs:hover {
  background: var(--datalib-hover);
}
/* The dev + layout toggles sit flush right as a cluster — the dev
   button carries the auto margin. */
.cards-dev-toggle {
  flex: 0 0 auto;
  margin-left: auto;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
  background: transparent;
  color: inherit;
  cursor: pointer;
  font-size: 12px;
  padding: 0.1rem 0.5rem;
}
.cards-dev-toggle:hover {
  background: var(--datalib-hover);
}
.cards-dev-toggle.is-active {
  background: var(--datalib-accent);
  color: var(--datalib-bg);
}
.cards-layout-toggle {
  /* Always claim full intrinsic width (never shrink). */
  flex: 0 0 auto;
  display: flex;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
  overflow: hidden;
}
.cards-layout-toggle button {
  border: none;
  background: transparent;
  color: inherit;
  cursor: pointer;
  font-size: 12px;
  padding: 0.1rem 0.5rem;
}
.cards-layout-toggle button + button {
  border-left: 1px solid var(--datalib-border);
}
.cards-layout-toggle button:hover {
  background: var(--datalib-hover);
}
.cards-layout-toggle button.is-active {
  background: var(--datalib-accent);
  color: var(--datalib-bg);
}
</style>
