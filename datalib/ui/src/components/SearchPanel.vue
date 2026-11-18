<script setup lang="ts">
// The advanced search panel, under the toolbar's box. It holds nothing of its
// own: everything it shows is read from the query, and everything it does
// writes the query, the same words a person could have typed.
import { computed, onMounted, ref, watch } from "vue";
import { fetchGroups, fetchSearchKeys, SEARCH, type RowGroup, type SearchRow } from "@/api";
import {
  DEFAULT_CUTOFF,
  isOneRowPerDocument,
  pickedSource,
  semanticOf,
  setOneRowPerDocument,
  setSemantic,
  setSource,
} from "@/cards/search";
import { iconUrl } from "@/config/icons";

const props = defineProps<{
  /** What the box holds. */
  query: string;
  /** What was last run, whose results the source counts describe. */
  settled: string;
  /** Whether Enter changes the search on screen, or opens a new one. */
  onSearchCard: boolean;
  /** How this machine spells the key that always opens a new tab. */
  newTabKey: string;
}>();
const emit = defineEmits<{
  /** Run this query. */
  apply: [query: string];
  /** Start a filter on this key: the box takes it from here. */
  addKey: [key: string];
}>();

const FILTERS: { label: string; keys: string[] }[] = [
  { label: "People", keys: ["from", "to", "with", "mention"] },
  { label: "When", keys: ["after", "before"] },
  { label: "What", keys: ["kind", "label", "subject", "channel", "project"] },
];

const known = ref<Set<string> | null>(null);
onMounted(async () => {
  const keys = await fetchSearchKeys(SEARCH).catch(() => null);
  if (keys) known.value = new Set(keys.map((k) => k.key));
});
const filters = computed(() =>
  FILTERS.map((f) => ({
    ...f,
    keys: f.keys.filter((k) => known.value?.has(k) ?? true),
  })),
);

const semantic = computed(() => semanticOf(props.query));
const shownCutoff = computed(() => (semantic.value ?? DEFAULT_CUTOFF).toFixed(2));

// Offered only while they can change something: a search already narrowed
// to one source has no other to offer, and clearing it is the box's chip.
const sources = ref<RowGroup<SearchRow>[]>([]);
const offersSources = computed(() => pickedSource(props.query) === null);
let asked: AbortController | null = null;
async function loadSources() {
  asked?.abort();
  if (!offersSources.value) {
    sources.value = [];
    return;
  }
  const ctrl = (asked = new AbortController());
  try {
    const r = await fetchGroups<SearchRow>(props.settled, "source_ref", ctrl.signal);
    if (asked === ctrl) sources.value = r.groups;
  } catch {
    /* the chips are a convenience; the search says what went wrong */
  }
}
watch([() => props.settled, offersSources], loadSources, { immediate: true });
</script>

<template>
  <section class="search-panel" aria-label="Advanced search" data-testid="search-panel">
    <div class="panel-row">
      <span class="panel-label">Match</span>
      <div class="panel-body semantic">
        <label class="semantic-on">
          <input
            type="checkbox"
            data-testid="search-semantic"
            :checked="semantic !== null"
            @change="
              emit(
                'apply',
                setSemantic(
                  query,
                  ($event.target as HTMLInputElement).checked ? DEFAULT_CUTOFF : null,
                ),
              )
            "
          />
          Also by meaning
        </label>
        <label v-if="semantic !== null" class="semantic-cutoff" title="How close a match has to be">
          Closeness
          <input
            type="range"
            min="0"
            max="0.6"
            step="0.05"
            data-testid="search-cutoff"
            :value="semantic"
            @change="
              emit('apply', setSemantic(query, Number(($event.target as HTMLInputElement).value)))
            "
          />
          <output>{{ shownCutoff }}</output>
        </label>
      </div>
    </div>

    <div class="panel-row">
      <span class="panel-label">Show</span>
      <div class="panel-body">
        <label class="semantic-on">
          <input
            type="checkbox"
            data-testid="search-per-document"
            :checked="isOneRowPerDocument(query)"
            @change="
              emit(
                'apply',
                setOneRowPerDocument(query, ($event.target as HTMLInputElement).checked),
              )
            "
          />
          One row per document
        </label>
      </div>
    </div>

    <div v-if="offersSources && sources.length > 0" class="panel-row">
      <span class="panel-label">Sources</span>
      <div class="panel-body chips" role="group" aria-label="Sources">
        <button
          v-for="g in sources"
          :key="g.sample.source_id"
          type="button"
          class="chip"
          :data-source="g.sample.source_id"
          @click="emit('apply', setSource(query, g.sample.source_id ?? null))"
        >
          <img
            v-if="iconUrl(g.sample.source_ref?.icon)"
            :src="iconUrl(g.sample.source_ref?.icon)!"
            alt=""
          />
          {{ g.sample.source_ref?.label ?? g.sample.source_id }} {{ g.count.toLocaleString() }}
        </button>
      </div>
    </div>

    <div v-for="f in filters" :key="f.label" class="panel-row">
      <span class="panel-label">{{ f.label }}</span>
      <div class="panel-body chips">
        <button
          v-for="k in f.keys"
          :key="k"
          type="button"
          class="chip chip-key"
          :data-key="k"
          @click="emit('addKey', k)"
        >
          {{ k }}:
        </button>
      </div>
    </div>

    <p class="panel-hint" data-testid="search-new-tab-hint">
      <template v-if="onSearchCard">
        Enter searches in this tab. <kbd>{{ newTabKey }}</kbd> opens a new one.
      </template>
      <template v-else>Enter opens the search in a new tab.</template>
    </p>
  </section>
</template>

<style scoped>
.search-panel {
  position: absolute;
  top: calc(100% + 4px);
  right: 0;
  z-index: 1000;
  width: min(520px, calc(100vw - 24px));
  max-height: min(70vh, 520px);
  overflow-y: auto;
  box-sizing: border-box;
  padding: 10px 12px;
  display: flex;
  flex-direction: column;
  gap: 8px;
  color: var(--datalib-fg);
  background: var(--datalib-bg);
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  box-shadow: 0 6px 20px rgb(0 0 0 / 25%);
}
.panel-row {
  display: grid;
  grid-template-columns: 64px minmax(0, 1fr);
  gap: 8px;
  align-items: start;
}
.panel-label {
  color: var(--datalib-muted);
  padding-top: 3px;
}
.panel-body {
  min-width: 0;
}
.semantic {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px 16px;
}
.semantic-on,
.semantic-cutoff {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  white-space: nowrap;
}
.semantic-cutoff {
  color: var(--datalib-muted);
}
.semantic-cutoff output {
  color: var(--datalib-fg);
  font-variant-numeric: tabular-nums;
}
.chips {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
}
.chip {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 1px 8px;
  font: inherit;
  color: var(--datalib-fg);
  white-space: nowrap;
  background: transparent;
  border: 1px solid var(--datalib-border);
  border-radius: 999px;
  cursor: pointer;
}
.chip:hover {
  background: var(--datalib-hover);
}
.chip img {
  width: var(--datalib-icon-size);
  height: var(--datalib-icon-size);
}
.panel-hint {
  margin: 2px 0 0;
  color: var(--datalib-muted);
}
.panel-hint kbd {
  font: inherit;
  padding: 0 4px;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
}
.chip-key {
  font-family: var(--datalib-font-mono, ui-monospace, monospace);
}
</style>
