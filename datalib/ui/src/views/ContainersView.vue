<script setup lang="ts">
// Containers layout host: a tree of containers, each laying out its own
// children (containerTree.ts holds the rules). The outermost container
// is always tabs, listed down the sidebar: the pinned ones, then the
// rest as a tree; what is inside each tab is drawn by the recursive
// ContainerNode. The tree is kept in the
// library (`/api/ui/state/layout`), so it survives a restart.
//
// The cards themselves live in one flat pool here and are teleported
// into the slot their node draws: rearranging the
// containers moves a card's DOM without remounting it.
import { computed, onBeforeUnmount, provide, reactive, ref, watch, watchEffect } from "vue";
import { useRoute, useRouter } from "vue-router";
import ShadowCard from "@/components/ShadowCard.vue";
import CardIcon from "@/components/CardIcon.vue";
import ContainerNode from "@/views/ContainerNode.vue";
import ContainerMenu from "@/views/ContainerMenu.vue";
import NameDialog from "@/views/NameDialog.vue";
import { fetchUiState, putUiState } from "@/api";
import { createBus } from "@/cards/bus";
import { chainHref } from "@/cards/chainHref";
import { setCardHelp } from "@/cards/help";
import { cardType, newCardId } from "@/cards/cardId";
import { displayTitle } from "@/cards/title";
import { editMode } from "@/editMode";
import { decodeColumns } from "@/router/columns";
import { pushToast } from "@/toasts";
import { isMainWindow } from "@/views/mainWindow";
import {
  BUILTIN_COMPOSITES,
  composite,
  isBuiltinComposite,
  loadComposites,
  saveComposite,
} from "@/views/composites";
import {
  DEFAULT_COLUMN,
  DIRECTIONS,
  DIRECTION_ICONS,
  DIRECTION_LABELS,
  LAYOUTS,
  LAYOUT_ICONS,
  LAYOUT_LABELS,
  addChild,
  cards,
  find,
  instantiate,
  isSolidified,
  makeBox,
  makeCard,
  move,
  openFrom,
  parentOf,
  parseTree,
  pinnedTabs,
  predatesPins,
  remove,
  rename,
  resetTo,
  reveal,
  setBasis,
  setCard,
  setDirection,
  setLayout as setBoxLayout,
  setPinned,
  setSolidified,
  setTemplate,
  tabRows,
  tabShowing,
  unwrap,
  withPins,
  wrap,
  type BoxNode,
  type CardNode,
  type Layout,
  type TreeNode,
} from "@/views/containerTree";
import {
  CONTAINERS_API,
  type ContainersApi,
  type Panel,
  type PanelAction,
  type PanelSection,
} from "@/views/containersApi";
import { PANEL_ICONS } from "@/views/panelIcons";
import type { CardCtx, HostCommands } from "@/cards/types";

const route = useRoute();
const router = useRouter();
const bus = createBus();

const STATE_NAME = "layout";
// A browser with this set neither reads nor writes the library's layout.
// The e2e suite sets it, so specs running side by side against one
// library do not trade tabs.
const UNSAVED_KEY = "datalib-layout-unsaved";
// Where a window that does not keep the library's layout keeps its own,
// so a reload finds its tabs: this window's session storage.
const SESSION_KEY = "datalib-layout";
// How long the tree sits unchanged before it is written: a resize drag
// or a burst of card state is one write, not dozens.
const SAVE_DELAY_MS = 400;

function dashboard(): TreeNode {
  return instantiate(BUILTIN_COMPOSITES.Dashboard, newCardId);
}

// The tabs a main window has pinned until the person says otherwise.
function defaultPins(): TreeNode[] {
  return [
    dashboard(),
    { ...makeCard(newCardId(), "searchView()"), name: "Search" },
    { ...makeCard(newCardId(), "sourcesView()"), name: "Sources" },
  ].map((tab) => ({ ...tab, pinned: true }));
}

// Closing the last tab brings the Dashboard back: the outermost
// container is never empty.
function withATab(box: BoxNode): BoxNode {
  if (box.children.length > 0) return box;
  const fresh = dashboard();
  return { ...box, children: [fresh], selected: fresh.id };
}

// Empty until start() has read the kept tree, so nothing is mounted
// only to be thrown away.
const root = ref<BoxNode>(makeBox(newCardId(), "tabs", []));
const ready = ref(false);
// Whether this window reads and writes the library's layout; the
// others keep theirs in their session storage.
let keeps = false;

function update(next: TreeNode) {
  if (next.kind === "box") root.value = withATab(next);
}

// ---- keeping the tree ----

let saveTimer: ReturnType<typeof setTimeout> | null = null;
let saveFailed = false;

async function save(keepalive = false) {
  saveTimer = null;
  if (!ready.value) return;
  if (!keeps) {
    try {
      sessionStorage.setItem(SESSION_KEY, JSON.stringify(root.value));
    } catch {
      // Blocked storage: the tabs last as long as the page.
    }
    return;
  }
  try {
    await putUiState(STATE_NAME, root.value, { keepalive });
    saveFailed = false;
  } catch (e) {
    console.warn("could not keep the layout", e);
    // Once per run of failures, not once per change.
    if (!saveFailed) pushToast("Could not save the layout to the library.");
    saveFailed = true;
  }
}
watch(root, () => {
  if (!ready.value) return;
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = setTimeout(() => void save(), SAVE_DELAY_MS);
});
function flush() {
  if (!saveTimer) return;
  clearTimeout(saveTimer);
  void save(true);
}
window.addEventListener("pagehide", flush);
onBeforeUnmount(() => {
  window.removeEventListener("pagehide", flush);
  flush();
});

// The cards a URL names (a link, a popped-out card), in a Columns
// container of their own, so what they open lands beside them as it
// did when the URL was the whole layout. The tree, not the URL, is
// what this layout keeps, so once opened the address goes back to "/".
function routeNode(): TreeNode | null {
  const specs = decodeColumns(route.path);
  if (specs.length === 0) return null;
  const nodes = specs.map((s) => ({
    ...makeCard(newCardId(), s.code, s.state),
    basis: s.size != null ? s.size * DEFAULT_COLUMN : null,
  }));
  return makeBox(newCardId(), "columns", nodes);
}

function openRoute() {
  const node = routeNode();
  if (!node) return;
  update(addChild(root.value, root.value.id, node));
  void router.replace("/");
}
watch(
  () => route.path,
  () => {
    if (ready.value) openRoute();
  },
);

function storedItem(storage: () => Storage, key: string): string | null {
  try {
    return storage().getItem(key);
  } catch {
    return null;
  }
}

// The tree as it was stored, unparsed.
async function readKept(): Promise<unknown> {
  if (!keeps) {
    const text = storedItem(() => sessionStorage, SESSION_KEY);
    try {
      return text === null ? null : (JSON.parse(text) as unknown);
    } catch {
      return null;
    }
  }
  try {
    return await fetchUiState(STATE_NAME);
  } catch (e) {
    console.warn("could not read the kept layout", e);
    pushToast("Could not read the saved layout from the library; starting afresh.");
    return null;
  }
}

async function start() {
  const mainWindow = await isMainWindow();
  keeps = mainWindow && storedItem(() => localStorage, UNSAVED_KEY) !== "1";
  void loadComposites();
  const stored = await readKept();
  const kept = parseTree(stored);
  // A second window starts with nothing but the cards its URL names.
  const fromUrl = routeNode();
  let tree = kept ?? makeBox(newCardId(), "tabs", mainWindow ? defaultPins() : []);
  if (kept && mainWindow && predatesPins(stored)) tree = withPins(kept, defaultPins());
  if (fromUrl) tree = addChild(tree, tree.id, fromUrl) as BoxNode;
  // The outermost container is tabs and never solidified, whatever was stored.
  root.value = withATab({ ...tree, layout: "tabs", solidified: false });
  ready.value = true;
  if (fromUrl) void router.replace("/");
}
void start();

// ---- the cards ----

const allCards = computed(() => cards(root.value));
// A card mounts the first time its slot is drawn, and stays mounted
// while it is in the tree, so a tab switched away from keeps its state.
const mounted = reactive(new Set<string>());
const slots = reactive(new Map<string, Element>());
const pool = computed(() => allCards.value.filter((c) => mounted.has(c.id)));
// The cards a Page container holds, which are as tall as their content.
const naturalCards = computed(() => {
  const out = new Set<string>();
  const walk = (n: TreeNode) => {
    if (n.kind === "card") return;
    for (const c of n.children) {
      if (n.layout === "page" && c.kind === "card") out.add(c.id);
      walk(c);
    }
  };
  walk(root.value);
  return out;
});
watch(allCards, (list) => {
  const live = new Set(list.map((c) => c.id));
  for (const id of [...mounted]) if (!live.has(id)) mounted.delete(id);
  for (const id of [...ctxCache.keys()]) if (!live.has(id)) ctxCache.delete(id);
});

function setSlot(id: string, el: Element | null) {
  if (el) {
    slots.set(id, el);
    mounted.add(id);
  } else {
    slots.delete(id);
  }
}

function cardById(id: string): CardNode | undefined {
  const n = find(root.value, id);
  return n?.kind === "card" ? n : undefined;
}

const ctxCache = new Map<string, CardCtx>();
function ctxFor(card: CardNode): CardCtx {
  let ctx = ctxCache.get(card.id);
  if (!ctx) {
    const cardId = card.id;
    const host: HostCommands = {
      openCards: (...sources) => {
        const shown = sources.length === 1 ? tabShowing(root.value, cardId, sources[0]) : null;
        if (shown !== null) {
          select(shown);
          return [shown];
        }
        const nodes = sources.map((s) => makeCard(newCardId(), s));
        update(openFrom(root.value, cardId, nodes));
        return nodes.map((n) => n.id);
      },
      hrefFor: (...sources) => chainHref(sources),
      setSource: (source) => update(setCard(root.value, cardId, { source, state: "" })),
      becomeComposite: (name) => {
        const template = composite(name);
        if (template) update(resetTo(root.value, cardId, template, newCardId));
      },
      close: () => close(cardId),
      setState: (state) => {
        if (cardById(cardId)?.state !== state) update(setCard(root.value, cardId, { state }));
      },
    };
    ctx = {
      cardId,
      get cardType() {
        return cardType(cardById(cardId)?.source ?? "");
      },
      get initialState() {
        return cardById(cardId)?.state ?? "";
      },
      // A card resets its title to null each time it runs, then names
      // itself; keeping the last real name means an unmounted tab has one.
      setTitle: (title) => {
        if (title !== null && cardById(cardId)?.title !== title) {
          update(setCard(root.value, cardId, { title }));
        }
      },
      setHelp: (html) => setCardHelp(cardId, html),
      bus,
      host,
    };
    ctxCache.set(cardId, ctx);
  }
  return ctx;
}

function titleOf(node: TreeNode): string {
  if (node.name) return node.name;
  if (node.kind === "card") return displayTitle(node.source, node.title);
  const first = node.children[0];
  return first ? `${titleOf(first)}${node.children.length > 1 ? " …" : ""}` : "Empty";
}

// ---- the commands ----

function select(id: string) {
  update(reveal(root.value, id));
}

function close(id: string) {
  update(remove(root.value, id));
}

// A tab of the outermost container: the only kind that can be pinned.
function isTab(id: string): boolean {
  return root.value.children.some((c) => c.id === id);
}

function setPin(id: string, pinned: boolean) {
  update(setPinned(root.value, id, pinned));
}

// Pin or unpin, for a tab; and Close, which a pinned tab does not have.
function tabActions(node: TreeNode): PanelAction[] {
  const pin: PanelAction = {
    label: node.pinned ? "Unpin" : "Pin",
    icon: PANEL_ICONS.pin,
    run: () => setPin(node.id, !node.pinned),
  };
  const shut: PanelAction = {
    label: "Close",
    icon: PANEL_ICONS.close,
    danger: true,
    run: () => close(node.id),
  };
  return [...(isTab(node.id) ? [pin] : []), ...(node.pinned ? [] : [shut])];
}

function commitSource(card: CardNode, e: Event) {
  const source = (e.target as HTMLTextAreaElement).value;
  if (source !== card.source) update(setCard(root.value, card.id, { source, state: "" }));
}

function setLayout(id: string, layout: Layout) {
  update(setBoxLayout(root.value, id, layout));
}

function toggleSolidified(box: BoxNode) {
  update(setSolidified(root.value, box.id, !box.solidified));
}

function addCard(boxId: string, source = "galleryView()") {
  update(addChild(root.value, boxId, makeCard(newCardId(), source)));
}

function addBox(boxId: string, layout: Layout) {
  const box = makeBox(newCardId(), layout, [makeCard(newCardId(), "galleryView()")]);
  update(addChild(root.value, boxId, box));
}

// The toolbar's "New card", and its Logs and Data sources.
function newTab() {
  addCard(root.value.id);
}
// A card opened from the chrome gets a tab of its own.
function showCard(source: string) {
  const have = allCards.value.find((c) => c.source === source);
  if (have) select(have.id);
  else addCard(root.value.id, source);
}
defineExpose({ addCard: newTab, showCard });

// ---- naming ----

const asking = ref<{
  title: string;
  initial: string;
  check?: (name: string) => string | null;
  resolve: (name: string | null) => void;
} | null>(null);

function askName(
  title: string,
  initial: string,
  check?: (name: string) => string | null,
): Promise<string | null> {
  return new Promise((resolve) => {
    asking.value = { title, initial, check, resolve };
  });
}
function answer(name: string | null) {
  asking.value?.resolve(name);
  asking.value = null;
}

async function renameNode(node: TreeNode) {
  const name = await askName("Name", titleOf(node));
  if (name !== null) update(rename(root.value, node.id, name));
}

async function saveAsComposite(box: BoxNode) {
  const name = await askName("Save as composite", box.name ?? titleOf(box), (n) =>
    isBuiltinComposite(n) ? `"${n}" is a built-in composite; pick another name.` : null,
  );
  if (name === null) return;
  try {
    await saveComposite(name, box);
  } catch (e) {
    console.warn("could not save the composite", e);
    pushToast(`Could not save the composite "${name}" to the library.`);
    return;
  }
  update(setTemplate(root.value, box.id, name));
}

// ---- panels ----

const panel = ref<{ build: () => Panel; x: number; y: number } | null>(null);

function openPanel(ev: MouseEvent, build: () => Panel) {
  ev.preventDefault();
  ev.stopPropagation();
  panel.value = { build, x: ev.clientX, y: ev.clientY };
}

// Moving among siblings, named for the way the parent lays them out.
function moveActions(node: TreeNode): PanelAction[] {
  const parent = parentOf(root.value, node.id);
  if (!parent || parent.children.length < 2) return [];
  const across =
    parent.layout === "columns" || (parent.layout === "split" && parent.direction === "row");
  return [
    {
      label: across ? "Move left" : "Move up",
      icon: across ? PANEL_ICONS.left : PANEL_ICONS.up,
      run: () => update(move(root.value, node.id, -1)),
      stay: true,
    },
    {
      label: across ? "Move right" : "Move down",
      icon: across ? PANEL_ICONS.right : PANEL_ICONS.down,
      run: () => update(move(root.value, node.id, 1)),
      stay: true,
    },
  ];
}

// What a new container starts as; its tab changes the layout after.
const NEW_LAYOUT: Layout = "columns";

function wrapAction(node: TreeNode): PanelAction {
  return {
    label: "Put in a new container",
    icon: PANEL_ICONS.wrap,
    run: () => update(wrap(root.value, node.id, NEW_LAYOUT, newCardId())),
  };
}

function addSection(boxId: string): PanelSection {
  return {
    kind: "tiles",
    title: "Add",
    actions: [
      { label: "Card", icon: PANEL_ICONS.card, run: () => addCard(boxId) },
      {
        label: "Container",
        icon: LAYOUT_ICONS[NEW_LAYOUT],
        run: () => addBox(boxId, NEW_LAYOUT),
      },
    ],
  };
}

function boxPanel(box: BoxNode): Panel {
  const title = titleOf(box);
  const template = box.template ? composite(box.template) : undefined;
  return {
    title,
    icon: LAYOUT_ICONS[box.layout],
    sections: [
      {
        kind: "tiles",
        title: "Layout",
        actions: LAYOUTS.map((layout) => ({
          label: LAYOUT_LABELS[layout],
          icon: LAYOUT_ICONS[layout],
          current: box.layout === layout,
          run: () => setLayout(box.id, layout),
          stay: true,
        })),
      },
      ...(box.layout === "split"
        ? [
            {
              kind: "tiles" as const,
              title: "Direction",
              actions: DIRECTIONS.map((direction) => ({
                label: DIRECTION_LABELS[direction],
                icon: DIRECTION_ICONS[direction],
                current: box.direction === direction,
                run: () => update(setDirection(root.value, box.id, direction)),
                stay: true,
              })),
            },
          ]
        : []),
      {
        kind: "toggle",
        label: "Solidified",
        hint: "Keeps its shape: cards opened inside go to the next container out, and outside edit mode it shows no frames.",
        icon: PANEL_ICONS.solidified,
        on: box.solidified,
        run: () => toggleSolidified(box),
      },
      addSection(box.id),
      {
        kind: "rows",
        title: "Arrange",
        actions: [
          ...moveActions(box),
          wrapAction(box),
          {
            label: "Take the cards out",
            icon: PANEL_ICONS.takeOut,
            run: () => update(unwrap(root.value, box.id)),
          },
        ],
      },
      {
        kind: "rows",
        actions: [
          { label: "Rename…", icon: PANEL_ICONS.rename, run: () => void renameNode(box) },
          {
            label: "Save as composite…",
            icon: PANEL_ICONS.save,
            run: () => void saveAsComposite(box),
          },
          ...(template
            ? [
                {
                  label: `Reset to "${box.template}"`,
                  icon: PANEL_ICONS.reset,
                  run: () => update(resetTo(root.value, box.id, template, newCardId)),
                },
              ]
            : []),
          ...tabActions(box),
        ],
      },
    ],
  };
}

function cardPanel(card: CardNode): Panel {
  const moves = moveActions(card);
  return {
    title: titleOf(card),
    icon: PANEL_ICONS.card,
    sections: [
      { kind: "rows", title: "Arrange", actions: [...moves, wrapAction(card)] },
      {
        kind: "rows",
        actions: [
          { label: "Rename…", icon: PANEL_ICONS.rename, run: () => void renameNode(card) },
          ...tabActions(card),
        ],
      },
    ],
  };
}

// A node's panel, rebuilt from the live tree each time so it shows what
// an action that keeps it open just changed.
function panelFor(id: string): () => Panel {
  return () => {
    const node = find(root.value, id);
    if (!node) return { title: "", icon: "", sections: [] };
    return node.kind === "box" ? boxPanel(node) : cardPanel(node);
  };
}

// ---- resizing ----

const MIN_PX = 60;

function startResize(id: string, axis: "x" | "y", ev: PointerEvent) {
  if (ev.button !== 0) return;
  ev.preventDefault();
  const handle = ev.currentTarget as HTMLElement;
  const child = handle.previousElementSibling as HTMLElement | null;
  // A stack sizes a child's body, below its header (ContainerNode `height`).
  const sized = axis === "y" ? child?.querySelector<HTMLElement>("[data-body]") : child;
  if (!sized) return;
  const startSize = axis === "x" ? sized.offsetWidth : sized.offsetHeight;
  const start = axis === "x" ? ev.clientX : ev.clientY;
  handle.setPointerCapture(ev.pointerId);
  const onMove = (e: PointerEvent) => {
    const delta = (axis === "x" ? e.clientX : e.clientY) - start;
    update(setBasis(root.value, id, Math.max(MIN_PX, Math.round(startSize + delta))));
  };
  const onUp = (e: PointerEvent) => {
    handle.releasePointerCapture(e.pointerId);
    handle.removeEventListener("pointermove", onMove);
    handle.removeEventListener("pointerup", onUp);
    handle.removeEventListener("pointercancel", onUp);
  };
  handle.addEventListener("pointermove", onMove);
  handle.addEventListener("pointerup", onUp);
  handle.addEventListener("pointercancel", onUp);
}

// ---- what the page shows ----

// The sidebar's two lists: the pinned tabs, which stay put while the
// list below them scrolls, and the rest as a tree.
const pinnedRows = computed(() => pinnedTabs(root.value).map((node) => ({ node, depth: 0 })));
const openRows = computed(() => tabRows(root.value));
const lists = computed(() => [
  ...(pinnedRows.value.length
    ? [{ key: "pinned", label: "pinned tabs", rows: pinnedRows.value }]
    : []),
  { key: "open", label: "open tabs", rows: openRows.value },
]);
const selectedTab = computed(() => root.value.children.find((c) => c.id === root.value.selected));

// The tabs, in any tabs container, shown at least once. A shown tab is
// then hidden in place rather than unmounted when another is picked: its
// cards' DOM must not move, since taking a <style> out of the page and
// putting it back rebuilds its stylesheet, and a grid that keeps its
// own column rules (SlickGrid) goes on writing to the old ones.
const shownTabs = reactive(new Set<string>());
function markShown(id: string | null) {
  if (id !== null) shownTabs.add(id);
}
watch(() => root.value.selected, markShown, { immediate: true });
watch(root, (tree) => {
  const live = new Set<string>();
  const walk = (n: TreeNode) => {
    live.add(n.id);
    if (n.kind === "box") n.children.forEach(walk);
  };
  walk(tree);
  for (const id of [...shownTabs]) if (!live.has(id)) shownTabs.delete(id);
});

watchEffect(() => {
  const tab = selectedTab.value;
  document.title = tab ? `${titleOf(tab)} · Datalib` : "Datalib";
});

const api: ContainersApi = {
  ctxFor,
  titleOf,
  setSlot,
  chromeShown: (id) => editMode.value || !isSolidified(root.value, id),
  isSolidified: (id) => isSolidified(root.value, id),
  select,
  close,
  commitSource,
  openPanel: (ev, build) => openPanel(ev, build),
  tabShown: (id) => shownTabs.has(id),
  markShown,
  addCard,
  panelFor,
  startResize,
};
provide(CONTAINERS_API, api);
</script>

<template>
  <div class="ct-root">
    <nav class="ct-sidebar" aria-label="tabs">
      <ul
        v-for="list in lists"
        :key="list.key"
        class="ct-tabs"
        :class="`ct-tabs-${list.key}`"
        role="tree"
        :aria-label="list.label"
      >
        <li
          v-for="row in list.rows"
          :key="row.node.id"
          class="ct-tab"
          :class="{ 'is-selected': row.node.id === root.selected, 'is-pinned': row.node.pinned }"
          role="treeitem"
          :aria-selected="row.node.id === root.selected"
          :data-node-id="row.node.id"
          :style="{ paddingLeft: 0.4 + row.depth * 0.9 + 'rem' }"
          :title="titleOf(row.node)"
          @click="select(row.node.id)"
          @contextmenu="openPanel($event, panelFor(row.node.id))"
          @auxclick.prevent="
            (e: MouseEvent) => e.button === 1 && !row.node.pinned && close(row.node.id)
          "
        >
          <CardIcon v-if="row.node.kind === 'card'" class="ct-tab-icon" :source="row.node.source" />
          <svg v-else class="ct-tab-icon ct-tab-glyph" viewBox="0 0 24 24" aria-hidden="true">
            <path :d="LAYOUT_ICONS[row.node.layout]" />
          </svg>
          <span class="ct-tab-label" @dblclick.stop="renameNode(row.node)">{{
            titleOf(row.node)
          }}</span>
          <button
            class="ct-tab-action"
            title="more"
            @click.stop="openPanel($event, panelFor(row.node.id))"
          >
            ⋯
          </button>
          <button
            v-if="row.node.pinned"
            class="ct-tab-action ct-tab-unpin"
            title="unpin"
            @click.stop="setPin(row.node.id, false)"
          >
            <svg class="ct-tab-glyph" viewBox="0 0 24 24" aria-hidden="true">
              <path :d="PANEL_ICONS.pin" />
            </svg>
          </button>
          <button v-else class="ct-tab-action" title="close" @click.stop="close(row.node.id)">
            ✕
          </button>
        </li>
        <li v-if="list.key === 'open'" class="ct-new-row" role="none">
          <button
            class="ct-new"
            title="a new tab, with a card that asks what it should show"
            @click="newTab"
          >
            ＋ New card
          </button>
        </li>
      </ul>
    </nav>
    <section class="ct-main">
      <template v-for="tab in root.children" :key="tab.id">
        <ContainerNode
          v-if="shownTabs.has(tab.id)"
          v-show="tab.id === root.selected"
          :class="{ 'ct-hidden-pane': tab.id !== root.selected }"
          :node="tab"
          parent-layout="tabs"
        />
      </template>
    </section>

    <div class="ct-pool" aria-hidden="true">
      <Teleport
        v-for="card in pool"
        :key="card.id"
        :to="slots.get(card.id)"
        :disabled="!slots.get(card.id)"
      >
        <ShadowCard
          class="ct-mounted-card"
          :source="card.source"
          :ctx="ctxFor(card)"
          :natural="naturalCards.has(card.id)"
        />
      </Teleport>
    </div>

    <ContainerMenu
      v-if="panel"
      :build="panel.build"
      :x="panel.x"
      :y="panel.y"
      @close="panel = null"
    />
    <NameDialog
      v-if="asking"
      :title="asking.title"
      :initial="asking.initial"
      :check="asking.check"
      @done="answer"
    />
  </div>
</template>

<style scoped>
.ct-root {
  display: flex;
  flex: 1 1 0;
  min-height: 0;
}
.ct-sidebar {
  flex: 0 0 240px;
  display: flex;
  flex-direction: column;
  min-height: 0;
  background: var(--datalib-sidebar);
  border-right: 1px solid var(--datalib-border);
}
.ct-tabs {
  flex: 1 1 auto;
  min-height: 0;
  overflow-y: auto;
  margin: 0;
  padding: var(--datalib-pad) 6px;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 3px;
}
.ct-tabs-pinned {
  flex: 0 0 auto;
  max-height: 50%;
  border-bottom: 1px solid var(--datalib-border);
}
.ct-tab {
  display: flex;
  align-items: center;
  gap: 6px;
  height: var(--datalib-row-h);
  padding-right: 4px;
  cursor: pointer;
  border-radius: var(--datalib-radius);
  color: var(--datalib-fg);
}
.ct-tab:hover {
  background: var(--datalib-hover);
}
.ct-tab.is-selected {
  background: var(--datalib-selected);
  font-weight: 600;
}
.ct-tab-icon {
  flex: 0 0 auto;
  width: var(--datalib-icon-size);
  height: var(--datalib-icon-size);
  color: var(--datalib-muted);
}
.ct-tab-glyph {
  fill: none;
  stroke: currentColor;
  stroke-width: 2.2;
  stroke-linejoin: round;
}
.ct-tab.is-selected .ct-tab-icon {
  color: var(--datalib-accent);
}
.ct-tab-label {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ct-tab-action {
  flex: 0 0 auto;
  visibility: hidden;
  padding: 0 0.2rem;
  border: none;
  border-radius: 3px;
  background: transparent;
  color: var(--datalib-muted);
  cursor: pointer;
  font-size: 11px;
}
.ct-tab:hover .ct-tab-action,
.ct-tab.is-selected .ct-tab-action {
  visibility: visible;
}
/* A pinned tab's pin shows always, as the mark that it is pinned. */
.ct-tab-unpin {
  visibility: visible;
  display: flex;
  align-items: center;
}
.ct-tab-unpin svg {
  width: 12px;
  height: 12px;
}
.ct-tab-action:hover {
  background: var(--datalib-hover);
  color: var(--datalib-fg);
}
.ct-new {
  width: 100%;
  height: var(--datalib-row-h);
  box-sizing: border-box;
  padding: 0 6px;
  cursor: pointer;
  font: inherit;
  text-align: left;
  color: var(--datalib-muted);
  background: transparent;
  border: 1px dashed var(--datalib-border);
  border-radius: var(--datalib-radius);
}
.ct-new:hover {
  color: var(--datalib-fg);
  background: var(--datalib-hover);
}
.ct-main {
  flex: 1 1 auto;
  min-width: 0;
  display: flex;
  flex-direction: column;
  background: var(--datalib-surface);
  overflow: hidden;
}
.ct-pool {
  display: none;
}
.ct-mounted-card {
  flex: 1 1 auto;
}
.ct-mounted-card[data-natural] {
  flex: 0 0 auto;
}
.ct-mounted-card {
  min-width: 0;
  min-height: 0;
}
</style>
