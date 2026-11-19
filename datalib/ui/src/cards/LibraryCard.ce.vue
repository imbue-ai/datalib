<script setup lang="ts">
// `libraryTreeView()`: the library as VS Code's tree (`<vs-tree>`,
// third-party/vscode-tree) — its sources, then their places, a level at a
// time — and, beside it, the node opened there as a node card
// (`nodeView`, cards/NodeCard.ce.vue), drawn in a shadow root of its own
// the way the Search card draws its preview. A node the node card opens
// in turn replaces it here; a document opens as a card. What is open in
// the tree, and the node shown, are the card's state. The rules are
// cards/library.ts.
import { onBeforeUnmount, onMounted, ref, useTemplateRef } from "vue";
import type { VsTree, VsTreeNode, VsTreeViewState } from "vs-tree";
import { useApi } from "@/cards/cardApi";
import type { CardCtx, Teardown } from "./types";
import { parseCardSource } from "./cardCall";
import { nodeSource, type NodeViewOpts } from "./cardSources";
import { nodeView } from "./libs/nodeView";
import { iconUrl } from "@/config/icons";
import { placeNodes, sourceNodes, type TreeNode } from "./library";

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
    const rows = (await api.fetchManageRows()).rows;
    const held = await sourcesWithPlaces(sourceNodes(rows).map((n) => n.id));
    return sourceNodes(rows, (id) => held.has(id), iconUrl);
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

function show(opts: NodeViewOpts | null) {
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

onMounted(async () => {
  props.ctx.setTitle("Library");
  props.ctx.setHelp(
    "<p>Your library as a tree: each source, then the places in it — accounts, " +
      "labels, workspaces, channels, folders, chats — a level at a time.</p>" +
      "<p>Double-click a place, or press Enter on it, to show what it holds " +
      "beside the tree. There, a click reads a message beside the list and a " +
      "double-click opens it. ↗ opens what is shown as a card of its own.</p>" +
      "<p>The tree keeps what you opened, and what you showed, when you come back.</p>",
  );
  try {
    await import("vs-tree");
  } catch (e) {
    failed.value = `The tree could not load: ${(e as Error).message}`;
    return;
  }
  const el = document.createElement("vs-tree") as VsTree;
  el.setAttribute("aria-label", "Library");
  if (state.tree) el.viewState = state.tree;
  el.addEventListener("viewstatechange", save);
  el.addEventListener("open", (e) => {
    const node = (e as CustomEvent<{ node: VsTreeNode }>).detail.node;
    show({ node: node.id, name: node.label });
  });
  el.dataSource = dataSource;
  treeHost.value?.appendChild(el);
  tree = el;
  if (state.node) show(state.node);
});

onBeforeUnmount(() => teardown?.());
</script>

<template>
  <div class="lib">
    <div class="lib-tree">
      <p v-if="failed" class="lib-note lib-failed">{{ failed }}</p>
      <div ref="treeHost" class="lib-tree-host" data-testid="library-tree" />
    </div>
    <section class="lib-pane" :class="{ 'is-empty': !shown }">
      <header v-if="shown" class="lib-pane-bar">
        <strong class="lib-pane-title">{{ paneTitle }}</strong>
        <button class="lib-pane-open" title="Open as a card of its own" @click="openShownAsCard">
          ↗
        </button>
      </header>
      <p v-else class="lib-note">Double-click a place in the tree to see what it holds.</p>
      <div ref="paneEl" class="lib-pane-body" data-testid="library-node" />
    </section>
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
