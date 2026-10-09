// What ContainersView (the host) provides down to the recursive
// ContainerNode, so ContainerNode needs only its node. Its own module to
// keep the host and the recursive component from importing each other.
import type { InjectionKey } from "vue";
import type { CardCtx } from "@/cards/types";
import type { BoxNode, CardNode, TreeNode } from "./containerTree";

// What a container's or a card's panel offers, in sections. An icon is
// a stroked path on a 24px grid (panelIcons.ts, LAYOUT_ICONS).
export type PanelAction = {
  label: string;
  icon: string;
  run: () => void;
  // The choice in effect, in a tiles section (the current layout).
  current?: boolean;
  danger?: boolean;
  // The panel stays open after it, to show the change (a layout, a flag).
  stay?: boolean;
};

export type PanelSection =
  | { kind: "tiles"; title: string; actions: PanelAction[] }
  | { kind: "rows"; title?: string; actions: PanelAction[] }
  | { kind: "toggle"; label: string; hint: string; icon: string; on: boolean; run: () => void };

export type Panel = { title: string; icon: string; sections: PanelSection[] };

export type ContainersApi = {
  ctxFor(card: CardNode): CardCtx;
  titleOf(node: TreeNode): string;
  // Register (or, with null, drop) the element a card's DOM is moved
  // into; the cards live in one pool in the host and are teleported, so
  // rearranging containers moves a card without remounting it.
  setSlot(id: string, el: Element | null): void;
  // Whether a card or container shows its chrome: always in edit mode,
  // and otherwise only outside a solidified subtree.
  chromeShown(id: string): boolean;
  isSolidified(id: string): boolean;
  select(id: string): void;
  close(id: string): void;
  commitSource(card: CardNode, e: Event): void;
  // Open the panel `build` describes at the pointer. It is rebuilt as
  // the tree changes, so an action that keeps it open shows its effect.
  openPanel(ev: MouseEvent, build: () => Panel): void;
  // Whether a tab has been shown, so it stays mounted (hidden) when
  // another is picked; and marking one shown.
  tabShown(id: string): boolean;
  markShown(id: string | null): void;
  // A gallery card at the end of container `boxId`.
  addCard(boxId: string): void;
  // The panel of node `id`, as a builder for openPanel.
  panelFor(id: string): () => Panel;
  // Drag the edge after child `id` to resize it along `axis`.
  startResize(id: string, axis: "x" | "y", ev: PointerEvent): void;
};

export const CONTAINERS_API: InjectionKey<ContainersApi> = Symbol("containersApi");
