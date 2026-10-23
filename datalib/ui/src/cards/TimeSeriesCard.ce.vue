<script setup lang="ts">
// `timeSeriesView()`: every time series in the time-series index, picked
// from a list and drawn together on one chart, one axis per quantity and
// two axes at most. Read from the `unified_time_series_index` applet; the
// rules are timeSeries.ts.
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { subscribeLive } from "@/live";
import {
  RANGES,
  decodeState,
  encodeState,
  grouped,
  newestMs,
  rangeBounds,
  refusal,
  toMs,
  unitText,
  type Answered,
  type Listed,
  type RangeKey,
} from "./timeSeries";
import { fetchPoints, fetchTimeSeries } from "./timeSeriesApi";
import { seriesColor } from "./plot/plot";
import TimeSeriesChart from "./TimeSeriesChart.vue";
import EntityChip from "./EntityChip.ce.vue";
import { uriFromEntity } from "./chipLinks";
import type { CardCtx } from "./types";

const props = defineProps<{ ctx: CardCtx }>();
props.ctx.setTitle("Time series");
props.ctx.setHelp(`
<p>Every time series your sources keep — a Garmin metric by day, a
sensor's readings — from the time-series index. Tick time series in the
list to draw them on one chart. Time series of one quantity share an
axis, converted to one unit — minutes and hours, kilograms and pounds; a
second quantity gets the right-hand axis, and a chart holds two at
most. A time series of text is listed but not drawn yet.</p>
<p>Past a few hundred points a line shows each stretch's mean, with the
shaded band from its lowest to its highest value; a shorter range shows
every point. Hover the chart to read each line's value at that time.</p>
<p>Click a line in the legend to turn it off or on; double-click it to
show it alone, and again to show them all. Drag across the chart to zoom
in, which reads that stretch again in finer detail; double-click the
chart to go back.</p>
`);

const restored = decodeState(props.ctx.initialState);
const all = ref<Listed[]>([]);
const ids = ref<string[]>(restored.ids);
const range = ref<RangeKey>(restored.range);
const filter = ref("");
const answered = ref<Map<string, Answered>>(new Map());
const error = ref<string | null>(null);
const notice = ref<string | null>(null);
const loaded = ref(false);

const cardEl = ref<HTMLElement | null>(null);
const chartWidth = ref(600);

const byId = computed(() => new Map(all.value.map((s) => [s.time_series_id, s])));
const picked = computed(() =>
  ids.value.flatMap((id) => {
    const s = byId.value.get(id);
    return s ? [s] : [];
  }),
);
const groups = computed(() => grouped(all.value, filter.value));
const colorOf = (id: string) => seriesColor(Math.max(0, ids.value.indexOf(id)));

function persist() {
  props.ctx.host.setState(encodeState(ids.value, range.value));
}

function toggle(s: Listed) {
  notice.value = null;
  if (ids.value.includes(s.time_series_id)) {
    ids.value = ids.value.filter((id) => id !== s.time_series_id);
  } else {
    const why = refusal(picked.value, s);
    if (why) {
      notice.value = why;
      return;
    }
    ids.value = [...ids.value, s.time_series_id];
  }
  persist();
}

function pickRange(key: RangeKey) {
  range.value = key;
  persist();
}

// ── Loading ────────────────────────────────────────────────────────

let listAsked = 0;
async function loadList() {
  const mine = ++listAsked;
  try {
    const listed = await fetchTimeSeries();
    if (mine !== listAsked) return;
    all.value = listed;
    error.value = null;
  } catch (e) {
    if (mine === listAsked) error.value = (e as Error).message;
  } finally {
    if (mine === listAsked) loaded.value = true;
  }
}

/// What the chart spans, in milliseconds: the range ending at the newest
/// point picked, else everything the picked time series hold.
const span = computed<{ fromMs: number; toMs: number } | null>(() => {
  const newest = newestMs(picked.value);
  if (newest === null) return null;
  const bounded = rangeBounds(range.value, newest);
  if (bounded) return bounded;
  const firsts = picked.value.flatMap((p) =>
    p.first_t === null ? [] : [toMs(p.time_kind, p.first_t)],
  );
  return { fromMs: Math.min(...firsts), toMs: newest };
});

let pointsAsked = 0;
async function loadPoints() {
  const mine = ++pointsAsked;
  const s = span.value;
  if (!s || !picked.value.length) {
    answered.value = new Map();
    return;
  }
  try {
    const got = await fetchPoints(picked.value, s, Math.max(50, Math.round(chartWidth.value)));
    if (mine !== pointsAsked) return;
    answered.value = got.answered;
    error.value = got.unreadable.length
      ? got.unreadable
          .map(
            (u) =>
              `${byId.value.get(u.time_series_id)?.human_name ?? u.time_series_id}: ${u.error}`,
          )
          .join("\n")
      : null;
  } catch (e) {
    if (mine === pointsAsked) error.value = (e as Error).message;
  }
}

let pointsTimer: ReturnType<typeof setTimeout> | null = null;
function schedulePoints() {
  if (pointsTimer) clearTimeout(pointsTimer);
  pointsTimer = setTimeout(() => void loadPoints(), 120);
}
watch([ids, range, () => Math.round(chartWidth.value / 50), all], schedulePoints);

// ── Lifecycle ──────────────────────────────────────────────────────

let stopLive: (() => void) | null = null;
onMounted(() => {
  void loadList();
  stopLive = subscribeLive(
    {
      root: (e) => {
        if (e.kind === "time_series_index_changed") void loadList();
      },
      resync: () => void loadList(),
    },
    { onScreen: cardEl.value ?? undefined },
  );
});
onBeforeUnmount(() => {
  stopLive?.();
  if (pointsTimer) clearTimeout(pointsTimer);
});
</script>

<template>
  <div ref="cardEl" class="ts-card">
    <div class="ts-list">
      <div class="ts-filter">
        <input
          v-model="filter"
          class="ts-input"
          type="search"
          placeholder="Find a time series"
          aria-label="Find a time series"
        />
      </div>
      <div class="ts-groups">
        <div v-if="loaded && !all.length" class="ts-empty">
          No time series yet. A Garmin source writes them on its next sync.
        </div>
        <div v-else-if="loaded && !groups.length" class="ts-empty">
          Nothing matches “{{ filter }}”.
        </div>
        <section v-for="g in groups" :key="g.source" class="ts-group">
          <h3 class="ts-source">
            <EntityChip
              :uri="uriFromEntity('group', g.source)"
              :shown="g.source"
              quiet
              :open-card="(source) => props.ctx.host.openCards(source)"
            />
            · {{ g.items.length + g.abouts.reduce((n, a) => n + a.items.length, 0) }}
          </h3>
          <label
            v-for="s in g.items"
            :key="s.time_series_id"
            class="ts-item"
            :class="{ 'ts-picked': ids.includes(s.time_series_id) }"
          >
            <input type="checkbox" :checked="ids.includes(s.time_series_id)" @change="toggle(s)" />
            <span
              class="ts-swatch"
              :style="{
                background: ids.includes(s.time_series_id)
                  ? colorOf(s.time_series_id)
                  : 'transparent',
              }"
            ></span>
            <span class="ts-label">{{ s.human_name }}</span>
            <span class="ts-meta">{{ unitText(s) }} · {{ s.point_count }}</span>
          </label>
          <details
            v-for="a in g.abouts"
            :key="a.uuid"
            class="ts-about"
            :open="filter.trim() !== '' || a.items.some((s) => ids.includes(s.time_series_id))"
          >
            <summary class="ts-about-name">{{ a.name }} · {{ a.items.length }}</summary>
            <label
              v-for="s in a.items"
              :key="s.time_series_id"
              class="ts-item ts-nested"
              :class="{ 'ts-picked': ids.includes(s.time_series_id) }"
            >
              <input
                type="checkbox"
                :checked="ids.includes(s.time_series_id)"
                @change="toggle(s)"
              />
              <span
                class="ts-swatch"
                :style="{
                  background: ids.includes(s.time_series_id)
                    ? colorOf(s.time_series_id)
                    : 'transparent',
                }"
              ></span>
              <span class="ts-label">{{ s.human_name }}</span>
              <span class="ts-meta">{{ unitText(s) }} · {{ s.point_count }}</span>
            </label>
          </details>
        </section>
      </div>
    </div>
    <div class="ts-main">
      <div class="ts-bar">
        <span class="ts-hint">{{
          picked.length
            ? "Drag across the chart to zoom; double-click to go back."
            : "Tick a time series to draw it."
        }}</span>
        <div role="group" aria-label="Range" class="ts-ranges">
          <button
            v-for="r in RANGES"
            :key="r.key"
            type="button"
            class="ts-range"
            :aria-pressed="range === r.key"
            @click="pickRange(r.key)"
          >
            {{ r.label }}
          </button>
        </div>
      </div>
      <div v-if="notice" class="ts-notice">{{ notice }}</div>
      <div v-if="error" class="ts-error">{{ error }}</div>
      <TimeSeriesChart
        :picked="picked"
        :answered="answered"
        :span="span"
        :color-of="colorOf"
        :remove="(id) => toggle(byId.get(id)!)"
        @resize="(w) => (chartWidth = w)"
      />
    </div>
  </div>
</template>

<style src="./timeSeriesChart.css"></style>
<style>
.ts-card {
  position: absolute;
  inset: 0;
  display: flex;
  container-type: inline-size;
  color: var(--datalib-fg);
  background: var(--datalib-bg);
  font-size: var(--datalib-font-size);
}
.ts-list {
  flex: 0 0 15rem;
  display: flex;
  flex-direction: column;
  border-right: 1px solid var(--datalib-border-soft);
  min-height: 0;
  max-height: 100%;
}
.ts-filter {
  padding: 0.5rem 0.75rem;
  border-bottom: 1px solid var(--datalib-border-soft);
}
.ts-input {
  width: 100%;
  box-sizing: border-box;
  font: inherit;
  padding: 0.2rem 0.45rem;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
}
.ts-groups {
  flex: 1 1 auto;
  overflow: auto;
  padding-bottom: 0.5rem;
}
.ts-source {
  display: flex;
  align-items: center;
  gap: 0.35rem;
  margin: 0;
  padding: 0.6rem 0.75rem 0.2rem;
  font-size: var(--datalib-font-size-small);
  font-weight: 600;
  color: var(--datalib-muted);
}
.ts-item {
  display: flex;
  align-items: center;
  gap: 0.4rem;
  padding: 0.25rem 0.75rem;
  cursor: pointer;
}
.ts-item:hover {
  background: var(--datalib-hover);
}
.ts-picked {
  background: var(--datalib-selected);
}
.ts-about-name {
  padding: 0.25rem 0.75rem;
  cursor: pointer;
  color: var(--datalib-muted);
}
.ts-about-name:hover {
  background: var(--datalib-hover);
}
.ts-nested {
  padding-left: 1.75rem;
}
.ts-label {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ts-meta {
  white-space: nowrap;
}
.ts-meta,
.ts-hint,
.ts-empty {
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
}
.ts-empty {
  padding: 0.75rem;
}
.ts-swatch {
  display: inline-block;
  width: 0.7rem;
  height: 0.2rem;
  border-radius: 1px;
  flex: none;
}
.ts-main {
  flex: 1 1 0;
  min-width: 0;
  display: flex;
  flex-direction: column;
  padding: 0.5rem 0.75rem;
  gap: 0.4rem;
  min-height: 16rem;
}
.ts-bar {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.4rem 0.75rem;
}
.ts-hint {
  flex: 1 1 auto;
}
.ts-ranges {
  display: flex;
  gap: 0.25rem;
}
.ts-range {
  font: inherit;
  font-size: var(--datalib-font-size-small);
  padding: 0.15rem 0.55rem;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface);
  color: var(--datalib-fg);
  cursor: pointer;
}
.ts-range[aria-pressed="true"] {
  background: var(--datalib-accent);
  border-color: var(--datalib-accent);
  color: var(--datalib-on-accent);
}
.ts-notice {
  padding: 0.3rem 0.5rem;
  border: 1px solid var(--datalib-warn-border);
  background: var(--datalib-warn-bg);
  color: var(--datalib-warn-fg);
  border-radius: var(--datalib-radius);
}
.ts-error {
  color: var(--datalib-error-fg);
  white-space: pre-line;
}
/* A narrow card stacks the list over the chart. */
@container (max-width: 34rem) {
  .ts-card {
    flex-direction: column;
  }
  .ts-list {
    flex: 0 0 40%;
    border-right: 0;
    border-bottom: 1px solid var(--datalib-border-soft);
  }
}
</style>
