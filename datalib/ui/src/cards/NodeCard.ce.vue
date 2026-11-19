<script setup lang="ts">
// `nodeView({ node, name })`: one node of the library's tree — a source's
// top, a container — as a list of what it holds, newest at the bottom,
// read a page at a time from `/children`. A picked row's document is read
// beside the list, as the Search card's preview is; a level of containers
// alone (an account's labels) is a list and nothing more. A double-click,
// or Enter, opens the child: a container's own children, a row's document.
// The rules are cards/library.ts.
import { nextTick, onBeforeUnmount, onMounted, ref, useTemplateRef, watch } from "vue";
import type { Child, ChildrenPage } from "@/api";
import { useApi } from "@/cards/cardApi";
import type { CardCtx, Teardown } from "./types";
import { formatRelative } from "@/config/timeFormat";
import { documentPreview } from "./libs/documentView";
import {
  childCardSource,
  childLabel,
  kindsDiffer,
  previewArgs,
  rowParts,
  wantsPreview,
} from "./library";

const props = defineProps<{ ctx: CardCtx; node: string; name: string | null }>();
const api = useApi();

const PAGE = 50;
const children = ref<Child[]>([]);
const before = ref<string | null>(null);
const after = ref<string | null>(null);
const loading = ref(true);
const failed = ref<string | null>(null);
const picked = ref<Child | null>(null);
const withPreview = ref(false);
const listEl = useTemplateRef<HTMLUListElement>("listEl");
const now = Date.now();

function savedPick(): string | null {
  try {
    const sel = JSON.parse(props.ctx.initialState || "{}").sel;
    return typeof sel === "string" ? sel : null;
  } catch {
    return null;
  }
}

function take(page: ChildrenPage, where: "first" | "earlier" | "later") {
  if (where === "first") {
    children.value = page.children;
    before.value = page.before;
    after.value = page.after;
  } else if (where === "earlier") {
    children.value = [...page.children, ...children.value];
    before.value = page.before;
  } else {
    children.value = [...children.value, ...page.children];
    after.value = page.after;
  }
  withPreview.value = wantsPreview(children.value);
}

async function load() {
  const sel = savedPick();
  try {
    // At the newest end, as a channel or a mailbox is read.
    take(await api.fetchChildren(props.node, { at: "now", limit: PAGE }), "first");
    picked.value = children.value.find((c) => c.uuid === sel) ?? null;
    await nextTick();
    const at = picked.value ? rowOf(picked.value.uuid) : null;
    if (at) at.scrollIntoView({ block: "center" });
    else if (listEl.value) listEl.value.scrollTop = listEl.value.scrollHeight;
  } catch (e) {
    failed.value = (e as Error).message;
  } finally {
    loading.value = false;
  }
}

async function loadEarlier() {
  if (!before.value) return;
  const el = listEl.value;
  const fromBottom = el ? el.scrollHeight - el.scrollTop : 0;
  take(await api.fetchChildren(props.node, { before: before.value, limit: PAGE }), "earlier");
  await nextTick();
  if (el) el.scrollTop = el.scrollHeight - fromBottom;
}

async function loadLater() {
  if (!after.value) return;
  take(await api.fetchChildren(props.node, { after: after.value, limit: PAGE }), "later");
}

function rowOf(uuid: string): HTMLElement | null {
  return listEl.value?.querySelector<HTMLElement>(`[data-uuid="${CSS.escape(uuid)}"]`) ?? null;
}

function pick(c: Child) {
  picked.value = c;
  props.ctx.host.setState(JSON.stringify({ sel: c.uuid }));
}

function open(c: Child | null) {
  const source = c && childCardSource(c);
  if (source) props.ctx.host.openCards(source);
}

function onKey(e: KeyboardEvent) {
  const list = children.value;
  const i = picked.value ? list.findIndex((c) => c.uuid === picked.value!.uuid) : -1;
  const step = e.key === "ArrowDown" ? 1 : e.key === "ArrowUp" ? -1 : 0;
  if (step !== 0) {
    e.preventDefault();
    const next = list[Math.min(list.length - 1, Math.max(0, i + step))];
    if (next) {
      pick(next);
      rowOf(next.uuid)?.focus();
    }
  } else if (e.key === "Enter") {
    e.preventDefault();
    open(picked.value);
  }
}

function when(iso: string | null): string {
  return iso ? formatRelative(iso, now) : "";
}

// The picked row's document, drawn in a shadow root of its own.
const previewEl = useTemplateRef<HTMLDivElement>("previewEl");
let previewRoot: ShadowRoot | null = null;
let teardown: Teardown | null = null;

function showPreview(c: Child | null) {
  teardown?.();
  teardown = null;
  if (!previewEl.value) return;
  previewRoot ??= previewEl.value.attachShadow({ mode: "open" });
  previewRoot.replaceChildren();
  const args = c && previewArgs(c);
  if (!args) return;
  teardown = documentPreview(
    args[0],
    args[1],
    [],
  )(previewRoot, {
    cardId: `${props.ctx.cardId}:preview`,
    cardType: "documentView",
    initialState: "",
    setTitle: () => {},
    setHelp: () => {},
    bus: props.ctx.bus,
    host: { ...props.ctx.host, setState: () => {}, setSource: () => {}, close: () => {} },
  });
}
watch([picked, withPreview], ([c]) => showPreview(c), { flush: "post" });

onMounted(() => {
  props.ctx.setTitle(props.name || "Library");
  props.ctx.setHelp(
    "<p>What this part of the library holds, newest at the bottom. " +
      "<b>Earlier</b> and <b>Later</b> read on past either end.</p>" +
      "<p>A click reads a message or a document beside the list. " +
      "A double-click, or Enter, opens it: a channel, a label or a folder " +
      "shows what it holds, a message or a document opens as a card.</p>",
  );
  void load();
});
onBeforeUnmount(() => teardown?.());
</script>

<template>
  <div class="nc">
    <p v-if="failed" class="nc-note nc-failed">{{ failed }}</p>
    <div class="nc-main">
      <div class="nc-side" :class="{ 'is-alone': !withPreview }">
        <button v-if="before" class="nc-more" @click="loadEarlier">Earlier</button>
        <ul
          ref="listEl"
          class="nc-list"
          aria-label="Children"
          data-testid="node-children"
          @keydown="onKey"
        >
          <li v-for="c in children" :key="c.uuid">
            <button
              class="nc-row"
              :class="{ 'is-picked': picked?.uuid === c.uuid, 'is-container': c.container }"
              :data-uuid="c.uuid"
              :tabindex="(picked ?? children[children.length - 1])?.uuid === c.uuid ? 0 : -1"
              @click="pick(c)"
              @dblclick="open(c)"
            >
              <span class="nc-line">
                <strong class="nc-title">{{ rowParts(c, name).title }}</strong>
                <span class="nc-when">{{ when(c.created_at) }}</span>
              </span>
              <span v-if="kindsDiffer(children)" class="nc-meta">
                <span class="nc-kind">{{ c.kind }}</span>
              </span>
              <span v-if="rowParts(c, name).snippet" class="nc-snippet">{{
                rowParts(c, name).snippet
              }}</span>
            </button>
          </li>
          <li v-if="!loading && children.length === 0" class="nc-note">Nothing here.</li>
          <li v-if="loading" class="nc-note">Reading…</li>
        </ul>
        <button v-if="after" class="nc-more" @click="loadLater">Later</button>
      </div>
      <div v-show="withPreview" class="nc-preview">
        <p v-if="picked && !previewArgs(picked)" class="nc-note">
          Double-click to open {{ childLabel(picked) }}.
        </p>
        <p v-else-if="!picked" class="nc-note">Pick something to read it here.</p>
        <div ref="previewEl" class="nc-preview-body" />
      </div>
    </div>
  </div>
</template>

<style scoped>
.nc {
  height: 100%;
  display: flex;
  flex-direction: column;
  font-size: var(--datalib-font-size);
  color: var(--datalib-fg);
  background: var(--datalib-bg);
}
.nc-main {
  flex: 1 1 auto;
  min-height: 0;
  display: flex;
}
.nc-side {
  flex: 0 0 min(420px, 42%);
  min-width: 0;
  display: flex;
  flex-direction: column;
  border-right: 1px solid var(--datalib-border-soft);
}
.nc-side.is-alone {
  flex: 1 1 auto;
  border-right: 0;
}
.nc-list {
  flex: 1 1 auto;
  min-height: 0;
  margin: 0;
  padding: 0;
  list-style: none;
  overflow-y: auto;
}
.nc-row {
  display: flex;
  flex-direction: column;
  gap: 2px;
  width: 100%;
  padding: 6px var(--datalib-pad);
  font: inherit;
  text-align: left;
  color: inherit;
  background: none;
  border: 0;
  border-left: 3px solid transparent;
  border-bottom: 1px solid var(--datalib-border-soft);
  cursor: pointer;
}
.nc-row:hover {
  background: var(--datalib-hover);
}
.nc-row.is-picked {
  background: color-mix(in srgb, var(--datalib-accent) 10%, var(--datalib-bg));
  border-left-color: var(--datalib-accent);
}
.nc-line {
  display: flex;
  gap: 8px;
  align-items: baseline;
}
.nc-title {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-weight: 500;
}
.nc-row.is-container .nc-title {
  font-weight: 600;
}
.nc-when,
.nc-meta {
  flex: 0 0 auto;
  color: var(--datalib-faint);
  font-size: var(--datalib-font-size-small);
}
.nc-meta {
  display: flex;
  gap: 8px;
}
.nc-kind {
  text-transform: uppercase;
  letter-spacing: 0.03em;
}
.nc-snippet {
  color: var(--datalib-muted);
  line-height: 1.4;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
}
.nc-more {
  flex: 0 0 auto;
  padding: 4px var(--datalib-pad);
  font: inherit;
  color: var(--datalib-accent);
  background: var(--datalib-surface);
  border: 0;
  border-bottom: 1px solid var(--datalib-border-soft);
  cursor: pointer;
  text-align: left;
}
.nc-more:last-child {
  border-bottom: 0;
  border-top: 1px solid var(--datalib-border-soft);
}
.nc-note {
  margin: 0;
  padding: 10px var(--datalib-pad);
  color: var(--datalib-muted);
}
.nc-failed {
  color: var(--datalib-error-fg);
  background: var(--datalib-error-bg);
}
.nc-preview {
  flex: 1 1 auto;
  min-width: 0;
  display: flex;
  flex-direction: column;
}
.nc-preview-body {
  position: relative;
  flex: 1 1 auto;
  min-height: 0;
}
</style>
