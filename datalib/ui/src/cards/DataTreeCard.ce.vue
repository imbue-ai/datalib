<script setup lang="ts">
// The library as a tree: the person's folders of sources at the top,
// then each source's places, threads and messages, a page of children
// at a time as each node opens (`/children`). Headless Tree keeps which
// nodes are open, focused and selected, and the keyboard; this file
// draws its flat list of rows. What the card decides is in dataTree.ts.
import { computed, nextTick, onBeforeUnmount, onMounted, onUpdated, ref } from "vue";
import {
  asyncDataLoaderFeature,
  createTree,
  dragAndDropFeature,
  hotkeysCoreFeature,
  renamingFeature,
  selectionFeature,
  type DragTarget,
  type ItemInstance,
} from "@headless-tree/core";
import { GRID_INDEX_STORE, type ChildrenPage } from "@/api";
import { formatStamp, formatShortStamp } from "@/config/timeFormat";
import { useApi } from "./cardApi";
import { documentSource } from "./cardSources";
import { isBrowserClick } from "./chatLink";
import { uriFromEntity } from "./chipLinks";
import EntityChip from "./EntityChip.ce.vue";
import {
  ROOT,
  SOURCES_SQL,
  SOURCE_TOP_SQL,
  addFolder,
  canMoveInto,
  decodeState,
  encodeState,
  isFolderId,
  isFolderItem,
  isSourceId,
  itemName,
  joinPage,
  layoutChildren,
  moveInto,
  nodeTitle,
  pageSize,
  removeFolder,
  renameFolder,
  shownChildren,
  sourceItemId,
  sourceOf,
  sourceTopPage,
  sourceTopParams,
  sourcesOf,
  type Dir,
  type Listed,
  type TreeItem,
} from "./dataTree";
import type { CardCtx } from "./types";

const props = defineProps<{ ctx: CardCtx; opts: { page?: number } }>();
const api = useApi();

const PAGE = pageSize(props.opts.page);

props.ctx.setTitle("Library tree");
props.ctx.setHelp(`
<p>Your library as a tree: your sources, then what each holds — a
workspace, its channels, their threads and messages — opened a page at a
time.</p>
<ul>
<li>Click a row with a triangle, or press → and ←, to open and close it.
A long list shows a <b>More…</b> row at its end; open it for the next page.</li>
<li><b>New folder</b> files sources under a name of your own. Drag a source
or a folder onto a folder to move it; F2 renames the focused folder.</li>
<li>Double-click a message or a thread, or use its <b>Open</b> link, to
read its document beside this card.</li>
<li>What is open, and your folders, are kept with the card.</li>
</ul>`);

const saved = decodeState(props.ctx.initialState);
let layout = saved.layout;
let sources: { id: string; label: string; count: number | null }[] = [];
let shown: Record<string, string[]> = { [ROOT]: [] };
const listed = new Map<string, Listed>();
const loadingMore = ref(new Set<string>());
const loadError = ref<string | null>(null);
const version = ref(0);

function dataOf(id: string): TreeItem {
  if (isFolderId(id)) return { type: "folder", name: layout.names[id] ?? "Folder" };
  const s = sources.find((x) => sourceItemId(x.id) === id);
  return s
    ? { type: "source", sourceId: s.id, label: s.label, count: s.count }
    : { type: "loading" };
}

function withData(ids: string[]): { id: string; data: TreeItem }[] {
  return ids.map((id) => ({ id, data: dataOf(id) }));
}

async function fetchPage(parent: string, dir: Dir | null, cursor: string | null) {
  if (isSourceId(parent)) {
    const sourceId = sourceOf(parent);
    const r = await api.fetchSql(
      GRID_INDEX_STORE,
      SOURCE_TOP_SQL,
      sourceTopParams(sourceId, dir === "after" ? cursor : null, PAGE),
      PAGE + 1,
    );
    return sourceTopPage(r.columns, r.rows, PAGE);
  }
  const ask = dir && cursor ? { [dir]: cursor, limit: PAGE } : { limit: PAGE };
  return api.fetchChildren(parent, ask);
}

function pageData(parent: string, l: Listed, page: ChildrenPage) {
  const byId = new Map(page.children.map((c) => [c.uuid, c]));
  return shownChildren(parent, l).map((id) => {
    const child = byId.get(id);
    if (child) return { id, data: { type: "node", child } as TreeItem };
    if (id.startsWith("more:")) {
      const dir: Dir = id.startsWith("more:before:") ? "before" : "after";
      return { id, data: { type: "more", parent, dir } as TreeItem };
    }
    return { id, data: tree.getItemInstance(id).getItemData() };
  });
}

async function loadChildren(id: string): Promise<{ id: string; data: TreeItem }[]> {
  try {
    if (id === ROOT) {
      const r = await api.fetchSql(GRID_INDEX_STORE, SOURCES_SQL, {}, 1000);
      sources = sourcesOf(r.rows);
      shown = layoutChildren(layout, sources);
      return withData(shown[ROOT]);
    }
    if (isFolderId(id)) return withData(shown[id] ?? []);
    const page = await fetchPage(id, null, null);
    const l = joinPage(null, page, null);
    listed.set(id, l);
    return pageData(id, l, page);
  } catch (e) {
    loadError.value = (e as Error).message;
    return [];
  }
}

async function loadMore(parent: string, dir: Dir) {
  const l = listed.get(parent);
  const more = `more:${dir}:${parent}`;
  if (!l || loadingMore.value.has(more)) return;
  loadingMore.value = new Set([...loadingMore.value, more]);
  try {
    const page = await fetchPage(parent, dir, l[dir]);
    const joined = joinPage(l, page, dir);
    listed.set(parent, joined);
    for (const { id, data } of pageData(parent, joined, page)) {
      tree.getItemInstance(id).updateCachedData(data, true);
    }
    tree.getItemInstance(parent).updateCachedChildrenIds(shownChildren(parent, joined));
  } catch (e) {
    loadError.value = (e as Error).message;
  } finally {
    const rest = new Set(loadingMore.value);
    rest.delete(more);
    loadingMore.value = rest;
  }
}

/// The folders and the root list what `layout` says, now.
function relayout(next: typeof layout) {
  layout = next;
  shown = layoutChildren(layout, sources);
  for (const parent of Object.keys(shown)) {
    for (const { id, data } of withData(shown[parent])) {
      tree.getItemInstance(id).updateCachedData(data, true);
    }
    tree.getItemInstance(parent).updateCachedChildrenIds(shown[parent], true);
  }
  tree.rebuildTree();
  persist();
}

let last = encodeState(saved);
function persist() {
  const s = tree.getState();
  const next = encodeState({
    expanded: s.expandedItems,
    selected: s.selectedItems ?? [],
    focused: s.focusedItem,
    layout,
  });
  if (next === last) return;
  last = next;
  props.ctx.host.setState(next);
}

const tree = createTree<TreeItem>({
  rootItemId: ROOT,
  initialState: {
    expandedItems: saved.expanded,
    selectedItems: saved.selected,
    focusedItem: saved.focused,
  },
  getItemName: (item) => itemName(item.getItemData()),
  isItemFolder: (item) => isFolderItem(item.getItemData()),
  createLoadingItemData: () => ({ type: "loading" }),
  dataLoader: {
    getItem: (id) => dataOf(id),
    getChildrenWithData: loadChildren,
  },
  setState: () => {
    version.value++;
    persist();
  },
  onPrimaryAction: (item) => {
    const d = item.getItemData();
    if (d?.type === "more") void loadMore(d.parent, d.dir);
  },
  canReorder: true,
  canDrag: (items) => items.every((i) => isSourceId(i.getId()) || isFolderId(i.getId())),
  canDrop: (items, target) =>
    canMoveInto(
      layout,
      items.map((i) => i.getId()),
      target.item.getId(),
    ),
  onDrop: (items: ItemInstance<TreeItem>[], target: DragTarget<TreeItem>) => {
    const index = "childIndex" in target ? target.childIndex : undefined;
    const ids = items.map((i) => i.getId());
    relayout(moveInto(layout, shown, ids, target.item.getId(), index));
  },
  canRename: (item) => isFolderId(item.getId()),
  onRename: (item, value) => {
    const name = value.trim();
    if (name) relayout(renameFolder(layout, item.getId(), name));
  },
  indent: 16,
  features: [
    asyncDataLoaderFeature,
    selectionFeature,
    hotkeysCoreFeature,
    dragAndDropFeature,
    renamingFeature,
  ],
});

const view = computed(() => ({ v: version.value, items: tree.getItems() }));

/// Headless Tree hands out React-style props; Vue binds `onDragstart`,
/// not `onDragStart`, and takes an element through a ref of its own.
function vueProps(p: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(p)) {
    if (k === "ref") continue;
    if (k === "tabIndex") out.tabindex = v;
    else if (/^on[A-Z]/.test(k)) out[`on${k[2]}${k.slice(3).toLowerCase()}`] = v;
    else out[k] = v;
  }
  return out;
}

const containerProps = computed(() => {
  void version.value;
  return vueProps(tree.getContainerProps("Library tree"));
});

function setContainer(el: unknown) {
  tree.registerElement((el as HTMLElement | null) ?? null);
}

// The rename box takes the focus once, after the render that draws it:
// a ref callback runs while the row it replaces still holds the focus.
let renameFocused: string | null = null;
onUpdated(() => {
  const id = tree.getState().renamingItem ?? null;
  if (!id) renameFocused = null;
  else if (id !== renameFocused) {
    const input = tree.getElement()?.querySelector<HTMLInputElement>("input.rename");
    if (input) {
      input.focus();
      input.select();
      renameFocused = id;
    }
  }
});

onMounted(() => {
  tree.setMounted(true);
  tree.rebuildTree();
});
onBeforeUnmount(() => {
  tree.setMounted(false);
  tree.registerElement(null);
});

function newFolder() {
  const key = crypto.randomUUID().slice(0, 8);
  relayout(addFolder(layout, key, "New folder"));
  void nextTick(() => tree.getItemInstance(`folder:${key}`).startRenaming());
}

function deleteFolder(id: string) {
  relayout(removeFolder(layout, id));
}

function collapseAll() {
  for (const item of tree.getItems()) if (item.isExpanded()) item.collapse();
}

function docSource(item: TreeItem): string | null {
  if (item.type !== "node" || !item.child.markdown_uuid) return null;
  return documentSource(item.child.markdown_uuid, item.child.uuid);
}

function openDoc(ev: MouseEvent, source: string) {
  if (isBrowserClick(ev)) return;
  ev.preventDefault();
  props.ctx.host.openCards(source);
}

function onDblClick(item: ItemInstance<TreeItem>) {
  const source = docSource(item.getItemData());
  if (source) props.ctx.host.openCards(source);
}

function renameInput(e: Event) {
  tree.applySubStateUpdate("renamingValue", (e.target as HTMLInputElement).value);
}

function rowClass(item: ItemInstance<TreeItem>) {
  return {
    focused: item.isFocused(),
    selected: item.isSelected(),
    "drop-target": item.isDragTarget(),
    more: item.getItemData().type === "more",
  };
}

function subtitle(d: TreeItem): string {
  if (d.type === "source") return d.count != null ? d.count.toLocaleString() : "";
  if (d.type === "folder") return "";
  if (d.type !== "node") return "";
  const c = d.child;
  if (c.container) return c.kind;
  return [c.author, c.created_at ? formatShortStamp(c.created_at) : ""].filter(Boolean).join(" · ");
}

function sourceUri(d: TreeItem): string {
  return d.type === "source" ? uriFromEntity("group", d.sourceId) : "";
}

function stampOf(d: TreeItem): string {
  return d.type === "node" && d.child.created_at ? formatStamp(d.child.created_at) : "";
}

function folderCount(id: string): number {
  return shown[id]?.length ?? 0;
}
</script>

<template>
  <div class="data-tree">
    <div class="toolbar">
      <button class="btn" @click="newFolder">New folder</button>
      <button class="btn" @click="collapseAll">Collapse all</button>
      <span v-if="loadError" class="error" :title="loadError">{{ loadError }}</span>
    </div>
    <div :ref="setContainer" class="tree" v-bind="containerProps">
      <template v-for="item in view.items" :key="item.getId()">
        <div
          v-if="item.isRenaming()"
          class="row renaming"
          :style="{ paddingLeft: `${item.getItemMeta().level * 16 + 22}px` }"
        >
          <input
            class="rename"
            :value="tree.getRenamingValue()"
            @input="renameInput"
            @blur="tree.completeRenaming()"
          />
        </div>
        <div
          v-else
          :ref="(el) => item.registerElement((el as HTMLElement | null) ?? null)"
          class="row"
          :class="rowClass(item)"
          :style="{ paddingLeft: `${item.getItemMeta().level * 16 + 4}px` }"
          v-bind="vueProps(item.getProps())"
          @dblclick="onDblClick(item)"
        >
          <span class="twisty" :class="{ open: item.isExpanded(), leaf: !item.isFolder() }">
            <span v-if="item.isLoading()" class="spinner" />
            <svg v-else-if="item.isFolder()" viewBox="0 0 16 16" aria-hidden="true">
              <path d="M6 4l4 4-4 4" />
            </svg>
          </span>
          <template v-if="item.getItemData().type === 'source'">
            <span class="chip-slot">
              <EntityChip
                :uri="sourceUri(item.getItemData())"
                :shown="item.getItemName()"
                quiet
                :open-card="(source: string) => ctx.host.openCards(source)"
              />
            </span>
          </template>
          <template v-else-if="item.getItemData().type === 'folder'">
            <svg class="glyph" viewBox="0 0 24 24" aria-hidden="true">
              <path
                d="M10 4H4c-1.1 0-2 .9-2 2v12c0 1.1.9 2 2 2h16c1.1 0 2-.9 2-2V8c0-1.1-.9-2-2-2h-8l-2-2z"
              />
            </svg>
            <span class="label strong">{{ item.getItemName() }}</span>
            <span class="meta">{{ folderCount(item.getId()) }}</span>
            <button
              class="icon-btn"
              title="Remove this folder; what it holds moves up"
              @click.stop="deleteFolder(item.getId())"
            >
              ×
            </button>
          </template>
          <template v-else-if="item.getItemData().type === 'more'">
            <span class="label muted">
              {{ loadingMore.has(item.getId()) ? "Loading…" : item.getItemName() }}
            </span>
          </template>
          <template v-else>
            <span
              class="label"
              :class="{ strong: item.isFolder() }"
              :title="stampOf(item.getItemData())"
              >{{ item.getItemName() }}</span
            >
            <span class="meta">{{ subtitle(item.getItemData()) }}</span>
            <a
              v-if="docSource(item.getItemData())"
              class="open-link"
              :href="ctx.host.hrefFor(docSource(item.getItemData())!)"
              @click.stop="openDoc($event, docSource(item.getItemData())!)"
              >Open</a
            >
          </template>
        </div>
      </template>
      <div v-if="view.items.length === 0 && !loadError" class="empty">Loading the library…</div>
      <div class="drag-line" :style="tree.getDragLineStyle()" />
    </div>
  </div>
</template>

<style>
.data-tree {
  display: flex;
  flex-direction: column;
  height: 100%;
  background: var(--datalib-bg);
  color: var(--datalib-fg);
  font-family: var(--datalib-font);
  font-size: var(--datalib-font-size);
}
.toolbar {
  display: flex;
  align-items: center;
  gap: var(--datalib-gap);
  padding: 6px var(--datalib-pad);
  border-bottom: 1px solid var(--datalib-border-soft);
}
.btn {
  height: var(--datalib-control-h);
  padding: 0 10px;
  border: 1px solid var(--datalib-border);
  border-radius: 6px;
  background: transparent;
  color: inherit;
  font: inherit;
  cursor: pointer;
}
.btn:hover {
  background: var(--datalib-hover);
}
.error {
  color: var(--datalib-log-error);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.tree {
  position: relative;
  flex: 1;
  overflow: auto;
  padding: 4px 0;
  outline: none;
}
.row {
  display: flex;
  align-items: center;
  gap: 6px;
  height: var(--datalib-row-h);
  padding-right: var(--datalib-pad);
  cursor: default;
  white-space: nowrap;
  outline: none;
  border-radius: 4px;
  margin: 0 4px;
}
.row:hover {
  background: var(--datalib-hover);
}
.row.selected {
  background: var(--datalib-selected);
}
.row.focused:focus-visible,
.row.focused:focus {
  box-shadow: inset 0 0 0 1.5px var(--datalib-accent);
}
.row.drop-target {
  background: color-mix(in srgb, var(--datalib-accent) 18%, transparent);
}
.twisty {
  flex: none;
  width: 16px;
  height: 16px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  color: var(--datalib-muted);
}
.twisty svg {
  width: 12px;
  height: 12px;
  fill: none;
  stroke: currentColor;
  stroke-width: 1.8;
  stroke-linecap: round;
  stroke-linejoin: round;
  transition: transform 120ms ease;
}
.twisty.open svg {
  transform: rotate(90deg);
}
.spinner {
  width: 10px;
  height: 10px;
  border: 1.5px solid var(--datalib-border);
  border-top-color: var(--datalib-accent);
  border-radius: 50%;
  animation: spin 0.8s linear infinite;
}
@keyframes spin {
  to {
    transform: rotate(360deg);
  }
}
.glyph {
  flex: none;
  width: var(--datalib-icon-size);
  height: var(--datalib-icon-size);
  fill: var(--datalib-muted);
}
.label {
  overflow: hidden;
  text-overflow: ellipsis;
  min-width: 0;
}
.strong {
  font-weight: 600;
}
.muted,
.row.more .label {
  color: var(--datalib-muted);
}
.row.more:hover .label {
  color: var(--datalib-accent);
}
.meta {
  flex: none;
  margin-left: auto;
  padding-left: 12px;
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
}
.chip-slot {
  min-width: 0;
  overflow: hidden;
}
.open-link,
.icon-btn {
  flex: none;
  visibility: hidden;
  font-size: var(--datalib-font-size-small);
}
.row:hover .open-link,
.row.focused .open-link,
.row:hover .icon-btn {
  visibility: visible;
}
.icon-btn {
  border: none;
  background: transparent;
  color: var(--datalib-muted);
  cursor: pointer;
  font-size: 14px;
  line-height: 1;
}
.icon-btn:hover {
  color: var(--datalib-fg);
}
.rename {
  font: inherit;
  height: calc(var(--datalib-row-h) - 6px);
  padding: 0 6px;
  border: 1px solid var(--datalib-accent);
  border-radius: 4px;
  background: var(--datalib-bg);
  color: inherit;
}
.drag-line {
  height: 2px;
  background: var(--datalib-accent);
  border-radius: 1px;
}
.empty {
  padding: var(--datalib-pad);
  color: var(--datalib-muted);
}
</style>
