<script setup lang="ts">
// The Search card: a query, the sources it hits with how many each, the
// results as a list with the words you typed marked, and the picked
// result read in place. The grid (`gridView`) is the same search as a
// table with every column; "View as table" opens it on this query.
import { computed, onBeforeUnmount, onMounted, ref, useTemplateRef, watch } from "vue";
import type { GroupsResponse, SearchResponse, SearchRow } from "@/api";
import { useApi } from "@/cards/cardApi";
import type { CardCtx, Teardown } from "./types";
import { oneAtATime, subscribeLive } from "@/live";
import { iconUrl } from "@/config/icons";
import { formatRelative } from "@/config/timeFormat";
import { documentView } from "./libs/documentView";
import {
  decodeSearchState,
  encodeSearchState,
  markWords,
  searchQuery,
  type SearchInput,
} from "./search";

const props = defineProps<{ ctx: CardCtx; q: string }>();
const api = useApi();

props.ctx.setHelp(`
<p>Type what you are looking for and press Enter. Plain words are matched by the words
they contain and by what they mean; tick <b>Meaning only</b> to match on meaning
alone. Filters work here as in the grid: <code>author:worf</code>,
<code>before:2371-01-01</code>, <code>-kind:contact</code>.</p>
<p>The chips under the box say which sources the results come from, and how many each;
pick one to see only its results. Pick a result to read it on the right; <b>Open as
card</b> puts it beside this one. <b>View as table</b> opens the same search in the grid,
with every column, sorting and grouping.</p>
`);

const input = ref<SearchInput>(decodeSearchState(props.ctx.initialState, props.q));
const typed = ref(input.value.text);

const results = ref<SearchRow[]>([]);
const total = ref(0);
const nextOffset = ref<number | null>(null);
const sources = ref<GroupsResponse["groups"]>([]);
const notice = ref<string | null>(null);
const loading = ref(false);
const picked = ref<SearchRow | null>(null);

const query = computed(() => searchQuery(input.value));
const now = Date.now();

watch(
  input,
  (i) => {
    props.ctx.host.setState(encodeSearchState(i));
    props.ctx.setTitle(i.text ? `Search: ${i.text}` : "Unified Search (new)");
  },
  { immediate: true, deep: true },
);

let inflight: AbortController | null = null;

async function run(keepPick = false) {
  inflight?.abort();
  const ctrl = new AbortController();
  inflight = ctrl;
  loading.value = true;
  try {
    const [page, groups]: [SearchResponse, GroupsResponse] = await Promise.all([
      api.fetchSearch(query.value, 50, ctrl.signal),
      api.fetchGroups(searchQuery(input.value, false), "source_ref", ctrl.signal),
    ]);
    if (ctrl.signal.aborted) return;
    results.value = page.rows;
    total.value = page.total;
    nextOffset.value = page.next_offset;
    sources.value = groups.groups;
    const echo = page.query_echo;
    notice.value =
      page.refused?.[0] ??
      (echo?.qmd_index_missing
        ? "Nothing has been indexed for search yet, so there is nothing to match your words against. Sync a source first."
        : (echo?.qmd_error ?? null));
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
      query.value,
      50,
      undefined,
      {},
      { offset: nextOffset.value },
    );
    results.value = [...results.value, ...page.rows];
    nextOffset.value = page.next_offset;
  } catch (e) {
    notice.value = (e as Error).message;
  }
}

function submit() {
  input.value = { ...input.value, text: typed.value.trim() };
}

function pickSource(id: string | null) {
  input.value = { ...input.value, sourceId: id };
}

watch(query, () => void run());

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

function viewAsTable() {
  props.ctx.host.openCards(`gridView(${JSON.stringify({ q: query.value })})`);
}

function title(row: SearchRow): string {
  return row.conversation_name || row.channel || row.kind || "Untitled";
}

function when(iso: string | null): string {
  if (!iso) return "";
  return formatRelative(iso, now);
}

const allCount = computed(() => sources.value.reduce((n, g) => n + g.count, 0));

const cardEl = useTemplateRef<HTMLDivElement>("cardEl");
const refresh = oneAtATime(() => run(true));
let stop: (() => void) | null = null;
onMounted(() => {
  void run();
  stop = subscribeLive(
    {
      root: (e) => {
        if (e.kind === "index_changed") refresh();
      },
      resync: refresh,
    },
    { onScreen: cardEl.value ?? undefined },
  );
});
onBeforeUnmount(() => {
  stop?.();
  inflight?.abort();
  teardown?.();
});
</script>

<template>
  <div ref="cardEl" class="sc">
    <form class="sc-bar" role="search" @submit.prevent="submit">
      <div class="sc-field">
        <svg class="sc-glyph" viewBox="0 0 24 24" aria-hidden="true">
          <path
            fill="currentColor"
            d="M15.5 14h-.79l-.28-.27C15.41 12.59 16 11.11 16 9.5 16 5.91 13.09 3 9.5 3S3 5.91 3 9.5 5.91 16 9.5 16c1.61 0 3.09-.59 4.23-1.57l.27.28v.79l5 4.99L20.49 19l-4.99-5zm-6 0C7.01 14 5 11.99 5 9.5S7.01 5 9.5 5 14 7.01 14 9.5 11.99 14 9.5 14z"
          />
        </svg>
        <input
          v-model="typed"
          type="search"
          aria-label="Search"
          placeholder="Search your data — words, or what they mean"
        />
        <span class="sc-count" aria-live="polite">
          {{ loading ? "Searching…" : `${total.toLocaleString()} results` }}
        </span>
      </div>
      <label class="sc-check">
        <input
          type="checkbox"
          :checked="input.meaningOnly"
          @change="input = { ...input, meaningOnly: ($event.target as HTMLInputElement).checked }"
        />
        Meaning only
      </label>
      <button type="button" class="sc-link" @click="viewAsTable">View as table</button>
    </form>

    <div class="sc-chips" role="group" aria-label="Sources">
      <button
        class="sc-chip"
        :class="{ 'is-on': !input.sourceId }"
        :aria-pressed="!input.sourceId"
        @click="pickSource(null)"
      >
        All {{ allCount.toLocaleString() }}
      </button>
      <button
        v-for="g in sources"
        :key="g.sample.source_id"
        class="sc-chip"
        :class="{ 'is-on': input.sourceId === g.sample.source_id }"
        :aria-pressed="input.sourceId === g.sample.source_id"
        @click="pickSource(g.sample.source_id)"
      >
        <img
          v-if="iconUrl(g.sample.source_ref?.icon)"
          :src="iconUrl(g.sample.source_ref?.icon)!"
          alt=""
        />
        {{ g.sample.source_ref?.label ?? g.sample.source_id }} {{ g.count.toLocaleString() }}
      </button>
    </div>

    <p v-if="notice" class="sc-notice">{{ notice }}</p>

    <div class="sc-main">
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
                <strong class="sc-title">{{ title(r) }}</strong>
                <span class="sc-when">{{ when(r.touched_at) }}</span>
              </span>
              <span class="sc-snippet"
                ><template v-for="(p, i) in markWords(r.snippet, input.text)" :key="i"
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
          Nothing matches. Try fewer words, or untick Meaning only.
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
          <strong class="sc-title">{{ title(picked) }}</strong>
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
  height: 100%;
  display: flex;
  flex-direction: column;
  font-size: var(--datalib-font-size);
  color: var(--datalib-fg);
  background: var(--datalib-bg);
}
.sc-bar {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: 12px;
  padding: var(--datalib-pad) var(--datalib-pad) 6px;
}
.sc-field {
  flex: 1 1 auto;
  display: flex;
  align-items: center;
  gap: 8px;
  height: calc(var(--datalib-control-h) + 8px);
  padding: 0 10px;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-input-bg);
}
.sc-field:focus-within {
  border-color: var(--datalib-accent);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--datalib-accent) 18%, transparent);
}
.sc-glyph {
  flex: 0 0 auto;
  width: var(--datalib-icon-size);
  height: var(--datalib-icon-size);
  color: var(--datalib-faint);
}
.sc-field input {
  flex: 1 1 auto;
  min-width: 0;
  border: 0;
  outline: none;
  background: transparent;
  color: var(--datalib-fg);
  font: inherit;
  font-size: var(--datalib-title-size);
}
.sc-count {
  flex: 0 0 auto;
  color: var(--datalib-faint);
  white-space: nowrap;
}
.sc-check {
  display: flex;
  align-items: center;
  gap: 6px;
  white-space: nowrap;
  color: var(--datalib-muted);
}
.sc-check input {
  margin: 0;
  accent-color: var(--datalib-accent);
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
.sc-chips {
  flex: 0 0 auto;
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  padding: 0 var(--datalib-pad) var(--datalib-pad);
  border-bottom: 1px solid var(--datalib-border-soft);
}
.sc-chip {
  display: flex;
  align-items: center;
  gap: 6px;
  height: calc(var(--datalib-control-h) - 2px);
  padding: 0 10px;
  font: inherit;
  border: 1px solid var(--datalib-border);
  border-radius: 999px;
  background: var(--datalib-bg);
  color: var(--datalib-fg);
  cursor: pointer;
}
.sc-chip img {
  width: var(--datalib-icon-size);
  height: var(--datalib-icon-size);
}
.sc-chip:hover {
  background: var(--datalib-hover);
}
.sc-chip.is-on {
  border-color: var(--datalib-fg);
  background: var(--datalib-fg);
  color: var(--datalib-bg);
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
