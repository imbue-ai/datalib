<script setup lang="ts">
// `libraryTreeView()`: the library as VS Code's tree (`<vs-tree>`,
// third-party/vscode-tree) — its sources, then their places, a level at a
// time, each drawn as its chip — and, beside it, the node picked there as
// a node card (`nodeView`, cards/NodeCard.ce.vue), drawn in a shadow root
// of its own the way the Search card draws its preview. A node the node
// card opens in turn replaces it here; a document opens as a card. A
// chip's double-click and right-click are a chip's everywhere
// (docs/dev/chips.md § Clicks). What is open in the tree, and the node
// shown, are the card's state. The rules are cards/library.ts.
import { onBeforeUnmount, onMounted, ref, useTemplateRef, watchEffect } from "vue";
import type { VsTree, VsTreeNode, VsTreeViewState } from "vs-tree";
import { useApi } from "@/cards/cardApi";
import type { CardCtx, Teardown } from "./types";
import { parseCardSource } from "./cardCall";
import { nodeSource, type NodeViewOpts } from "./cardSources";
import { nodeView } from "./libs/nodeView";
import ChipMenu from "./ChipMenu.ce.vue";
import chipCss from "./chip.css?inline";
import {
  CHIP_SELECTOR,
  chipElAt,
  chipMenuOf,
  chipOf,
  runChipAction,
  type Chip,
  type ChipEntry,
  type ChipEntryId,
} from "./chipActions";
import { entities, entityCell } from "./entities";
import { nodeCell } from "./nodeChip";
import {
  TREE_PICKS,
  nodeOpts,
  placeNodes,
  sourceNodes,
  treeChip,
  type TreeChip,
  type TreeNode,
} from "./library";

const props = defineProps<{ ctx: CardCtx }>();
const api = useApi();

type State = { tree?: VsTreeViewState; node?: NodeViewOpts | null };

function readState(): State {
  try {
    const s = JSON.parse(props.ctx.initialState || "{}");
    return typeof s === "object" && s !== null ? s : {};
  } catch {
    return {};
  }
}
const state: State = readState();

const treeHost = useTemplateRef<HTMLDivElement>("treeHost");
const paneEl = useTemplateRef<HTMLDivElement>("paneEl");
const shown = ref<NodeViewOpts | null>(null);
const paneTitle = ref("");
const failed = ref<string | null>(null);
// Until the tree has its first level: `<vs-tree>` draws nothing meanwhile.
const loading = ref(true);
let tree: VsTree | null = null;

/// A level of places, every page of it: a level is a few dozen places, a
/// few thousand at the most (a drive's folders).
async function placesUnder(parent: string): Promise<TreeNode[]> {
  const children = [];
  let after: string | undefined;
  do {
    const page = await api.fetchChildren(parent, { places: true, limit: 1000, after });
    children.push(...page.children);
    after = page.after ?? undefined;
  } while (after);
  return placeNodes(children);
}

/// Which sources hold places at their top: one place each tells, so a
/// source without any draws no arrow to open onto nothing, and is shown
/// beside the tree instead.
async function sourcesWithPlaces(ids: string[]): Promise<Set<string>> {
  const held = await Promise.all(
    ids.map(async (id) => {
      const page = await api.fetchChildren(id, { places: true, limit: 1 });
      return page.children.length > 0 ? id : null;
    }),
  );
  return new Set(held.filter((id): id is string => id !== null));
}

const dataSource = {
  async getChildren(node: VsTreeNode | null): Promise<VsTreeNode[]> {
    if (node !== null) return placesUnder(node.id);
    try {
      const rows = (await api.fetchManageRows()).rows;
      const held = await sourcesWithPlaces(sourceNodes(rows).map((n) => n.id));
      return sourceNodes(rows, (id) => held.has(id));
    } catch (e) {
      failed.value = `The library could not be read: ${(e as Error).message}`;
      return [];
    } finally {
      loading.value = false;
    }
  },
};

let saving = false;
function save() {
  if (saving) return;
  saving = true;
  requestAnimationFrame(() => {
    saving = false;
    props.ctx.host.setState(JSON.stringify({ tree: tree?.viewState, node: shown.value }));
  });
}

// ---- the node beside the tree ----
let paneRoot: ShadowRoot | null = null;
let teardown: Teardown | null = null;

/// A node card's opens: another node shows here, anything else opens as
/// cards do.
function openFromPane(...sources: string[]): string[] {
  if (sources.length === 1) {
    const call = parseCardSource(sources[0]);
    const opts = call.args[0] as NodeViewOpts | undefined;
    if (call.name === "nodeView" && typeof opts?.node === "string") {
      show(opts);
      return [];
    }
  }
  return props.ctx.host.openCards(...sources);
}

// A pick in the tree waiting for the selection to settle. Whatever the pane
// shows meanwhile, from the pane itself, cancels it.
let selecting: ReturnType<typeof setTimeout> | undefined;

function show(opts: NodeViewOpts | null) {
  clearTimeout(selecting);
  teardown?.();
  teardown = null;
  shown.value = opts;
  paneTitle.value = opts?.name ?? "";
  save();
  if (!paneEl.value) return;
  paneRoot ??= paneEl.value.attachShadow({ mode: "open" });
  paneRoot.replaceChildren();
  if (!opts) return;
  teardown = nodeView(opts)(paneRoot, {
    cardId: `${props.ctx.cardId}:node`,
    cardType: "nodeView",
    initialState: "",
    setTitle: (t) => {
      if (t) paneTitle.value = t;
    },
    setHelp: () => {},
    bus: props.ctx.bus,
    host: {
      ...props.ctx.host,
      openCards: openFromPane,
      setState: () => {},
      setSource: () => {},
      close: () => show(null),
    },
  });
}

function openShownAsCard() {
  if (shown.value) props.ctx.host.openCards(nodeSource(shown.value));
}

// ---- chips ----

/// A node's chip, drawn from what is known now: a source's from what
/// `entities` says of its group, a place's from its own row.
function chipEl(c: TreeChip | null): HTMLElement | null {
  if (!c) return null;
  if (c.kind === "group") return entityCell(c.uri, c.shown, entities.lookup(c.uri), c.mark);
  return nodeCell(c.id, c.name, c.nodeKind, c.doc);
}

const chipHost = { openCard: (source: string) => void props.ctx.host.openCards(source) };

type MenuAt = { chip: Chip; entries: ChipEntry[]; x: number; y: number };
const menuAt = ref<MenuAt | null>(null);

function openMenu(el: HTMLElement | null, x: number, y: number) {
  const chip = el ? chipOf(el) : null;
  if (chip) menuAt.value = { chip, entries: chipMenuOf(chip, false), x, y };
}

async function onMenuPick(id: string) {
  const menu = menuAt.value;
  menuAt.value = null;
  if (menu) await runChipAction(id as ChipEntryId, menu.chip, chipHost);
}

/// The chip a press landed on, else the chip of the row it landed in.
function chipAt(ev: Event | undefined): HTMLElement | null {
  const path = ev?.composedPath() ?? [];
  const on = chipElAt(path[0]);
  if (on) return on;
  const row = path.find((t) => t instanceof HTMLElement && t.classList.contains("monaco-list-row"));
  return (row as HTMLElement | undefined)?.querySelector<HTMLElement>(CHIP_SELECTOR) ?? null;
}

/// What picking a node does, as `TREE_PICKS` says.
function pick(how: keyof typeof TREE_PICKS, node: TreeNode, chip: HTMLElement | null) {
  const c = chip ? chipOf(chip) : null;
  if (TREE_PICKS[how] === "card" && c) void runChipAction("open", c, chipHost);
  else show(nodeOpts(node));
}

// The shown node's chip, in the pane's bar.
const titleEl = useTemplateRef<HTMLElement>("titleEl");
const entitiesMoved = ref(0);
const stopEntities = entities.subscribe(() => {
  entitiesMoved.value++;
  tree?.redraw();
});
watchEffect(() => {
  void entitiesMoved.value;
  const el = titleEl.value;
  const opts = shown.value;
  if (!el || !opts) return;
  const node: TreeNode = {
    id: opts.node,
    label: paneTitle.value || opts.name || "",
    hasChildren: false,
    ...(opts.doc ? { doc: opts.doc } : {}),
  };
  const c = chipEl(treeChip(node));
  el.replaceChildren(c ?? node.label);
});

function onTitleDblClick(ev: MouseEvent) {
  const el = chipElAt(ev.target);
  const c = el ? chipOf(el) : null;
  if (c) void runChipAction("open", c, chipHost);
}

function onTitleMenu(ev: MouseEvent) {
  const el = chipElAt(ev.target);
  if (!el) return;
  ev.preventDefault();
  openMenu(el, ev.clientX, ev.clientY);
}

/// A chip is a link only to be copied: a click on one selects its row.
function holdLink(ev: Event) {
  if (chipElAt(ev.composedPath()[0])) ev.preventDefault();
}

onMounted(async () => {
  props.ctx.setTitle("Library");
  props.ctx.setHelp(
    "<p>Your library as a tree: each source, then the places in it — accounts, " +
      "labels, workspaces, channels, folders, chats — a level at a time.</p>" +
      "<p>Pick a place to show what it holds beside the tree. There, a click " +
      "reads a message beside the list. A double-click on a name opens its " +
      "card, and a right-click shows what else you can do with it. ↗ opens " +
      "what is shown as a card of its own.</p>" +
      "<p>The tree keeps what you opened, and what you showed, when you come back.</p>",
  );
  try {
    await import("vs-tree");
  } catch (e) {
    failed.value = `The tree could not load: ${(e as Error).message}`;
    loading.value = false;
    return;
  }
  const el = document.createElement("vs-tree") as VsTree;
  el.setAttribute("aria-label", "Library");
  if (state.tree) el.viewState = state.tree;
  el.addEventListener("viewstatechange", save);
  el.extraCss = chipCss + TREE_CHIP_CSS;
  el.renderLabel = (node, label) => {
    const c = chipEl(treeChip(node as TreeNode));
    label.append(c ?? node.label);
  };
  // Arrowing through the tree shows each place it stops on, once it stops.
  el.addEventListener("select", (e) => {
    const [node] = (e as CustomEvent<{ nodes: TreeNode[] }>).detail.nodes;
    clearTimeout(selecting);
    if (!node || node.id === shown.value?.node) return;
    selecting = setTimeout(() => pick("select", node, null), 120);
  });
  el.addEventListener("open", (e) => {
    const { node, event } = (e as CustomEvent<{ node: TreeNode; event?: MouseEvent }>).detail;
    if (!event) return pick("enter", node, null);
    const onChip = chipElAt(event.composedPath()[0]);
    pick(onChip ? "doubleClickOnChip" : "doubleClickElsewhere", node, onChip ?? chipAt(event));
  });
  el.addEventListener("menu", (e) => {
    const { x, y, event } = (e as CustomEvent<{ x: number; y: number; event?: Event }>).detail;
    openMenu(chipAt(event), x, y);
  });
  el.addEventListener("click", holdLink);
  el.dataSource = dataSource;
  treeHost.value?.appendChild(el);
  tree = el;
  if (state.node) show(state.node);
});

onBeforeUnmount(() => {
  teardown?.();
  stopEntities();
});

// A chip in a row is never wider than the row's label leaves it.
const TREE_CHIP_CSS = `.label { display: flex; min-width: 0; } .label > .chip { max-width: 100%; }`;
</script>

<template>
  <div class="lib">
    <div class="lib-tree">
      <p v-if="failed" class="lib-note lib-failed">{{ failed }}</p>
      <p v-else-if="loading" class="lib-note" data-testid="library-loading">Loading…</p>
      <div ref="treeHost" class="lib-tree-host" data-testid="library-tree" />
    </div>
    <section class="lib-pane" :class="{ 'is-empty': !shown }">
      <header v-if="shown" class="lib-pane-bar">
        <strong
          ref="titleEl"
          class="lib-pane-title"
          @click="holdLink"
          @dblclick="onTitleDblClick"
          @contextmenu="onTitleMenu"
        />
        <button class="lib-pane-open" title="Open as a card of its own" @click="openShownAsCard">
          ↗
        </button>
      </header>
      <p v-else class="lib-note">Pick a place in the tree to see what it holds.</p>
      <div ref="paneEl" class="lib-pane-body" data-testid="library-node" />
    </section>
    <ChipMenu
      v-if="menuAt"
      :entries="menuAt.entries"
      :x="menuAt.x"
      :y="menuAt.y"
      @pick="onMenuPick"
      @close="menuAt = null"
    />
  </div>
</template>

<style scoped>
.lib {
  height: 100%;
  display: flex;
  font-size: var(--datalib-font-size);
  color: var(--datalib-fg);
  background: var(--datalib-bg);
}
.lib-tree {
  flex: 0 0 auto;
  width: 280px;
  min-width: 160px;
  max-width: 60%;
  position: relative;
  resize: horizontal;
  overflow: hidden;
  border-right: 1px solid var(--datalib-border-soft);
}
.lib-tree-host {
  position: absolute;
  inset: 0;
}
.lib-tree-host :deep(vs-tree) {
  position: absolute;
  inset: 0;
  --vs-tree-fg: var(--datalib-fg);
  --vs-tree-bg: var(--datalib-bg);
  --vs-tree-hover: var(--datalib-hover);
  --vs-tree-focus: var(--datalib-accent);
  --vs-tree-focus-bg: color-mix(in srgb, var(--datalib-accent) 10%, var(--datalib-bg));
  --vs-tree-selected-bg: color-mix(in srgb, var(--datalib-accent) 22%, var(--datalib-bg));
  --vs-tree-selected-fg: var(--datalib-fg);
  --vs-tree-inactive-selected-bg: var(--datalib-selected);
  --vs-tree-guide: var(--datalib-border);
  --vs-tree-guide-faint: var(--datalib-border-soft);
  --vs-tree-font: var(--datalib-font-size) / 1.4 var(--datalib-font);
}
.lib-pane {
  flex: 1 1 auto;
  min-width: 0;
  display: flex;
  flex-direction: column;
}
.lib-pane-bar {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 4px var(--datalib-pad);
  border-bottom: 1px solid var(--datalib-border-soft);
}
.lib-pane-title {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.lib-pane-open {
  flex: 0 0 auto;
  font: inherit;
  color: var(--datalib-muted);
  background: none;
  border: 1px solid var(--datalib-border-soft);
  border-radius: var(--datalib-radius);
  cursor: pointer;
}
.lib-pane-open:hover {
  color: var(--datalib-fg);
  background: var(--datalib-hover);
}
.lib-pane-body {
  position: relative;
  flex: 1 1 auto;
  min-height: 0;
}
.lib-pane.is-empty .lib-pane-body {
  display: none;
}
.lib-note {
  margin: 0;
  padding: 10px var(--datalib-pad);
  color: var(--datalib-muted);
}
.lib-failed {
  color: var(--datalib-error-fg);
  background: var(--datalib-error-bg);
}
</style>
