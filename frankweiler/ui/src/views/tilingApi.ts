// The contract TilingView (the host) provides down to the recursive
// TileNode renderer via provide/inject, so TileNode only needs a
// `node` prop. Kept in its own module to avoid a circular import
// between the host and the recursive component.
import type { InjectionKey } from "vue";
import type { CardCtx } from "@/cards/types";
import type { Dir, TileLeaf, TileSplit } from "./tilingTree";

export type TilingApi = {
  ctxFor(leaf: TileLeaf): CardCtx;
  commitSource(leaf: TileLeaf, e: Event): void;
  // The card's human-readable title (declared via cards/title.ts, with
  // a source-derived fallback) — what the chrome and tab bar show when
  // dev mode is off. Lives on the host because the ShadowCards (which
  // report the declared titles) live in the host's persistent pool.
  titleFor(leaf: TileLeaf): string;
  // Remove a node by id — a tile (a card's close, via ctx.host.close) or
  // a whole grouped child (a tab's ✕).
  closeNode(id: string): void;
  // Begin dragging the divider after children[index] of `split`.
  startResize(split: TileSplit, index: number, ev: PointerEvent): void;
  // Show children[index] of a tab split.
  setActive(split: TileSplit, index: number): void;
  // Switch a container's arrangement (the per-container h/v/tab control).
  setDir(split: TileSplit, dir: Dir): void;
  // Register (or, with null, drop) the DOM slot a leaf's card teleports
  // into. The host keeps the cards in a flat pool and teleports each
  // into its slot, so restructuring the tree moves a card's DOM without
  // remounting it (see TilingView).
  setSlot(id: string, el: HTMLElement | null): void;
  // Append a fresh blank card at the end of a container (the "add"
  // button / drop area).
  addCard(containerId: string): void;
  // Begin dragging a node (a card or a container) by its grip strip.
  startDrag(id: string, ev: PointerEvent): void;
  // Whether `id` is the (never-collapsing, undraggable) root container.
  isRoot(id: string): boolean;
  // Whether `id` is the node currently being dragged (render dimmed).
  isDragging(id: string): boolean;
  // Whether `id` is the leaf currently under the drag (drop = split it).
  isLeafDrop(id: string): boolean;
  // Whether `id` is the container whose add area is under the drag.
  isAddDrop(id: string): boolean;
};

export const TILING_API: InjectionKey<TilingApi> = Symbol("tilingApi");
