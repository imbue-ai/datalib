<script setup lang="ts">
// The bar across the top of a document: its source's mark and its
// title, which stay put while the document scrolls beneath, then where
// it came from, its ID, and a ⋯ menu of everything else
// (`documentMenu.ts`). It draws; the card does what is picked.
import { computed, onBeforeUnmount, ref, watchEffect } from "vue";
import type { ChatResponse } from "@/api";
import { iconUrl } from "@/config/icons";
import ChipMenu from "./ChipMenu.ce.vue";
import { people as resolver, peopleTitle } from "./contacts";
import { documentMenu, documentTitle, outlinkOf, type DocumentAction } from "./documentMenu";

const props = defineProps<{
  doc: ChatResponse;
  preview: boolean;
  revealLabel: string | null;
  // Who the document is titled by, drawn as their chips: a contact's
  // page, a conversation named after its people. Empty: its name.
  people: { handle: string; shownAs: string }[];
}>();
const emit = defineEmits<{ act: [id: DocumentAction] }>();

const host = computed(() => ({ preview: props.preview, revealLabel: props.revealLabel }));
const title = computed(() => documentTitle(props.doc));
const tile = computed(() => iconUrl(props.doc.source_ref?.icon));
const link = computed(() => outlinkOf(props.doc, host.value));
const entries = computed(() => documentMenu(props.doc, host.value));

const titleEl = ref<HTMLElement | null>(null);
const answered = ref(0);
const stopPeople = resolver.subscribe(() => answered.value++);
onBeforeUnmount(stopPeople);
watchEffect(() => {
  void answered.value;
  const el = titleEl.value;
  if (!el || props.people.length === 0) return;
  const chips = peopleTitle(props.people);
  // A chip's href is for a copy, never to be followed from the bar.
  chips.addEventListener("click", (e) => e.preventDefault());
  el.replaceChildren(chips);
});

/// Where the menu is open: under the ⋯, its right edge on the button's,
/// or at a right-click on the bar.
const menuAt = ref<{ x: number; y: number; alignRight: boolean } | null>(null);

function openUnder(ev: MouseEvent) {
  if (menuAt.value) {
    menuAt.value = null;
    return;
  }
  const r = (ev.currentTarget as HTMLElement).getBoundingClientRect();
  menuAt.value = { x: r.right, y: r.bottom + 2, alignRight: true };
}

function openAt(ev: MouseEvent) {
  menuAt.value = { x: ev.clientX, y: ev.clientY, alignRight: false };
}

function pick(id: string) {
  menuAt.value = null;
  emit("act", id as DocumentAction);
}
</script>

<template>
  <header class="doc-bar" @contextmenu.prevent.stop="openAt">
    <img v-if="tile" class="doc-bar-tile" :src="tile" alt="" />
    <strong v-if="people.length" ref="titleEl" class="doc-bar-title" />
    <strong v-else class="doc-bar-title" :title="title">{{ title }}</strong>
    <button
      v-if="preview"
      type="button"
      class="doc-bar-btn"
      aria-label="Open as card"
      title="Open as card"
      @click="emit('act', 'open-card')"
    >
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <rect x="3" y="4" width="18" height="16" rx="2" />
        <path d="M14 4v16M7 10l2 2-2 2" />
      </svg>
    </button>
    <a
      v-if="link?.id === 'open-url'"
      class="doc-bar-btn"
      :href="link.target"
      target="_blank"
      rel="noopener"
      :aria-label="link.label"
      :title="link.label"
      @click.prevent="emit('act', 'open-url')"
    >
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <path d="M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5" />
      </svg>
    </a>
    <button
      v-else-if="link"
      type="button"
      class="doc-bar-btn"
      :aria-label="link.label"
      :title="`${link.label} (${link.target})`"
      @click="emit('act', link.id)"
    >
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
      </svg>
    </button>
    <button
      type="button"
      class="doc-bar-btn"
      aria-label="Copy ID"
      :title="`Copy ID (${doc.markdown_uuid})`"
      @click="emit('act', 'copy-id')"
    >
      <span class="doc-bar-id">ID</span>
    </button>
    <button
      type="button"
      class="doc-bar-btn"
      :class="{ 'is-open': menuAt }"
      aria-haspopup="menu"
      :aria-expanded="!!menuAt"
      aria-label="More for this document"
      title="More"
      @click="openUnder"
    >
      <svg viewBox="0 0 24 24" aria-hidden="true" class="doc-bar-dots">
        <circle cx="5" cy="12" r="1.9" />
        <circle cx="12" cy="12" r="1.9" />
        <circle cx="19" cy="12" r="1.9" />
      </svg>
    </button>
    <ChipMenu
      v-if="menuAt"
      :entries="entries"
      :x="menuAt.x"
      :y="menuAt.y"
      :align-right="menuAt.alignRight"
      @pick="pick"
      @close="menuAt = null"
    />
  </header>
</template>

<style>
/* Room on the right for whatever the layout floats over the bar's end
   (`--datalib-chrome-inset`, set by ContainerNode on a card that draws
   its own title bar). */
.doc-bar {
  flex: none;
  display: flex;
  align-items: center;
  gap: 4px;
  min-height: 32px;
  box-sizing: border-box;
  padding: 0 calc(4px + var(--datalib-chrome-inset, 0px)) 0 10px;
  border-bottom: 1px solid var(--datalib-border-soft);
  background: var(--datalib-bg);
  font-size: var(--datalib-font-size);
}
.doc-bar-tile {
  flex: none;
  width: 16px;
  height: 16px;
  margin-right: 4px;
  object-fit: contain;
}
.doc-bar-title {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: calc(var(--datalib-font-size) + 1px);
  font-weight: 600;
}
.doc-bar-btn {
  flex: none;
  width: 24px;
  height: 24px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  padding: 0;
  border: none;
  border-radius: 5px;
  background: transparent;
  color: var(--datalib-muted);
  font: inherit;
  cursor: pointer;
}
.doc-bar-btn:hover,
.doc-bar-btn.is-open {
  background: var(--datalib-hover);
  color: var(--datalib-fg);
}
.doc-bar-btn svg {
  width: 14px;
  height: 14px;
  fill: none;
  stroke: currentColor;
  stroke-width: 2;
  stroke-linecap: round;
  stroke-linejoin: round;
}
.doc-bar-btn svg.doc-bar-dots {
  width: 15px;
  height: 15px;
  fill: currentColor;
  stroke: none;
}
.doc-bar-id {
  font-size: 9px;
  font-weight: 700;
  line-height: 1;
  padding: 2px 3px;
  border: 1.5px solid currentColor;
  border-radius: 3px;
}
</style>
