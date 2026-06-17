<script setup lang="ts">
// Recursive renderer for the tile tree (see TilingView.vue and
// tilingTree.ts). A leaf renders the card chrome + ShadowCard; a
// container renders a bar (grip + h/v/tab switch) over its children,
// with a draggable divider between each pair and an "add" button at
// its end. Both leaves and (non-root) containers carry a grip strip
// you can drag to reparent them. Everything structural — ctx, source
// edits, close, resize, tabs, arrangement, add, drag — comes from the
// host through the injected TilingApi, so this component only takes a
// `node` prop and recurses.
import { inject } from "vue";
import ShadowCard from "@/components/ShadowCard.vue";
import { growSourceBox, vAutoGrow } from "@/components/autoGrow";
import { TILING_API } from "./tilingApi";
import type { TileNode, TileSplit } from "./tilingTree";

defineProps<{ node: TileNode }>();
const api = inject(TILING_API)!;

// In the container branches `node` is a TileSplit, but the template
// type checker doesn't narrow the prop across v-if/v-else-if/v-else;
// this casts it back for the resize / tab / arrangement handlers.
const asSplit = (n: TileNode) => n as TileSplit;

// Tab-bar label for a child: the card's source for a tile, or a
// generic marker for a nested group.
const tabLabel = (child: TileNode) =>
  child.kind === "leaf" ? child.source.trim() || "blank" : "group";
</script>

<template>
  <!-- Leaf: one card. The whole leaf is a drop target (drop a node on
       it to split it); the grip strip drags this card. -->
  <section
    v-if="node.kind === 'leaf'"
    class="tiling-leaf"
    :class="{
      'is-dragging': api.isDragging(node.id),
      'is-drop': api.isLeafDrop(node.id),
    }"
    :data-tiling-leaf="node.id"
  >
    <div
      class="tiling-grip"
      title="drag to move this card"
      @pointerdown="(e) => api.startDrag(node.id, e)"
    />
    <div class="tiling-chrome">
      <textarea
        v-auto-grow
        class="tiling-source"
        rows="1"
        :value="node.source"
        spellcheck="false"
        placeholder="card source — e.g. documentView(&quot;uuid&quot;), Enter to run"
        @input="growSourceBox($event.target as HTMLTextAreaElement)"
        @keydown.enter.exact.prevent="api.commitSource(node, $event)"
      />
      <a
        v-if="node.source.trim() !== ''"
        class="tiling-alone"
        :href="api.aloneHref(node)"
        target="_blank"
        rel="noopener"
        title="open this card alone"
        >↗</a
      >
      <button class="tiling-close" title="close tile" @click="api.closeNode(node.id)">
        ✕
      </button>
    </div>
    <ShadowCard class="tiling-card" :source="node.source" :ctx="api.ctxFor(node)" />
  </section>

  <!-- Tab group: a bar (grip + arrangement switch), then a tab bar,
       then the active child; the rest stay mounted (v-show) so
       switching tabs preserves their cards. -->
  <div
    v-else-if="node.dir === 'tab'"
    class="tiling-tabs"
    :class="{ 'is-dragging': api.isDragging(node.id) }"
  >
    <div class="tiling-cbar">
      <div
        v-if="!api.isRoot(node.id)"
        class="tiling-grip tiling-grip--container"
        title="drag to move this group"
        @pointerdown="(e) => api.startDrag(node.id, e)"
      />
      <div v-else class="tiling-cbar-spacer" />
      <div class="tiling-dirs" role="group" aria-label="container layout">
        <button title="lay out horizontally" @click="api.setDir(asSplit(node), 'h')">
          ⬌
        </button>
        <button title="lay out vertically" @click="api.setDir(asSplit(node), 'v')">
          ⬍
        </button>
        <button class="is-active" title="lay out as tabs" @click="api.setDir(asSplit(node), 'tab')">
          ▭
        </button>
      </div>
    </div>
    <div class="tiling-tabbar" role="tablist">
      <div
        v-for="(child, i) in node.children"
        :key="child.id"
        class="tiling-tab"
        :class="{ 'is-active': i === (node.active ?? 0) }"
        role="tab"
        :aria-selected="i === (node.active ?? 0)"
        @click="api.setActive(asSplit(node), i)"
      >
        <span class="tiling-tab-label">{{ tabLabel(child) }}</span>
        <button
          class="tiling-tab-close"
          title="close tab"
          @click.stop="api.closeNode(child.id)"
        >
          ✕
        </button>
      </div>
      <div
        class="tiling-add tiling-add--tab"
        :class="{ 'is-drop': api.isAddDrop(node.id) }"
        :data-tiling-add="node.id"
        role="button"
        title="add a tab here"
        @click="api.addCard(node.id)"
      >
        ＋
      </div>
    </div>
    <div class="tiling-tab-bodies">
      <TilingNode
        v-for="(child, i) in node.children"
        v-show="i === (node.active ?? 0)"
        :key="child.id"
        :node="child"
        :style="{ flex: '1 1 0' }"
      />
    </div>
  </div>

  <!-- Split: a bordered container — a bar (grip + arrangement switch),
       then the children laid out along `dir` with dividers, then an
       add button. -->
  <div
    v-else
    class="tiling-split"
    :class="{ 'is-dragging': api.isDragging(node.id) }"
  >
    <div class="tiling-cbar">
      <div
        v-if="!api.isRoot(node.id)"
        class="tiling-grip tiling-grip--container"
        title="drag to move this group"
        @pointerdown="(e) => api.startDrag(node.id, e)"
      />
      <div v-else class="tiling-cbar-spacer" />
      <div class="tiling-dirs" role="group" aria-label="container layout">
        <button
          :class="{ 'is-active': node.dir === 'h' }"
          title="lay out horizontally"
          @click="api.setDir(asSplit(node), 'h')"
        >
          ⬌
        </button>
        <button
          :class="{ 'is-active': node.dir === 'v' }"
          title="lay out vertically"
          @click="api.setDir(asSplit(node), 'v')"
        >
          ⬍
        </button>
        <button title="lay out as tabs" @click="api.setDir(asSplit(node), 'tab')">
          ▭
        </button>
      </div>
    </div>
    <div
      class="tiling-split-body"
      :class="node.dir === 'h' ? 'tiling-split-body--h' : 'tiling-split-body--v'"
    >
      <template v-for="(child, i) in node.children" :key="child.id">
        <TilingNode :node="child" :style="{ flexGrow: child.weight }" />
        <div
          v-if="i < node.children.length - 1"
          class="tiling-divider"
          role="separator"
          :aria-orientation="node.dir === 'h' ? 'vertical' : 'horizontal'"
          @pointerdown="(e) => api.startResize(asSplit(node), i, e)"
        />
      </template>
      <div
        class="tiling-add"
        :class="[
          node.dir === 'h' ? 'tiling-add--h' : 'tiling-add--v',
          { 'is-drop': api.isAddDrop(node.id) },
        ]"
        :data-tiling-add="node.id"
        role="button"
        :title="node.dir === 'h' ? 'add a card to the right' : 'add a card below'"
        @click="api.addCard(node.id)"
      >
        ＋
      </div>
    </div>
  </div>
</template>

<style scoped>
/* `--tiling-edge` is the shared "chrome" color: container borders and
   the drag grips all use it, so they read as one family. A muted brown
   (not the blue accent, which looked like a text selection). */
.tiling-leaf,
.tiling-split,
.tiling-tabs {
  --tiling-edge: #9c6b43;
  flex-basis: 0;
  min-width: 0;
  min-height: 0;
}
.tiling-leaf {
  position: relative;
  display: flex;
  flex-direction: column;
  box-sizing: border-box;
  background: var(--fw-bg);
  border: 1px solid #888;
  border-radius: 4px;
  overflow: hidden;
}
/* Containers (internal nodes) get a visible brown border and a little
   inset padding, so the nesting structure reads at a glance — each box
   sits just inside its parent. */
.tiling-split,
.tiling-tabs {
  box-sizing: border-box;
  display: flex;
  flex-direction: column;
  padding: 4px;
  border: 2px solid var(--tiling-edge);
  border-radius: 6px;
}
.tiling-split-body {
  flex: 1 1 auto;
  display: flex;
  min-width: 0;
  min-height: 0;
}
.tiling-split-body--h {
  flex-direction: row;
}
.tiling-split-body--v {
  flex-direction: column;
}

/* Container bar: drag grip (or a spacer on the root) + the h/v/tab
   arrangement switch. */
.tiling-cbar {
  flex: 0 0 auto;
  display: flex;
  align-items: stretch;
  gap: 4px;
  height: 16px;
  margin-bottom: 4px;
}
.tiling-cbar-spacer {
  flex: 1 1 auto;
}
.tiling-dirs {
  flex: 0 0 auto;
  display: flex;
  gap: 1px;
}
.tiling-dirs button {
  border: none;
  background: transparent;
  color: inherit;
  opacity: 0.5;
  cursor: pointer;
  font-size: 11px;
  line-height: 1;
  padding: 0 5px;
  border-radius: 2px;
}
.tiling-dirs button:hover {
  opacity: 0.85;
  background: var(--fw-hover);
}
.tiling-dirs button.is-active {
  opacity: 1;
  background: color-mix(in srgb, var(--tiling-edge) 28%, transparent);
}

/* Drag grips — a thin strip with a brown handle pill, matching the
   container borders. */
.tiling-grip {
  flex: 0 0 auto;
  height: 9px;
  display: flex;
  align-items: center;
  justify-content: center;
  cursor: grab;
  background: rgba(0, 0, 0, 0.04);
}
.tiling-grip::before {
  content: "";
  width: 28px;
  height: 3px;
  border-radius: 2px;
  background: var(--tiling-edge);
}
.tiling-grip:hover {
  background: rgba(0, 0, 0, 0.1);
}
.tiling-grip:active {
  cursor: grabbing;
}
.tiling-grip--container {
  flex: 1 1 auto;
  height: auto;
  border-radius: 3px;
}
.tiling-grip--container::before {
  width: 44px;
}

/* Dimmed while being dragged; highlighted while a drop would land
   (the accent only appears mid-drag, so it can't be mistaken for a
   persistent border). */
.tiling-leaf.is-dragging,
.tiling-split.is-dragging,
.tiling-tabs.is-dragging {
  opacity: 0.4;
}
.tiling-leaf.is-drop {
  outline: 2px solid var(--fw-accent);
  outline-offset: -2px;
}

/* Add button — a card-creating drop zone at the container's end. */
.tiling-add {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  justify-content: center;
  cursor: pointer;
  user-select: none;
  font-size: 1rem;
  line-height: 1;
  color: color-mix(in srgb, var(--fw-fg) 45%, transparent);
  border: 1px dashed color-mix(in srgb, var(--fw-fg) 22%, transparent);
  border-radius: 3px;
  box-sizing: border-box;
}
.tiling-add:hover {
  color: var(--fw-fg);
  background: var(--fw-hover);
}
.tiling-add.is-drop {
  color: var(--fw-bg);
  background: var(--fw-accent);
  border-color: var(--fw-accent);
}
.tiling-add--h {
  width: 22px;
  align-self: stretch;
  margin-left: 4px;
}
.tiling-add--v {
  height: 22px;
  margin-top: 4px;
}
.tiling-add--tab {
  width: 26px;
  align-self: stretch;
  border: none;
  border-radius: 0;
}

/* Tab group internals. */
.tiling-tabbar {
  flex: 0 0 auto;
  display: flex;
  align-items: stretch;
  gap: 1px;
  overflow-x: auto;
  background: rgba(0, 0, 0, 0.08);
  border-radius: 3px 3px 0 0;
}
.tiling-tab {
  display: flex;
  align-items: center;
  gap: 0.3rem;
  max-width: 16rem;
  padding: 0.25rem 0.5rem;
  cursor: pointer;
  font: 12px/1.4 ui-monospace, Menlo, monospace;
  border-right: 1px solid color-mix(in srgb, #888 50%, transparent);
  opacity: 0.65;
  white-space: nowrap;
}
.tiling-tab:hover {
  opacity: 0.85;
}
.tiling-tab.is-active {
  opacity: 1;
  background: var(--fw-bg);
}
.tiling-tab-label {
  overflow: hidden;
  text-overflow: ellipsis;
}
.tiling-tab-close {
  flex: 0 0 auto;
  border: none;
  background: transparent;
  color: inherit;
  opacity: 0.5;
  cursor: pointer;
  font-size: 0.72rem;
  line-height: 1;
  padding: 0;
}
.tiling-tab-close:hover {
  opacity: 1;
}
.tiling-tab-bodies {
  flex: 1 1 auto;
  display: flex;
  min-height: 0;
}

/* Divider: a thin transparent grab strip with a centered hairline.
   Negative margins let it straddle the gap between tiles without
   stealing layout space from them. */
.tiling-divider {
  flex: 0 0 auto;
  position: relative;
  z-index: 2;
}
.tiling-divider::before {
  content: "";
  position: absolute;
  background: #888;
}
.tiling-split-body--h > .tiling-divider {
  width: 8px;
  margin: 0 -4px;
  cursor: col-resize;
}
.tiling-split-body--h > .tiling-divider::before {
  top: 0;
  bottom: 0;
  left: 50%;
  width: 1px;
  transform: translateX(-0.5px);
}
.tiling-split-body--v > .tiling-divider {
  height: 8px;
  margin: -4px 0;
  cursor: row-resize;
}
.tiling-split-body--v > .tiling-divider::before {
  left: 0;
  right: 0;
  top: 50%;
  height: 1px;
  transform: translateY(-0.5px);
}
.tiling-divider:hover::before {
  background: var(--fw-accent);
}

.tiling-chrome {
  flex: 0 0 auto;
  display: flex;
  align-items: flex-start;
  gap: 0.4rem;
  padding: 0.3rem 0.5rem;
  border-bottom: 1px solid #888;
  background: rgba(0, 0, 0, 0.08);
}
.tiling-chrome:focus-within {
  background: rgba(99, 102, 241, 0.18);
}
.tiling-source {
  flex: 1 1 auto;
  font: 12px/1.5 ui-monospace, Menlo, monospace;
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
.tiling-source:focus {
  outline: none;
}
.tiling-alone,
.tiling-close {
  flex: 0 0 auto;
  border: none;
  background: transparent;
  color: inherit;
  opacity: 0.6;
  cursor: pointer;
  font-size: 0.8rem;
  line-height: 1.5;
  text-decoration: none;
  padding: 0.2rem 0;
}
.tiling-alone:hover,
.tiling-close:hover {
  opacity: 1;
}
.tiling-card {
  flex: 1 1 auto;
  min-height: 0;
}
</style>
