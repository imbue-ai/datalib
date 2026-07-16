<script setup lang="ts">
// Recursive renderer for the tile tree (see TilingView.vue and
// tilingTree.ts) — structure and chrome only. A leaf renders the card
// chrome plus an empty slot div the host teleports the card into (the
// card itself lives in the host's persistent pool, never here, so it
// isn't remounted when the tree restructures). A container renders a
// bar (grip + h/v/tab switch) over its children, with a draggable
// divider between each pair and an "add" button at its end. Both leaves
// and (non-root) containers carry a grip strip you can drag to reparent
// them. Everything structural — slot registration, source edits, close,
// resize, tabs, arrangement, add, drag — comes from the host through
// the injected TilingApi, so this component only takes a `node` prop
// and recurses.
import { inject } from "vue";
import CardControls from "@/components/CardControls.vue";
import { growSourceBox, vAutoGrow } from "@/components/autoGrow";
import { devMode } from "@/devMode";
import { TILING_API } from "./tilingApi";
import type { TileNode, TileSplit } from "./tilingTree";

defineProps<{ node: TileNode }>();
const api = inject(TILING_API)!;

// In the container branch `node` is a TileSplit, but the template type
// checker doesn't narrow the prop across the v-if/v-else; this casts it
// back for the resize / tab / arrangement handlers.
const asSplit = (n: TileNode) => n as TileSplit;

// Tab-bar label for a child: the card's source (dev mode) or its
// human-readable title for a tile, or a generic marker for a nested
// group.
const tabLabel = (child: TileNode) =>
  child.kind === "leaf"
    ? devMode.value
      ? child.source.trim() || "blank"
      : api.titleFor(child)
    : "group";
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
    <div class="tiling-chrome" :class="{ 'tiling-chrome--title': !devMode }">
      <textarea
        v-if="devMode"
        v-auto-grow
        class="tiling-source"
        rows="1"
        :value="node.source"
        spellcheck="false"
        placeholder="card source — e.g. documentView(&quot;uuid&quot;), Enter to run"
        @input="growSourceBox($event.target as HTMLTextAreaElement)"
        @keydown.enter.exact.prevent="api.commitSource(node, $event)"
      />
      <div v-else class="tiling-title">{{ api.titleFor(node) }}</div>
      <CardControls :source="node.source" :ctx="api.ctxFor(node)" />
    </div>
    <!-- Empty slot: the host teleports this leaf's persistent card here
         (keyed by id), so it isn't remounted when the tree restructures
         around it. -->
    <div
      class="tiling-card"
      :ref="(el) => api.setSlot(node.id, el as HTMLElement | null)"
    />
  </section>

  <!-- Container (any arrangement). One rendering for h / v / tab: the
       child <TilingNode>s live in the same v-for under the same body
       element regardless of `dir`, so `dir` only changes the body's
       flex direction, whether a tab bar / dividers show, and which
       children are visible (v-show). Switching arrangement therefore
       never remounts a child card — it keeps its DOM, its state, and
       (crucially) doesn't re-run a grid card's selection restore. -->
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
        <button
          :class="{ 'is-active': node.dir === 'tab' }"
          title="lay out as tabs"
          @click="api.setDir(asSplit(node), 'tab')"
        >
          ▭
        </button>
      </div>
    </div>

    <!-- Tab bar — only in tab mode. The active child shows in the body
         below; the rest stay mounted but hidden. -->
    <div v-if="node.dir === 'tab'" class="tiling-tabbar" role="tablist">
      <div
        v-for="(child, i) in node.children"
        :key="child.id"
        class="tiling-tab"
        :class="{ 'is-active': i === (node.active ?? 0) }"
        role="tab"
        :aria-selected="i === (node.active ?? 0)"
        @click="api.setActive(asSplit(node), i)"
      >
        <span
          class="tiling-tab-label"
          :class="{ 'tiling-tab-label--title': !devMode }"
          >{{ tabLabel(child) }}</span
        >
        <button
          class="tiling-tab-close"
          title="close tab"
          @click.stop="api.closeNode(child.id)"
        >
          ✕
        </button>
      </div>
      <!-- Blank cards are only usable through the source box, so the
           add buttons (here and below) are dev-mode furniture. -->
      <div
        v-if="devMode"
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

    <div class="tiling-body" :class="`tiling-body--${node.dir}`">
      <template v-for="(child, i) in node.children" :key="child.id">
        <TilingNode
          v-show="node.dir !== 'tab' || i === (node.active ?? 0)"
          :node="child"
          :style="node.dir === 'tab' ? { flex: '1 1 0' } : { flexGrow: child.weight }"
        />
        <div
          v-if="node.dir !== 'tab' && i < node.children.length - 1"
          class="tiling-divider"
          role="separator"
          :aria-orientation="node.dir === 'h' ? 'vertical' : 'horizontal'"
          @pointerdown="(e) => api.startResize(asSplit(node), i, e)"
        />
      </template>
      <div
        v-if="node.dir !== 'tab' && devMode"
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
.tiling-split {
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
.tiling-split {
  box-sizing: border-box;
  display: flex;
  flex-direction: column;
  padding: 4px;
  border: 2px solid var(--tiling-edge);
  border-radius: 6px;
}
.tiling-body {
  flex: 1 1 auto;
  display: flex;
  min-width: 0;
  min-height: 0;
}
.tiling-body--h {
  flex-direction: row;
}
.tiling-body--v {
  flex-direction: column;
}
/* Tab mode: only the active child is shown (v-show), and it fills. */
.tiling-body--tab {
  flex-direction: row;
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
.tiling-split.is-dragging {
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
/* Non-dev tabs carry titles, not source — drop the tab bar's
   monospace for them. */
.tiling-tab-label--title {
  font-family:
    system-ui,
    -apple-system,
    sans-serif;
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
.tiling-body--h > .tiling-divider {
  width: 8px;
  margin: 0 -4px;
  cursor: col-resize;
}
.tiling-body--h > .tiling-divider::before {
  top: 0;
  bottom: 0;
  left: 50%;
  width: 1px;
  transform: translateX(-0.5px);
}
.tiling-body--v > .tiling-divider {
  height: 8px;
  margin: -4px 0;
  cursor: row-resize;
}
.tiling-body--v > .tiling-divider::before {
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
/* Non-dev: accent-washed title bar, title inked in the accent (see
   MillerView for the mixing rationale). */
.tiling-chrome--title {
  background: color-mix(in srgb, var(--fw-accent) 16%, transparent);
  border-bottom-color: color-mix(in srgb, var(--fw-accent) 55%, transparent);
  color: color-mix(in srgb, var(--fw-accent) 70%, var(--fw-fg));
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
/* Non-dev chrome: the card's human-readable title where the source
   box would be. Styled as a heading (proportional, semibold) so it
   reads as a title, not code; the 18px line box matches the source
   box's 12px × 1.5 so toggling dev mode doesn't reflow the bar. */
.tiling-title {
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
/* Slot the host teleports the card into; a flex container so the
   mounted card fills it. */
.tiling-card {
  flex: 1 1 auto;
  min-height: 0;
  display: flex;
}
</style>
