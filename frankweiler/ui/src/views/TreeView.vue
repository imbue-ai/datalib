<script setup lang="ts">
// Tree layout host: cards are nodes on a pannable/zoomable 2D plane.
// The card contract is identical to the miller layout's — same
// source-in-header chrome, same ShadowCard mounting, same CardCtx —
// only `ctx.host.openCards(source)` behaves differently: instead of
// replacing everything to the right it spawns a child node the caller
// points to (and `openCards(a, b, …)` spawns a parent→child spine).
// Closing a node closes its whole subtree (children would be
// orphaned otherwise).
//
// Positions are auto-layout first: every open/close/resize re-runs
// the tidy-tree layout (treeLayout.ts) and nodes animate to their new
// spots. Dragging a node by its title bar stores a manual offset ON
// TOP of its layout position; descendants inherit ancestor offsets,
// so dragging moves the whole subtree and later reflows preserve the
// deviation instead of snapping it back. Nodes resize from any of
// their four corners. Gestures follow the design-tool (Figma/tldraw)
// convention:
// wheel / two-finger scroll pans, ctrl-or-cmd+wheel and trackpad
// pinch zoom toward the cursor, space+drag / middle-drag / background
// drag pan. A wheel over a card is left alone so the card's own
// content (grid, document) keeps scrolling.
//
// Unlike the miller layout there is NO URL sync: the tree is
// in-memory only and lost on reload. Cards are also not carried
// across when toggling layouts (see CardsView).
import { computed, reactive, ref, nextTick, useTemplateRef, onMounted, onBeforeUnmount } from "vue";
import ShadowCard from "@/components/ShadowCard.vue";
import { growSourceBox, vAutoGrow } from "@/components/autoGrow";
import { createBus } from "@/cards/bus";
import { encodeColumns } from "@/router/columns";
import { layoutTree, type Rect } from "./treeLayout";
import type { CardCtx, HostCommands } from "@/cards/types";
import { handOffToAgent } from "@/cards/handoff";

const bus = createBus();

type TreeNode = {
  id: string;
  source: string;
  // Opaque per-card state string (see HostCommands.setState). Kept
  // in memory only — the tree layout has no URL to put it in.
  state: string;
  parentId: string | null;
  width: number;
  height: number;
  // Manual drag offset, applied on top of the computed layout
  // position. Descendants inherit ancestor offsets (see rects).
  dx: number;
  dy: number;
};

const NODE_WIDTH = 640;
const NODE_HEIGHT = 480;
const MIN_WIDTH = 240;
const MIN_HEIGHT = 160;
const H_GAP = 100;
const V_GAP = 32;
const MIN_ZOOM = 0.1;
const MAX_ZOOM = 2;

let nextId = 1;
function newNode(source: string, parentId: string | null): TreeNode {
  return {
    id: `card-${nextId++}`,
    source,
    state: "",
    parentId,
    width: NODE_WIDTH,
    height: NODE_HEIGHT,
    dx: 0,
    dy: 0,
  };
}

const nodes = ref<TreeNode[]>([newNode("gridView()", null)]);

const layoutRects = computed(() =>
  layoutTree(nodes.value, { hGap: H_GAP, vGap: V_GAP }),
);

// Layout rects shifted by each node's manual drag offset plus all of
// its ancestors' — so dragging a node carries its subtree along.
const rects = computed(() => {
  const byId = new Map(nodes.value.map((n) => [n.id, n]));
  const offsets = new Map<string, { x: number; y: number }>();
  function offsetOf(id: string): { x: number; y: number } {
    let o = offsets.get(id);
    if (o) return o;
    const n = byId.get(id)!;
    const p =
      n.parentId !== null && byId.has(n.parentId)
        ? offsetOf(n.parentId)
        : { x: 0, y: 0 };
    o = { x: p.x + n.dx, y: p.y + n.dy };
    offsets.set(id, o);
    return o;
  }
  const out = new Map<string, Rect>();
  for (const [id, r] of layoutRects.value) {
    const o = offsetOf(id);
    out.set(id, { x: r.x + o.x, y: r.y + o.y, width: r.width, height: r.height });
  }
  return out;
});

function rectOf(id: string): Rect {
  // Every node the template iterates is in the layout; the fallback
  // only guards a transient render between list and rects updates.
  return (
    rects.value.get(id) ?? { x: 0, y: 0, width: NODE_WIDTH, height: NODE_HEIGHT }
  );
}

// Parent→child connectors: one cubic bezier per non-root node, from
// the right-center of the parent to the left-center of the child
// with horizontal tangents — the standard node-editor "noodle".
const edges = computed(() => {
  const out: { id: string; d: string }[] = [];
  for (const n of nodes.value) {
    if (n.parentId === null) continue;
    const p = rects.value.get(n.parentId);
    const c = rects.value.get(n.id);
    if (!p || !c) continue;
    const x1 = p.x + p.width;
    const y1 = p.y + p.height / 2;
    const x2 = c.x;
    const y2 = c.y + c.height / 2;
    const k = Math.max(40, (x2 - x1) / 2);
    out.push({
      id: n.id,
      d: `M ${x1} ${y1} C ${x1 + k} ${y1}, ${x2 - k} ${y2}, ${x2} ${y2}`,
    });
  }
  return out;
});

// ---- viewport (pan/zoom) ----

const viewportEl = useTemplateRef<HTMLDivElement>("viewportEl");
// Plane transform: screen = plane * zoom + pan. Start with a small
// margin so the root card isn't glued to the corner.
const pan = reactive({ x: 48, y: 24 });
const zoom = ref(1);
const panning = ref(false);
const spaceHeld = ref(false);

// Zoom to `next`, keeping the plane point under viewport coordinates
// (cx, cy) fixed on screen.
function zoomAt(next: number, cx: number, cy: number) {
  const z = Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, next));
  pan.x = cx - ((cx - pan.x) / zoom.value) * z;
  pan.y = cy - ((cy - pan.y) / zoom.value) * z;
  zoom.value = z;
}

function viewportCenter(): { cx: number; cy: number } {
  const el = viewportEl.value!;
  return { cx: el.clientWidth / 2, cy: el.clientHeight / 2 };
}

function zoomStep(factor: number) {
  const { cx, cy } = viewportCenter();
  zoomAt(zoom.value * factor, cx, cy);
}

function zoomReset() {
  const { cx, cy } = viewportCenter();
  zoomAt(1, cx, cy);
}

function zoomToFit() {
  const el = viewportEl.value;
  if (!el || rects.value.size === 0) return;
  let x1 = Infinity, y1 = Infinity, x2 = -Infinity, y2 = -Infinity;
  for (const r of rects.value.values()) {
    x1 = Math.min(x1, r.x);
    y1 = Math.min(y1, r.y);
    x2 = Math.max(x2, r.x + r.width);
    y2 = Math.max(y2, r.y + r.height);
  }
  const M = 48; // screen-px margin around the content
  const z = Math.min(
    MAX_ZOOM,
    Math.max(
      MIN_ZOOM,
      Math.min((el.clientWidth - 2 * M) / (x2 - x1), (el.clientHeight - 2 * M) / (y2 - y1), 1),
    ),
  );
  zoom.value = z;
  pan.x = (el.clientWidth - (x2 - x1) * z) / 2 - x1 * z;
  pan.y = (el.clientHeight - (y2 - y1) * z) / 2 - y1 * z;
}

// Firefox reports mouse-wheel deltas in lines (deltaMode 1); normalize
// everything to pixels.
function wheelPixels(e: WheelEvent): { dx: number; dy: number } {
  const f = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? 360 : 1;
  return { dx: e.deltaX * f, dy: e.deltaY * f };
}

function onWheel(e: WheelEvent) {
  const el = viewportEl.value;
  if (!el) return;
  if (e.ctrlKey || e.metaKey) {
    // Trackpad pinch arrives as ctrl+wheel; preventDefault also stops
    // the browser's page zoom.
    e.preventDefault();
    const r = el.getBoundingClientRect();
    const { dy } = wheelPixels(e);
    zoomAt(zoom.value * Math.exp(-dy * 0.01), e.clientX - r.left, e.clientY - r.top);
    return;
  }
  // Plain wheel over a card scrolls the card's own content; over the
  // background it pans the plane.
  if (e.target instanceof Element && e.target.closest(".tree-node")) return;
  e.preventDefault();
  const { dx, dy } = wheelPixels(e);
  pan.x -= dx;
  pan.y -= dy;
}

function startPan(e: PointerEvent) {
  const el = viewportEl.value;
  if (!el) return;
  e.preventDefault();
  panning.value = true;
  const startX = e.clientX;
  const startY = e.clientY;
  const px = pan.x;
  const py = pan.y;
  el.setPointerCapture(e.pointerId);
  const onMove = (ev: PointerEvent) => {
    pan.x = px + ev.clientX - startX;
    pan.y = py + ev.clientY - startY;
  };
  const onUp = (ev: PointerEvent) => {
    panning.value = false;
    el.releasePointerCapture(ev.pointerId);
    el.removeEventListener("pointermove", onMove);
    el.removeEventListener("pointerup", onUp);
    el.removeEventListener("pointercancel", onUp);
  };
  el.addEventListener("pointermove", onMove);
  el.addEventListener("pointerup", onUp);
  el.addEventListener("pointercancel", onUp);
}

function onPointerDown(e: PointerEvent) {
  if (!(e.target instanceof Element)) return;
  // The controls overlay is buttons, never a pan start — capturing
  // the pointer on the viewport would swallow the button's click.
  if (e.target.closest(".tree-controls")) return;
  const overNode = !!e.target.closest(".tree-node");
  const middle = e.button === 1;
  const primary = e.button === 0;
  if (middle || (primary && spaceHeld.value) || (primary && !overNode)) {
    startPan(e);
  }
}

// Space held = pan with any drag (the e.target of keyboard events
// retargets to shadow hosts, so walk shadow roots to find the real
// focused element and leave typing alone).
function deepActiveElement(): Element | null {
  let el: Element | null = document.activeElement;
  while (el?.shadowRoot?.activeElement) el = el.shadowRoot.activeElement;
  return el;
}

function isEditing(): boolean {
  const el = deepActiveElement();
  return (
    el instanceof HTMLInputElement ||
    el instanceof HTMLTextAreaElement ||
    (el instanceof HTMLElement && el.isContentEditable)
  );
}

function visible(): boolean {
  // The view stays mounted (v-show) while the miller layout is
  // active; don't grab keys then.
  return viewportEl.value?.offsetParent != null;
}

function onKeyDown(e: KeyboardEvent) {
  if (e.code !== "Space" || e.repeat || !visible() || isEditing()) return;
  spaceHeld.value = true;
  e.preventDefault();
}

function onKeyUp(e: KeyboardEvent) {
  if (e.code === "Space") spaceHeld.value = false;
}

onMounted(() => {
  window.addEventListener("keydown", onKeyDown);
  window.addEventListener("keyup", onKeyUp);
});
onBeforeUnmount(() => {
  window.removeEventListener("keydown", onKeyDown);
  window.removeEventListener("keyup", onKeyUp);
});

// Pan just enough to bring a (new) node fully into view, with a small
// margin; if it's larger than the viewport, favor its top-left.
function revealNode(id: string) {
  const el = viewportEl.value;
  const r = rects.value.get(id);
  if (!el || !r) return;
  const z = zoom.value;
  const M = 24;
  const x1 = r.x * z + pan.x;
  const y1 = r.y * z + pan.y;
  const x2 = x1 + r.width * z;
  const y2 = y1 + r.height * z;
  let dx = 0;
  if (x2 > el.clientWidth - M) dx = el.clientWidth - M - x2;
  if (x1 + dx < M) dx = M - x1;
  let dy = 0;
  if (y2 > el.clientHeight - M) dy = el.clientHeight - M - y2;
  if (y1 + dy < M) dy = M - y1;
  pan.x += dx;
  pan.y += dy;
}

// ---- host commands ----

const ctxCache = new Map<string, CardCtx>();

function openCardFrom(parentId: string, source: string): string {
  const node = newNode(source, parentId);
  nodes.value = [...nodes.value, node];
  void nextTick(() => revealNode(node.id));
  return node.id;
}

// host.openCards: open a chain of cards. Each source hangs off the
// node the previous source produced, so the chain forms a parent→child
// spine descending from the caller. Returns the new node ids in order.
function openCardsFrom(parentId: string, sources: string[]): string[] {
  let prev = parentId;
  const ids: string[] = [];
  for (const source of sources) {
    prev = openCardFrom(prev, source);
    ids.push(prev);
  }
  return ids;
}

function addRootCard() {
  const node = newNode("", null);
  nodes.value = [...nodes.value, node];
  void nextTick(() => revealNode(node.id));
}

function closeNode(id: string) {
  const doomed = new Set([id]);
  let grew = true;
  while (grew) {
    grew = false;
    for (const n of nodes.value) {
      if (n.parentId !== null && doomed.has(n.parentId) && !doomed.has(n.id)) {
        doomed.add(n.id);
        grew = true;
      }
    }
  }
  nodes.value = nodes.value.filter((n) => !doomed.has(n.id));
  for (const d of doomed) ctxCache.delete(d);
}

function setNodeState(id: string, state: string) {
  const node = nodes.value.find((n) => n.id === id);
  if (node) node.state = state;
}

// host.setSource: replace this node's own source (clearing state) —
// drives the agent hand-off (see cards/handoff.ts).
function setNodeSource(id: string, source: string) {
  const node = nodes.value.find((n) => n.id === id);
  if (!node) return;
  node.source = source;
  node.state = "";
}

// Same memoization pattern as MillerView's ctxFor: stable identity
// for the lifetime of the node, initialState as a getter so a source
// re-run sees the latest saved state.
function ctxFor(node: TreeNode): CardCtx {
  let ctx = ctxCache.get(node.id);
  if (!ctx) {
    const cardId = node.id;
    const host: HostCommands = {
      openCards: (...sources) => openCardsFrom(cardId, sources),
      setSource: (source) => setNodeSource(cardId, source),
      close: () => closeNode(cardId),
      setState: (state) => setNodeState(cardId, state),
    };
    ctx = {
      cardId,
      get initialState() {
        return node.state;
      },
      bus,
      host,
    };
    ctxCache.set(cardId, ctx);
  }
  return ctx;
}

function commitSource(node: TreeNode, e: Event) {
  const next = (e.target as HTMLTextAreaElement).value;
  if (next !== node.source) {
    node.source = next;
    // New code means the old card's state no longer applies.
    node.state = "";
  }
}

// Standalone view: a miller URL containing just this card.
function aloneHref(node: TreeNode): string {
  return encodeColumns([{ code: node.source, state: node.state }]);
}

// True while a node is being moved or resized; pauses the re-layout
// position animation, which would otherwise trail the pointer by its
// transition duration (and drag a moved subtree behind its parent).
const nodeDragLive = ref(false);

// Shared pointer-capture drag plumbing for node moves and resizes.
function captureDrag(
  ev: PointerEvent,
  onMove: (mx: number, my: number) => void,
) {
  ev.preventDefault();
  nodeDragLive.value = true;
  const startX = ev.clientX;
  const startY = ev.clientY;
  const target = ev.currentTarget as HTMLElement;
  target.setPointerCapture(ev.pointerId);
  // Pointer deltas are screen px; divide by zoom to get plane px.
  const move = (e: PointerEvent) =>
    onMove((e.clientX - startX) / zoom.value, (e.clientY - startY) / zoom.value);
  const up = (e: PointerEvent) => {
    nodeDragLive.value = false;
    target.releasePointerCapture(e.pointerId);
    target.removeEventListener("pointermove", move);
    target.removeEventListener("pointerup", up);
    target.removeEventListener("pointercancel", up);
  };
  target.addEventListener("pointermove", move);
  target.addEventListener("pointerup", up);
  target.addEventListener("pointercancel", up);
}

// Drag any of a node's four corners to resize it. Left/top corners
// compensate via the drag offset so the opposite edge stays roughly
// pinned (roughly: the reflow may still recenter siblings).
const CORNERS = ["nw", "ne", "sw", "se"] as const;
type Corner = (typeof CORNERS)[number];

function onResizeStart(node: TreeNode, corner: Corner, ev: PointerEvent) {
  if (ev.button !== 0) return;
  const left = corner === "nw" || corner === "sw";
  const top = corner === "nw" || corner === "ne";
  const startW = node.width;
  const startH = node.height;
  const startDx = node.dx;
  const startDy = node.dy;
  captureDrag(ev, (mx, my) => {
    node.width = Math.max(MIN_WIDTH, left ? startW - mx : startW + mx);
    node.height = Math.max(MIN_HEIGHT, top ? startH - my : startH + my);
    if (left) node.dx = startDx + (startW - node.width);
    if (top) node.dy = startDy + (startH - node.height);
  });
}

// Drag a node by the free space of its title bar to move it (and its
// subtree — descendants inherit the offset). Interactive chrome
// (source box, links, buttons) keeps its own behavior, and space+drag
// stays a canvas pan even over nodes.
function onChromeDown(node: TreeNode, ev: PointerEvent) {
  if (ev.button !== 0 || spaceHeld.value) return;
  if (ev.target instanceof Element && ev.target.closest("textarea, a, button")) {
    return;
  }
  const startDx = node.dx;
  const startDy = node.dy;
  captureDrag(ev, (mx, my) => {
    node.dx = startDx + mx;
    node.dy = startDy + my;
  });
}
</script>

<template>
  <div
    class="tree-root"
    :class="{ 'tree-root--panning': panning, 'tree-root--pannable': spaceHeld }"
  >
    <div
      ref="viewportEl"
      class="tree-viewport"
      :style="{
        backgroundPosition: `${pan.x}px ${pan.y}px`,
        backgroundSize: `${24 * zoom}px ${24 * zoom}px`,
      }"
      @wheel="onWheel"
      @pointerdown="onPointerDown"
    >
      <div
        class="tree-plane"
        :class="{ 'tree-plane--live': nodeDragLive }"
        :style="{ transform: `translate(${pan.x}px, ${pan.y}px) scale(${zoom})` }"
      >
        <svg class="tree-edges">
          <path v-for="e in edges" :key="e.id" class="tree-edge" :d="e.d" />
        </svg>
        <section
          v-for="node in nodes"
          :key="node.id"
          class="tree-node"
          :style="{
            left: rectOf(node.id).x + 'px',
            top: rectOf(node.id).y + 'px',
            width: node.width + 'px',
            height: node.height + 'px',
          }"
        >
          <div
            class="tree-node-chrome"
            @pointerdown="(e) => onChromeDown(node, e)"
          >
            <textarea
              v-auto-grow
              class="tree-node-source"
              rows="1"
              :value="node.source"
              spellcheck="false"
              placeholder="card source — e.g. documentView(&quot;uuid&quot;), Enter to run"
              @input="growSourceBox($event.target as HTMLTextAreaElement)"
              @keydown.enter.exact.prevent="commitSource(node, $event)"
            />
            <a
              v-if="node.source.trim() !== ''"
              class="tree-node-alone"
              :href="aloneHref(node)"
              target="_blank"
              rel="noopener"
              title="open this card alone"
              >↗</a
            >
            <button
              class="tree-node-agent"
              title="let a coding agent work on this card"
              @click="handOffToAgent(ctxFor(node).host)"
            >
              🤖
            </button>
            <button
              class="tree-node-close"
              title="close card (and its subtree)"
              @click="closeNode(node.id)"
            >
              ✕
            </button>
          </div>
          <ShadowCard
            class="tree-node-card"
            :source="node.source"
            :ctx="ctxFor(node)"
          />
          <div
            v-for="corner in CORNERS"
            :key="corner"
            class="tree-node-resize"
            :class="`tree-node-resize--${corner}`"
            role="separator"
            @pointerdown.stop="(e) => onResizeStart(node, corner, e)"
          />
        </section>
      </div>
      <div class="tree-controls">
        <button title="zoom out" @click="zoomStep(1 / 1.2)">−</button>
        <button
          class="tree-controls-pct"
          title="reset zoom to 100%"
          @click="zoomReset"
        >
          {{ Math.round(zoom * 100) }}%
        </button>
        <button title="zoom in" @click="zoomStep(1.2)">+</button>
        <button title="zoom to fit" @click="zoomToFit">⛶</button>
        <span class="tree-controls-sep" />
        <button title="add a blank card" @click="addRootCard">+ card</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.tree-root {
  display: flex;
  flex-direction: column;
  flex: 1 1 0;
  min-height: 0;
}
.tree-root--pannable .tree-viewport {
  cursor: grab;
}
.tree-root--panning .tree-viewport {
  cursor: grabbing;
  user-select: none;
}
.tree-viewport {
  position: relative;
  flex: 1 1 auto;
  min-height: 0;
  overflow: hidden;
  /* Dot grid that moves with the plane (position/size bound above). */
  background-image: radial-gradient(
    circle,
    color-mix(in srgb, var(--fw-fg) 18%, transparent) 1px,
    transparent 1px
  );
  /* Plain-wheel pan and pinch zoom are handled in JS. */
  touch-action: none;
}
.tree-plane {
  position: absolute;
  top: 0;
  left: 0;
  transform-origin: 0 0;
}
.tree-edges {
  position: absolute;
  top: 0;
  left: 0;
  width: 1px;
  height: 1px;
  overflow: visible;
  pointer-events: none;
}
.tree-edge {
  fill: none;
  stroke: color-mix(in srgb, var(--fw-fg) 35%, transparent);
  stroke-width: 1.5;
  /* Follow the nodes' re-layout animation (no-op where the `d`
     property isn't transitionable, e.g. older Safari). */
  transition: d 180ms ease;
}
.tree-node {
  position: absolute;
  display: flex;
  flex-direction: column;
  box-sizing: border-box;
  background: var(--fw-bg);
  border: 1px solid #888;
  border-radius: 6px;
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.15);
  overflow: hidden;
  /* Animate to freshly laid-out positions so siblings shuffling to
     make room reads as motion, not teleport. */
  transition:
    left 180ms ease,
    top 180ms ease;
}
/* While a node is being dragged or resized, positions must track the
   pointer directly — the animation would trail it (and lag a moved
   subtree behind its parent). */
.tree-plane--live .tree-node,
.tree-plane--live .tree-edge {
  transition: none;
}
.tree-node-chrome {
  flex: 0 0 auto;
  display: flex;
  align-items: flex-start;
  gap: 0.4rem;
  /* Extra top padding: a grab strip for moving the node without
     misclicking into the source textarea. */
  padding: 0.85rem 0.5rem 0.3rem;
  border-bottom: 1px solid #888;
  background: rgba(0, 0, 0, 0.08);
  cursor: grab;
}
.tree-node-chrome:active {
  cursor: grabbing;
}
.tree-node-chrome:focus-within {
  background: rgba(99, 102, 241, 0.18);
}
.tree-node-source {
  flex: 1 1 auto;
  cursor: text;
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
.tree-node-source:focus {
  outline: none;
}
.tree-node-alone,
.tree-node-agent,
.tree-node-close {
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
.tree-node-alone:hover,
.tree-node-agent:hover,
.tree-node-close:hover {
  opacity: 1;
}
.tree-node-card {
  flex: 1 1 auto;
  min-height: 0;
}
.tree-node-resize {
  position: absolute;
  width: 14px;
  height: 14px;
  z-index: 1;
}
.tree-node-resize--nw {
  left: 0;
  top: 0;
  cursor: nwse-resize;
}
.tree-node-resize--ne {
  right: 0;
  top: 0;
  cursor: nesw-resize;
}
.tree-node-resize--sw {
  left: 0;
  bottom: 0;
  cursor: nesw-resize;
}
.tree-node-resize--se {
  right: 0;
  bottom: 0;
  cursor: nwse-resize;
}
.tree-controls {
  position: absolute;
  left: 0.75rem;
  bottom: 0.75rem;
  display: flex;
  align-items: center;
  gap: 0.2rem;
  padding: 0.2rem;
  border: 1px solid var(--fw-border);
  border-radius: 6px;
  background: var(--fw-bg);
  box-shadow: 0 1px 4px rgba(0, 0, 0, 0.15);
}
.tree-controls button {
  border: none;
  background: transparent;
  color: inherit;
  cursor: pointer;
  font-size: 0.85rem;
  line-height: 1.4;
  padding: 0.1rem 0.4rem;
  border-radius: 4px;
}
.tree-controls button:hover {
  background: var(--fw-hover);
}
.tree-controls-pct {
  min-width: 3.2em;
  text-align: center;
  font-variant-numeric: tabular-nums;
}
.tree-controls-sep {
  width: 1px;
  align-self: stretch;
  margin: 0.15rem 0.2rem;
  background: var(--fw-border);
}
</style>
