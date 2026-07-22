<script setup lang="ts">
// Tiling layout host: a tiling-window-manager surface. Cards live in
// the leaves of a split tree (see tilingTree.ts), which starts as one
// horizontal root container holding a single card. Containers show
// visible borders so the structure reads at a glance.
//
// Ways to grow/reshape the layout:
//   - the "add" button at a container's end appends a blank card;
//   - a card's `ctx.host.openCards(source)` opens the new card as a
//     sibling next to the caller — no prompt; the user reshapes later
//     with the per-container h/v/tab switch or by dragging
//     (`openCards(a, b, …)` opens a run of siblings at once);
//   - each container has a switch to set its arrangement (h/v/tab);
//   - drag a node by its grip strip onto a container's add area (moves
//     it there) or onto a card (replaces the card with a new split,
//     perpendicular to the card's parent, holding the card then the
//     dragged node).
//
// Closing a node removes it; a non-root split left with one child
// collapses and promotes that child. Dividers between siblings drag to
// reweight them.
//
// Persistent cards: the recursive TilingNode tree renders only the
// structure and chrome — where a card goes it leaves an empty slot. The
// cards themselves live in one flat, id-keyed pool here (`tiles`) and
// are <Teleport>ed into their slots. Because the pool never reorders or
// re-parents a card, restructuring the tree (drag, wrap, arrangement
// switch) MOVES a card's DOM to its new slot instead of remounting it —
// preserving its shadow root, scroll, fetches, and (crucially) not
// re-running a grid card's selection restore, which would otherwise
// spawn a duplicate document.
//
// Like the tree layout this is in-memory only — no URL sync — and
// cards are not carried across when toggling layouts (see CardsView).
import { computed, provide, reactive, ref, watch } from "vue";
import ShadowCard from "@/components/ShadowCard.vue";
import { createBus } from "@/cards/bus";
import { displayTitle } from "@/cards/title";
import type { CardCtx, HostCommands } from "@/cards/types";
import {
  addSibling,
  appendChild,
  deleteNode,
  dropOntoLeaf,
  findNode,
  findTile,
  listTiles,
  makeTile,
  makeRoot,
  moveNodeToContainer,
  subtreeContains,
  type Dir,
  type TileLeaf,
  type TileNode,
  type TileSplit,
} from "./tilingTree";
import { TILING_API, type TilingApi } from "./tilingApi";
import TilingNode_ from "./TilingNode.vue";

const bus = createBus();

let nextId = 1;
function freshId(): string {
  return `card-${nextId++}`;
}

// Start as one horizontal root container holding a single card. The
// root is the sole single-child split allowed (see tilingTree.ts) and is
// never draggable or collapsible.
const root = ref<TileNode>(makeRoot(freshId(), makeTile(freshId(), "gridView()")));

// Every live tile, flat. Drives the persistent card pool below.
const tiles = computed(() => listTiles(root.value));

// The DOM slot each leaf's card teleports into, registered by the leaf
// (see TilingNode's `tiling-card` div). Reactive so the teleport
// re-targets when the tree restructures and a leaf's slot element is
// replaced. Null registrations are ignored; stale ids are pruned below.
const slots = reactive(new Map<string, HTMLElement>());
function setSlot(id: string, el: HTMLElement | null) {
  if (el) slots.set(id, el);
}

// One CardCtx per tile id, pruned to the live tiles whenever the tree
// changes structurally (reassigned root). In-place weight/state/source
// mutations keep the same tiles, so they don't churn the cache. The
// slot map is pruned the same way (a closed leaf's TilingNode unmounts
// with a null ref, which we ignore, so its entry lingers until here).
const ctxCache = new Map<string, CardCtx>();
watch(root, (tree) => {
  const live = new Set(listTiles(tree).map((p) => p.id));
  for (const id of [...ctxCache.keys()]) {
    if (!live.has(id)) ctxCache.delete(id);
  }
  for (const id of [...slots.keys()]) {
    if (!live.has(id)) slots.delete(id);
  }
  for (const id of [...titles.keys()]) {
    if (!live.has(id)) titles.delete(id);
  }
});

// Human-readable titles by tile id, set by each card via ctx.setTitle;
// null when the card never set one. Shown via titleFor when dev mode
// is off.
const titles = reactive(new Map<string, string | null>());
function titleFor(leaf: TileLeaf): string {
  return displayTitle(leaf.source, titles.get(leaf.id));
}

// ---- host commands ----

// A card opening another card (e.g. a grid row → its document) places
// the new card as a sibling of the caller. No prompt: every leaf has a
// parent now, and the user can re-arrange afterwards (container switch
// / drag). Returns the new id (callers ignore it).
function openCardFrom(fromId: string, source: string): string {
  const newTile = makeTile(freshId(), source);
  root.value = addSibling(root.value, fromId, newTile);
  return newTile.id;
}

// host.openCards: open a chain of cards, each placed as a sibling of
// the one the previous source produced. Returns the new tile ids in
// chain order.
function openCardsFrom(fromId: string, sources: string[]): string[] {
  let prev = fromId;
  const ids: string[] = [];
  for (const source of sources) {
    prev = openCardFrom(prev, source);
    ids.push(prev);
  }
  return ids;
}

function closeNode(id: string) {
  // A replacement tile keeps the tree non-empty when the last card
  // goes — a gallery card, same as addCard.
  root.value = deleteNode(root.value, id, () =>
    makeTile(freshId(), "galleryView()"),
  );
}

function setTileState(id: string, state: string) {
  const tile = findTile(root.value, id);
  if (tile && tile.state !== state) tile.state = state;
}

// CardCtx memoized per id. Host commands close over the id, and
// `initialState` reads the live tile by id, so the ctx survives the
// tile object being rebuilt by a tree op.
function ctxFor(leaf: TileLeaf): CardCtx {
  let ctx = ctxCache.get(leaf.id);
  if (!ctx) {
    const cardId = leaf.id;
    const host: HostCommands = {
      openCards: (...sources) => openCardsFrom(cardId, sources),
      setSource: (source) => setTileSource(cardId, source),
      close: () => closeNode(cardId),
      setState: (state) => setTileState(cardId, state),
    };
    ctx = {
      cardId,
      get initialState() {
        return findTile(root.value, cardId)?.state ?? "";
      },
      setTitle: (title) => {
        titles.set(cardId, title);
      },
      bus,
      host,
    };
    ctxCache.set(cardId, ctx);
  }
  return ctx;
}

function commitSource(leaf: TileLeaf, e: Event) {
  const next = (e.target as HTMLTextAreaElement).value;
  if (next !== leaf.source) {
    leaf.source = next;
    // New code means the old card's state no longer applies.
    leaf.state = "";
  }
}

// host.setSource: replace this tile's own source (clearing state) —
// drives the agent hand-off (see handoff.ts).
function setTileSource(id: string, source: string) {
  const tile = findTile(root.value, id);
  if (!tile) return;
  tile.source = source;
  tile.state = "";
}

// ---- divider resize ----

const MIN_TILE_PX = 80;

// Drag the divider after children[index]: shift weight between it and
// the next child, keeping their sum fixed so the rest of the split
// holds still. Weights are unitless flex-grow factors, so convert the
// pointer delta to weight via the pair's current pixel size.
function startResize(split: TileSplit, index: number, ev: PointerEvent) {
  if (ev.button !== 0) return;
  ev.preventDefault();
  const divider = ev.currentTarget as HTMLElement;
  const prev = divider.previousElementSibling as HTMLElement | null;
  const nextEl = divider.nextElementSibling as HTMLElement | null;
  if (!prev || !nextEl) return;
  const a = split.children[index];
  const b = split.children[index + 1];
  const horiz = split.dir === "h";
  const sizeA = horiz ? prev.offsetWidth : prev.offsetHeight;
  const sizeB = horiz ? nextEl.offsetWidth : nextEl.offsetHeight;
  const pairPx = sizeA + sizeB;
  const pairWeight = a.weight + b.weight;
  if (pairPx <= 0 || pairWeight <= 0) return;
  const start = horiz ? ev.clientX : ev.clientY;
  const startWeightA = a.weight;
  // Keep both tiles at least MIN_TILE_PX wide, expressed in weight.
  const minWeight = (MIN_TILE_PX / pairPx) * pairWeight;

  const onMove = (e: PointerEvent) => {
    const delta = (horiz ? e.clientX : e.clientY) - start;
    let wa = startWeightA + (delta / pairPx) * pairWeight;
    wa = Math.max(minWeight, Math.min(pairWeight - minWeight, wa));
    a.weight = wa;
    b.weight = pairWeight - wa;
  };
  const onUp = (e: PointerEvent) => {
    divider.releasePointerCapture(e.pointerId);
    divider.removeEventListener("pointermove", onMove);
    divider.removeEventListener("pointerup", onUp);
    divider.removeEventListener("pointercancel", onUp);
  };
  divider.setPointerCapture(ev.pointerId);
  divider.addEventListener("pointermove", onMove);
  divider.addEventListener("pointerup", onUp);
  divider.addEventListener("pointercancel", onUp);
}

// ---- tabs / container arrangement ----

function setActive(split: TileSplit, index: number) {
  // `split` is the live reactive node; mutating in place re-renders.
  split.active = index;
}

function setDir(split: TileSplit, dir: Dir) {
  split.dir = dir;
  // A tab container needs a shown child; default to the first.
  if (dir === "tab" && split.active == null) split.active = 0;
}

// ---- add / drag-and-drop ----

function isRoot(id: string): boolean {
  return root.value.id === id;
}

// ＋ add area: a gallery card (both modes), which the user resolves by
// picking a component (it replaces itself via host.setSource).
function addCard(containerId: string) {
  root.value = appendChild(root.value, containerId, makeTile(freshId(), "galleryView()"));
}

// What's being dragged, and where the pointer currently hovers — a
// container's add area or a card to split onto. Both drive highlights.
const draggingId = ref<string | null>(null);
const dropTarget = ref<{ kind: "add" | "leaf"; id: string } | null>(null);

function isDragging(id: string): boolean {
  return draggingId.value === id;
}
function isLeafDrop(id: string): boolean {
  return dropTarget.value?.kind === "leaf" && dropTarget.value.id === id;
}
function isAddDrop(id: string): boolean {
  return dropTarget.value?.kind === "add" && dropTarget.value.id === id;
}

// Resolve the element under the pointer to a valid drop target. A drop
// is invalid onto the dragged node itself or into its own subtree.
function resolveDrop(
  el: Element | null,
  draggedId: string,
): { kind: "add" | "leaf"; id: string } | null {
  const dragged = findNode(root.value, draggedId);
  if (!el || !dragged) return null;
  const addEl = el.closest("[data-tiling-add]");
  if (addEl) {
    const id = addEl.getAttribute("data-tiling-add")!;
    return id !== draggedId && !subtreeContains(dragged, id)
      ? { kind: "add", id }
      : null;
  }
  const leafEl = el.closest("[data-tiling-leaf]");
  if (leafEl) {
    const id = leafEl.getAttribute("data-tiling-leaf")!;
    return id !== draggedId && !subtreeContains(dragged, id)
      ? { kind: "leaf", id }
      : null;
  }
  return null;
}

// Pointer-based drag: window-level (capture) listeners so the drag
// tracks across cards and shadow roots, and `elementFromPoint` finds
// the drop target beneath the cursor without pointer capture.
function startDrag(id: string, ev: PointerEvent) {
  if (ev.button !== 0) return;
  ev.preventDefault();
  draggingId.value = id;
  dropTarget.value = null;
  const onMove = (e: PointerEvent) => {
    dropTarget.value = resolveDrop(
      document.elementFromPoint(e.clientX, e.clientY),
      id,
    );
  };
  const onUp = () => {
    window.removeEventListener("pointermove", onMove, true);
    window.removeEventListener("pointerup", onUp, true);
    window.removeEventListener("pointercancel", onUp, true);
    const target = dropTarget.value;
    draggingId.value = null;
    dropTarget.value = null;
    if (!target) return;
    root.value =
      target.kind === "add"
        ? moveNodeToContainer(root.value, id, target.id)
        : dropOntoLeaf(root.value, id, target.id, freshId());
  };
  window.addEventListener("pointermove", onMove, true);
  window.addEventListener("pointerup", onUp, true);
  window.addEventListener("pointercancel", onUp, true);
}

const api: TilingApi = {
  ctxFor,
  commitSource,
  titleFor,
  closeNode,
  startResize,
  setActive,
  setDir,
  setSlot,
  addCard,
  startDrag,
  isRoot,
  isDragging,
  isLeafDrop,
  isAddDrop,
};
provide(TILING_API, api);
</script>

<template>
  <div class="tiling-root" :class="{ 'tiling-root--dragging': draggingId !== null }">
    <TilingNode_ :node="root" :style="{ flex: '1 1 0' }" />
    <!-- Persistent card pool: one ShadowCard per leaf, kept in this
         flat keyed list (never remounted by tree restructuring) and
         teleported into the leaf's slot in the tree above. While a
         leaf's slot isn't registered yet the teleport is disabled and
         the card renders here, hidden, then moves once the slot
         appears — so the card mounts at most once. -->
    <div class="tiling-card-pool" aria-hidden="true">
      <Teleport
        v-for="leaf in tiles"
        :key="leaf.id"
        :to="slots.get(leaf.id)"
        :disabled="!slots.get(leaf.id)"
      >
        <ShadowCard
          class="tiling-mounted-card"
          :source="leaf.source"
          :ctx="ctxFor(leaf)"
        />
      </Teleport>
    </div>
  </div>
</template>

<style scoped>
.tiling-root {
  display: flex;
  flex: 1 1 0;
  min-height: 0;
  /* Breathing room so the outermost container's border isn't flush
     with the viewport edges. */
  padding: 6px;
  box-sizing: border-box;
  gap: 0;
}
/* While dragging a node, suppress text selection and show a move
   cursor everywhere so the gesture reads as a drag. */
.tiling-root--dragging {
  cursor: grabbing;
  user-select: none;
}
/* The pool only ever holds not-yet-teleported (transient) cards;
   teleported ones live in their slots. Hidden so the transient state
   never flashes. */
.tiling-card-pool {
  display: none;
}
/* A teleported card fills its slot (a flex container in TilingNode). */
.tiling-mounted-card {
  flex: 1 1 auto;
  min-width: 0;
  min-height: 0;
}
</style>
