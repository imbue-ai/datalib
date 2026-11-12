<script setup lang="ts">
// The search as a list with a preview: each result with the words that
// were typed marked, and the picked one read in place beside the list.
// One of the Search card's two views (GridCard.ce.vue hosts it and owns
// the query); the other is the table.
import { nextTick, onBeforeUnmount, onMounted, ref, useTemplateRef, watch } from "vue";
import type { SearchResponse, SearchRow } from "@/api";
import { useApi } from "@/cards/cardApi";
import type { CardCtx, Teardown } from "./types";
import { oneAtATime, subscribeLive } from "@/live";
import { iconUrl } from "@/config/icons";
import { formatRelative } from "@/config/timeFormat";
import { documentPreview } from "./libs/documentView";
import { markWords } from "./search";
import { datesInText } from "./dates";
import { people, peopleTitle } from "./contacts";
import { handleFromUri } from "./chipLinks";
import { endWait, startWait, type Wait } from "@/telemetry";
import { formatBytes } from "@/config/bytes";
import {
  asking,
  firstWindow,
  nextFetch,
  withoutPage,
  withPage,
  type Page,
  type PagedWindow,
} from "@/grid/pagedWindow";
import { sortsBySize } from "@/grid/searchSort";
import { stepThroughList, tabLeavesRows } from "@/grid/rowIdioms";

const props = defineProps<{
  ctx: CardCtx;
  // The query to show results for: the host's, once typing has paused.
  query: string;
  // Whether this view is the one shown. A hidden list asks nothing, and
  // catches up when it is shown again.
  active: boolean;
  // The order the host asks for; null for the search's own.
  sort: string | null;
}>();
const emit = defineEmits<{
  // What the answer said beyond its rows, for the host's line above them.
  answered: [echo: SearchResponse["query_echo"]];
}>();
const api = useApi();

const PAGE = 50;

const results = ref<SearchRow[]>([]);
const total = ref(0);
const nextOffset = ref<number | null>(null);
/// The rows held, a prefix of the search's, as the table holds its own.
let win: PagedWindow<SearchRow, number> | null = null;
const notice = ref<string | null>(null);
const loading = ref(false);
const picked = ref<SearchRow | null>(null);
// The query the results on screen answer; null before the first answer.
const shownQuery = ref<string | null>(null);
// That query with the order it was answered in.
let answered: string | null = null;
const keyNow = () => `${props.sort ?? ""}\u0000${props.query}`;
const pageOf = (r: SearchResponse): Page<SearchRow, number> => ({
  rows: r.rows,
  next: r.next_offset,
  total: r.total,
  at: r.at ?? null,
});
const now = Date.now();

let inflight: AbortController | null = null;

/// The person's wait for an answer not yet on screen, as the table
/// times its own (GridCard.ce.vue).
let wait: Wait | null = startWait("open");

function report(q: string, outcome: "rows" | "refused" | "failed", more = {}) {
  if (!wait) return;
  endWait("search_answered", wait, {
    card: props.ctx.cardId,
    card_type: props.ctx.cardType,
    view: "list",
    q,
    sort: props.sort,
    outcome,
    ...more,
  });
  wait = null;
}

/// Ask for the first page, or, when the index moved under the rows on
/// screen, for as many as are held and through the picked one, so nobody
/// loses their place.
async function run(keepPick = false) {
  if (!keepPick) wait ??= startWait("search");
  inflight?.abort();
  const ctrl = new AbortController();
  inflight = ctrl;
  const q = props.query;
  const key = keyNow();
  const again = keepPick && answered === key;
  loading.value = true;
  try {
    const page: SearchResponse = await api.fetchSearch(
      q,
      again ? Math.max(PAGE, results.value.length) : PAGE,
      ctrl.signal,
      { toast: false },
      { sort: props.sort, through: again ? (picked.value?.uuid ?? null) : null },
    );
    if (ctrl.signal.aborted) return;
    const echo = page.query_echo;
    notice.value =
      page.refused?.[0] ??
      (echo?.qmd_index_missing
        ? "Nothing has been indexed for search yet, so there is nothing to match your words against. Sync a source first."
        : (echo?.qmd_error ?? null));
    // A query the search cannot read yet leaves the last results up.
    if (page.refused?.length) return report(q, "refused");
    emit("answered", echo);
    win = firstWindow(pageOf(page));
    results.value = win.rows;
    total.value = page.total;
    nextOffset.value = win.next;
    shownQuery.value = q;
    answered = key;
    if (!again) rowsEl.value?.scrollTo({ top: 0 });
    if (!keepPick || !results.value.some((r) => r.uuid === picked.value?.uuid)) {
      picked.value = results.value[0] ?? null;
    }
    report(q, "rows", { rows: page.rows.length, total: page.total });
    // The observer speaks only when the end comes or goes; a first page
    // too short to push it out of view would otherwise wait for a scroll.
    void nextTick(readOnIfNearEnd);
  } catch (e) {
    if (!ctrl.signal.aborted) {
      notice.value = (e as Error).message;
      report(q, "failed");
    }
  } finally {
    if (inflight === ctrl) loading.value = false;
  }
}

// The page on its way, so a second ask while it is out waits for it.
let moreOut: Promise<void> | null = null;
const loadingMore = ref(false);

function more(): Promise<void> {
  if (moreOut) return moreOut;
  const fetch = win && nextFetch(win, win.rows.length, PAGE);
  if (!win || !fetch) return Promise.resolve();
  win = asking(win, fetch);
  loadingMore.value = true;
  moreOut = fetchMore(fetch).finally(() => {
    moreOut = null;
    loadingMore.value = false;
    // A list shorter than its box still shows its end: read on.
    void nextTick(readOnIfNearEnd);
  });
  return moreOut;
}

async function fetchMore(fetch: { from: number; limit: number }) {
  const key = keyNow();
  try {
    const page = await api.fetchSearch(
      props.query,
      fetch.limit,
      undefined,
      {},
      { offset: fetch.from, sort: props.sort },
    );
    // A page of a search since replaced lines up with nothing on screen.
    if (key !== keyNow() || !win) return;
    const next = withPage(win, fetch.from, pageOf(page));
    if (next === "moved") return void run(true);
    win = next;
    results.value = win.rows;
    total.value = page.total;
    nextOffset.value = win.next;
  } catch (e) {
    notice.value = (e as Error).message;
    if (win) win = withoutPage(win, fetch.from);
  }
}

watch(
  () => [props.query, props.sort, props.active] as const,
  ([, , active]) => {
    if (active && keyNow() !== answered) void run();
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
  const render = documentPreview(
    row.markdown_uuid,
    row.is_document ? null : row.uuid,
    titlePeople(row),
  );
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

// The list is one stop for Tab, on the picked result; the arrows move
// the pick, and Enter opens it, as they do in the table. Tab from a
// result focuses one of these zero-size stops either side of the list,
// and the browser's own Tab goes on from there, past the chips in the
// results below it.
const beforeList = useTemplateRef<HTMLElement>("beforeList");
const afterList = useTemplateRef<HTMLElement>("afterList");

function onListKey(e: KeyboardEvent) {
  if (!(e.target as Element | null)?.closest?.(".sc-result")) return;
  const exit = tabLeavesRows(e);
  if (exit) return void (exit === "after" ? afterList : beforeList).value?.focus();
  if (e.key === "Enter" && !e.metaKey && !e.ctrlKey && !e.altKey && !e.shiftKey) {
    if (!picked.value) return;
    e.preventDefault();
    openPicked();
    return;
  }
  const at = results.value.findIndex((r) => r.uuid === picked.value?.uuid);
  const to = stepThroughList(e, at, results.value.length);
  if (to == null) return;
  e.preventDefault();
  if (e.key === "ArrowDown" && to === at && nextOffset.value != null) {
    void more().then(() => pickAt(at + 1));
    return;
  }
  pickAt(to);
}

/// Tab into the list from outside lands on the picked result, not on
/// a chip in one (a chip is a link, so the browser stops on each).
function onListFocus(e: FocusEvent) {
  const el = e.target as HTMLElement;
  const inResult = el.closest?.(".sc-result") && !el.classList.contains("sc-result");
  if (!inResult || !el.matches(":focus-visible")) return;
  const at = results.value.findIndex((r) => r.uuid === picked.value?.uuid);
  pickAt(Math.max(at, 0));
}

function pickAt(i: number) {
  const row = results.value[i];
  if (!row) return;
  picked.value = row;
  void nextTick(() => {
    const el = rowsEl.value?.querySelectorAll<HTMLElement>(".sc-result")[i];
    el?.focus();
    el?.scrollIntoView({ block: "nearest" });
  });
}

function title(row: SearchRow): string {
  return row.conversation_name || row.channel || row.kind || "Untitled";
}

// A contact's row is titled by its chip: the photo and the name your
// contacts give them. `people` answering redraws it.
const peopleAnswered = ref(0);
const stopPeople = people.subscribe(() => peopleAnswered.value++);

/// Who a row is titled by: a contact's own row by the contact, a
/// conversation named after its people by them; empty for a row titled
/// by its name.
function titlePeople(row: SearchRow): { handle: string; shownAs: string }[] {
  const contact = row.contact_ref ? handleFromUri(row.contact_ref.id) : null;
  if (contact) return [{ handle: contact, shownAs: row.contact_ref!.label }];
  return (row.subject_ref ?? []).flatMap((p) => {
    const handle = handleFromUri(p.id);
    return handle ? [{ handle, shownAs: p.label }] : [];
  });
}

function drawTitle(el: unknown, row: SearchRow) {
  const named = titlePeople(row);
  if (!(el instanceof HTMLElement) || named.length === 0) return;
  const title = peopleTitle(named);
  // The row is picked by a click anywhere on it, the chips included; a
  // chip's href is for a copy, never to be followed from here.
  title.addEventListener("click", (e) => e.preventDefault());
  el.replaceChildren(title);
}

function when(iso: string | null): string {
  if (!iso) return "";
  return formatRelative(iso, now);
}

const listEl = useTemplateRef<HTMLDivElement>("listEl");
const rowsEl = useTemplateRef<HTMLUListElement>("rowsEl");

// The next page is asked for as the end of the list nears the bottom of
// its box, a few screens early so a steady scroll rarely waits on it.
const endEl = useTemplateRef<HTMLLIElement>("endEl");
const READ_AHEAD_PX = 600;
let endObserver: IntersectionObserver | null = null;

/// Measured, not the observer's last word, which lags rows just added.
function readOnIfNearEnd() {
  const el = rowsEl.value;
  if (!el || !props.active || el.clientHeight === 0) return;
  if (el.scrollHeight - el.scrollTop - el.clientHeight < READ_AHEAD_PX) void more();
}
const refresh = oneAtATime(async () => {
  if (props.active) await run(true);
  else {
    shownQuery.value = null;
    answered = null;
  }
});
let stop: (() => void) | null = null;
onMounted(() => {
  if (props.active) void run();
  if (rowsEl.value && endEl.value && typeof IntersectionObserver !== "undefined") {
    endObserver = new IntersectionObserver(
      ([e]) => {
        if (e.isIntersecting && props.active) void more();
      },
      { root: rowsEl.value, rootMargin: `0px 0px ${READ_AHEAD_PX}px 0px` },
    );
    endObserver.observe(endEl.value);
  }
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
  endObserver?.disconnect();
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
      <span ref="beforeList" class="sc-edge" tabindex="-1" />
      <ul
        ref="rowsEl"
        class="sc-list"
        aria-label="Results"
        @keydown="onListKey"
        @focusin="onListFocus"
      >
        <li v-for="r in results" :key="r.uuid">
          <button
            class="sc-result"
            :class="{ 'is-picked': picked?.uuid === r.uuid }"
            :tabindex="(picked ?? results[0])?.uuid === r.uuid ? 0 : -1"
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
                  v-if="titlePeople(r).length"
                  :ref="(el) => drawTitle(el, r)"
                  class="sc-title"
                  :data-answered="peopleAnswered"
                />
                <strong v-else class="sc-title">{{ title(r) }}</strong>
                <span class="sc-when">{{ when(r.touched_at) }}</span>
              </span>
              <span class="sc-snippet"
                ><template
                  v-for="(p, i) in markWords(datesInText(r.snippet), shownQuery ?? '')"
                  :key="i"
                  ><mark v-if="p.hit">{{ p.text }}</mark
                  ><template v-else>{{ p.text }}</template></template
                ></span
              >
              <span class="sc-meta">
                {{ r.source_ref?.label ?? r.source
                }}<template v-if="r.author"> · {{ r.author }}</template
                ><template v-if="sortsBySize(sort) && r.byte_size != null">
                  · {{ formatBytes(r.byte_size) }}</template
                >
              </span>
            </span>
          </button>
        </li>
        <li ref="endEl" class="sc-more" data-testid="search-list-end">
          <template v-if="nextOffset != null && loadingMore">Loading more…</template>
        </li>
        <li v-if="!loading && results.length === 0 && !notice" class="sc-empty">
          Nothing matches. Try fewer words.
        </li>
      </ul>
      <span ref="afterList" class="sc-edge" tabindex="-1" />
      <section class="sc-preview" aria-label="Preview">
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
.sc-edge {
  width: 0;
  height: 0;
  overflow: hidden;
  outline: none;
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
.sc-more:empty {
  padding: 0;
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
.sc-preview-body {
  position: relative;
  flex: 1 1 auto;
  min-height: 0;
}
</style>
