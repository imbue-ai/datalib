<script setup lang="ts">
// Routed view for the card surface. Owns the chrome the layouts
// share — the dev and layout toggles along the bottom — and keeps
// each layout host alive across toggles (v-show, not v-if) so
// switching back doesn't lose its cards. The data root and its size
// are the app-wide `RootStorageBar` below this; a grid card carries
// its own row count.
import { onBeforeUnmount, onMounted, ref, useTemplateRef } from "vue";
import MillerView from "@/views/MillerView.vue";
import TreeView from "@/views/TreeView.vue";
import TilingView from "@/views/TilingView.vue";
import { devMode } from "@/devMode";
import { surface, type SurfaceCommands } from "@/surface";

type Layout = "columns" | "tree" | "tiling";
const layout = ref<Layout>("columns");
const treeMounted = ref(false);
const tilingMounted = ref(false);

function setLayout(next: Layout) {
  layout.value = next;
  if (next === "tree") treeMounted.value = true;
  if (next === "tiling") tilingMounted.value = true;
}

// The toolbar's commands go to whichever layout is showing.
const miller = useTemplateRef<SurfaceCommands>("miller");
const tree = useTemplateRef<SurfaceCommands>("tree");
const tiling = useTemplateRef<SurfaceCommands>("tiling");
function active(): SurfaceCommands | null {
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
    <MillerView ref="miller" v-show="layout === 'columns'" />
    <TreeView v-if="treeMounted" ref="tree" v-show="layout === 'tree'" />
    <TilingView v-if="tilingMounted" ref="tiling" v-show="layout === 'tiling'" />
    <div class="cards-statusbar">
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
  padding: 0.15rem 0.8rem;
  border-top: 1px solid #888;
  background: rgba(0, 0, 0, 0.08);
  font-size: 0.8rem;
  opacity: 0.85;
  min-height: 1.5rem;
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
  font-size: 0.75rem;
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
  font-size: 0.75rem;
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
