<script setup lang="ts">
// The search as a list with a preview: each result with the words that
// were typed marked, and the picked one read in place beside the list.
// One of the Search card's two views (GridCard.ce.vue hosts it and owns
// the query); the other is the table.
import { onBeforeUnmount, onMounted, ref, useTemplateRef, watch } from "vue";
import type { SearchResponse, SearchRow, SearchTab } from "@/api";
import { useApi } from "@/cards/cardApi";
import type { CardCtx, Teardown } from "./types";
import { oneAtATime, subscribeLive } from "@/live";
import { iconUrl } from "@/config/icons";
import { formatRelative } from "@/config/timeFormat";
import { documentView } from "./libs/documentView";
import { markWords } from "./search";
import { chipCell, people } from "./contacts";
import { handleFromUri } from "./chipLinks";

const props = defineProps<{
  ctx: CardCtx;
  // The query to show results for: the host's, once typing has paused.
  query: string;
  // Whether this view is the one shown. A hidden list asks nothing, and
  // catches up when it is shown again.
  active: boolean;
  // The answer to free text the host shows; null for a search without
  // free text. While `waiting`, no tab has opened yet, and the list asks
  // nothing rather than show an answer that would then be replaced.
  tab: SearchTab | null;
  waiting: boolean;
}>();
const api = useApi();

const PAGE = 50;

const results = ref<SearchRow[]>([]);
const total = ref(0);
const nextOffset = ref<number | null>(null);
const notice = ref<string | null>(null);
const loading = ref(false);
const picked = ref<SearchRow | null>(null);
// The query the results on screen answer; null before the first answer.
const shownQuery = ref<string | null>(null);
// That query with the tab it was answered in.
let answered: string | null = null;
const keyNow = () => `${props.tab ?? ""}\u0000${props.query}`;
const now = Date.now();

let inflight: AbortController | null = null;

async function run(keepPick = false) {
  inflight?.abort();
  const ctrl = new AbortController();
  inflight = ctrl;
  const q = props.query;
  const key = keyNow();
  loading.value = true;
  try {
    const page: SearchResponse = await api.fetchSearch(
      q,
      PAGE,
      ctrl.signal,
      { toast: false },
      { tab: props.tab },
    );
    if (ctrl.signal.aborted) return;
    const echo = page.query_echo;
    notice.value =
      page.refused?.[0] ??
      (echo?.qmd_index_missing
        ? "Nothing has been indexed for search yet, so there is nothing to match your words against. Sync a source first."
        : (echo?.qmd_error ?? null));
    // A query the search cannot read yet leaves the last results up.
    if (page.refused?.length) return;
    results.value = page.rows;
    total.value = page.total;
    nextOffset.value = page.next_offset;
    shownQuery.value = q;
    answered = key;
    if (!keepPick || !results.value.some((r) => r.uuid === picked.value?.uuid)) {
      picked.value = results.value[0] ?? null;
    }
  } catch (e) {
    if (!ctrl.signal.aborted) notice.value = (e as Error).message;
  } finally {
    if (inflight === ctrl) loading.value = false;
  }
}

async function more() {
  if (nextOffset.value == null) return;
  try {
    const page = await api.fetchSearch(
      props.query,
      PAGE,
      undefined,
      {},
      { offset: nextOffset.value, tab: props.tab },
    );
    results.value = [...results.value, ...page.rows];
    nextOffset.value = page.next_offset;
  } catch (e) {
    notice.value = (e as Error).message;
  }
}

watch(
  () => [props.query, props.tab, props.waiting, props.active] as const,
  ([, , waiting, active]) => {
    if (active && !waiting && keyNow() !== answered) void run();
  },
);

// ── The preview: the picked result's document, drawn by the document
// card itself inside a shadow root of its own.
const previewEl = useTemplateRef<HTMLDivElement>("previewEl");
let previewRoot: ShadowRoot | null = null;
let teardown: Teardown | null = null;

function docSource(row: SearchRow): string | null {
  if (!row.markdown_uuid) return null;
  const args = [row.markdown_uuid, row.is_document ? null : row.uuid];
  return `documentView(${args.map((a) => JSON.stringify(a)).join(", ")})`;
}

function showPreview(row: SearchRow | null) {
  teardown?.();
  teardown = null;
  if (!previewEl.value) return;
  previewRoot ??= previewEl.value.attachShadow({ mode: "open" });
  previewRoot.replaceChildren();
  if (!row?.markdown_uuid) return;
  const render = documentView(row.markdown_uuid, row.is_document ? null : row.uuid);
  teardown = render(previewRoot, {
    cardId: `${props.ctx.cardId}:preview`,
    cardType: "documentView",
    initialState: "",
    setTitle: () => {},
    setHelp: () => {},
    bus: props.ctx.bus,
    host: { ...props.ctx.host, setState: () => {}, setSource: () => {}, close: () => {} },
  });
}
watch(picked, (row) => showPreview(row), { flush: "post" });

function openPicked() {
  const src = picked.value && docSource(picked.value);
  if (src) props.ctx.host.openCards(src);
}

function title(row: SearchRow): string {
  return row.conversation_name || row.channel || row.kind || "Untitled";
}

// A contact's row is titled by its chip: the photo and the name your
// contacts give them. `people` answering redraws it.
const peopleAnswered = ref(0);
const stopPeople = people.subscribe(() => peopleAnswered.value++);

function contactHandle(row: SearchRow): string | null {
  return row.contact_ref ? handleFromUri(row.contact_ref.id) : null;
}

function drawContact(el: unknown, row: SearchRow) {
  const handle = contactHandle(row);
  if (!(el instanceof HTMLElement) || !handle) return;
  const chip = chipCell(handle, row.contact_ref!.label, people.lookup(handle), false);
  // The row is picked by a click anywhere on it, the chip included; the
  // chip's href is for a copy, never to be followed from here.
  chip.addEventListener("click", (e) => e.preventDefault());
  el.replaceChildren(chip);
}

function when(iso: string | null): string {
  if (!iso) return "";
  return formatRelative(iso, now);
}

const listEl = useTemplateRef<HTMLDivElement>("listEl");
const refresh = oneAtATime(async () => {
  if (props.active && !props.waiting) await run(true);
  else {
    shownQuery.value = null;
    answered = null;
  }
});
let stop: (() => void) | null = null;
onMounted(() => {
  if (props.active && !props.waiting) void run();
  stop = subscribeLive(
    {
      root: (e) => {
        if (e.kind === "index_changed") refresh();
      },
      resync: refresh,
    },
    { onScreen: listEl.value ?? undefined },
  );
});
onBeforeUnmount(() => {
  stop?.();
  stopPeople();
  inflight?.abort();
  teardown?.();
});
</script>

<template>
  <div ref="listEl" class="sc">
    <p class="sc-count" aria-live="polite" data-testid="search-list-count">
      {{ loading ? "Searching…" : `${total.toLocaleString()} results` }}
    </p>
    <p v-if="notice" class="sc-notice">{{ notice }}</p>

    <div class="sc-main" :data-shown-query="shownQuery">
      <ul class="sc-list" aria-label="Results">
        <li v-for="r in results" :key="r.uuid">
          <button
            class="sc-result"
            :class="{ 'is-picked': picked?.uuid === r.uuid }"
            @click="picked = r"
            @dblclick="openPicked"
          >
            <img
              v-if="iconUrl(r.source_ref?.icon)"
              class="sc-tile"
              :src="iconUrl(r.source_ref?.icon)!"
              alt=""
            />
            <span class="sc-text">
              <span class="sc-line">
                <strong
                  v-if="contactHandle(r)"
                  :ref="(el) => drawContact(el, r)"
                  class="sc-title"
                  :data-answered="peopleAnswered"
                />
                <strong v-else class="sc-title">{{ title(r) }}</strong>
                <span class="sc-when">{{ when(r.touched_at) }}</span>
              </span>
              <span class="sc-snippet"
                ><template v-for="(p, i) in markWords(r.snippet, shownQuery ?? '')" :key="i"
                  ><mark v-if="p.hit">{{ p.text }}</mark
                  ><template v-else>{{ p.text }}</template></template
                ></span
              >
              <span class="sc-meta">
                {{ r.source_ref?.label ?? r.source
                }}<template v-if="r.author"> · {{ r.author }}</template>
              </span>
            </span>
          </button>
        </li>
        <li v-if="nextOffset != null" class="sc-more">
          <button class="sc-link" @click="more">Show more</button>
        </li>
        <li v-if="!loading && results.length === 0 && !notice" class="sc-empty">
          Nothing matches. Try fewer words, or another tab.
        </li>
      </ul>
      <section class="sc-preview" aria-label="Preview">
        <header v-if="picked" class="sc-preview-head">
          <img
            v-if="iconUrl(picked.source_ref?.icon)"
            class="sc-tile"
            :src="iconUrl(picked.source_ref?.icon)!"
            alt=""
          />
          <strong
            v-if="contactHandle(picked)"
            :ref="(el) => drawContact(el, picked!)"
            class="sc-title"
            :data-answered="peopleAnswered"
          />
          <strong v-else class="sc-title">{{ title(picked) }}</strong>
          <button class="sc-link" :disabled="!picked.markdown_uuid" @click="openPicked">
            Open as card
          </button>
          <a
            v-if="picked.source_url"
            class="sc-link"
            :href="picked.source_url"
            target="_blank"
            rel="noopener"
            >Open at the source</a
          >
        </header>
        <p v-if="!picked" class="sc-empty">Pick a result to read it here.</p>
        <div ref="previewEl" class="sc-preview-body" />
      </section>
    </div>
  </div>
</template>

<style scoped>
.sc {
  flex: 1 1 auto;
  min-height: 0;
  display: flex;
  flex-direction: column;
  font-size: var(--datalib-font-size);
  color: var(--datalib-fg);
  border: 1px solid var(--datalib-border-soft);
  border-radius: 4px;
}
.sc-count {
  flex: 0 0 auto;
  margin: 0;
  padding: 4px var(--datalib-pad);
  color: var(--datalib-muted);
  border-bottom: 1px solid var(--datalib-border-soft);
}
.sc-link {
  padding: 0;
  border: 0;
  background: none;
  font: inherit;
  font-weight: 600;
  color: var(--datalib-accent);
  cursor: pointer;
  white-space: nowrap;
  text-decoration: none;
}
.sc-link:hover:not(:disabled) {
  text-decoration: underline;
}
.sc-link:disabled {
  opacity: 0.4;
  cursor: default;
}
.sc-notice {
  flex: 0 0 auto;
  margin: 0;
  padding: 6px var(--datalib-pad);
  background: var(--datalib-warn-bg);
  color: var(--datalib-warn-fg);
  border-bottom: 1px solid var(--datalib-warn-border);
}
.sc-main {
  flex: 1 1 auto;
  min-height: 0;
  display: flex;
}
.sc-list {
  flex: 0 0 min(440px, 42%);
  min-width: 0;
  margin: 0;
  padding: 0;
  list-style: none;
  overflow-y: auto;
  border-right: 1px solid var(--datalib-border-soft);
}
.sc-result {
  display: flex;
  gap: 10px;
  width: 100%;
  padding: 8px var(--datalib-pad);
  font: inherit;
  text-align: left;
  color: inherit;
  background: none;
  border: 0;
  border-left: 3px solid transparent;
  border-bottom: 1px solid var(--datalib-border-soft);
  cursor: pointer;
}
.sc-result:hover {
  background: var(--datalib-hover);
}
.sc-result.is-picked {
  background: color-mix(in srgb, var(--datalib-accent) 10%, var(--datalib-bg));
  border-left-color: var(--datalib-accent);
}
.sc-tile {
  flex: 0 0 auto;
  width: calc(var(--datalib-icon-size) + 4px);
  height: calc(var(--datalib-icon-size) + 4px);
}
.sc-text {
  flex: 1 1 auto;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.sc-line {
  display: flex;
  gap: 8px;
  align-items: baseline;
}
.sc-title {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.sc-when,
.sc-meta {
  flex: 0 0 auto;
  color: var(--datalib-faint);
  font-size: var(--datalib-font-size-small);
}
.sc-snippet {
  color: var(--datalib-muted);
  line-height: 1.4;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
}
.sc-snippet mark {
  background: color-mix(in srgb, var(--datalib-warn-dot) 35%, transparent);
  color: inherit;
  border-radius: 3px;
  padding: 0 1px;
}
.sc-more,
.sc-empty {
  padding: 10px var(--datalib-pad);
  color: var(--datalib-muted);
}
.sc-preview {
  flex: 1 1 auto;
  min-width: 0;
  display: flex;
  flex-direction: column;
}
.sc-preview-head {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 6px var(--datalib-pad);
  border-bottom: 1px solid var(--datalib-border-soft);
}
.sc-preview-body {
  position: relative;
  flex: 1 1 auto;
  min-height: 0;
}
</style>
