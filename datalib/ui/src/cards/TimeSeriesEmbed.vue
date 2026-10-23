<script setup lang="ts">
// A chart inside a document: the time series a render named in a
// `<div data-time-series="<id> <id>">`, over everything they hold.
// Mounted into the document's frame by timeSeriesEmbed.ts.
import { computed, onMounted, ref } from "vue";
import { toMs, type Answered, type Listed } from "./timeSeries";
import { fetchPoints, fetchTimeSeries } from "./timeSeriesApi";
import { seriesColor } from "./plot/plot";
import TimeSeriesChart from "./TimeSeriesChart.vue";

const props = defineProps<{ ids: string[] }>();

const picked = ref<Listed[]>([]);
const answered = ref<Map<string, Answered>>(new Map());
const error = ref<string | null>(null);
const loaded = ref(false);
const width = ref(600);

const colorOf = (id: string) => seriesColor(Math.max(0, props.ids.indexOf(id)));

const span = computed(() => {
  const firsts = picked.value.flatMap((p) =>
    p.first_t === null ? [] : [toMs(p.time_kind, p.first_t)],
  );
  const lasts = picked.value.flatMap((p) =>
    p.last_t === null ? [] : [toMs(p.time_kind, p.last_t)],
  );
  if (!firsts.length || !lasts.length) return null;
  return { fromMs: Math.min(...firsts), toMs: Math.max(...lasts) };
});

async function load() {
  try {
    const all = await fetchTimeSeries();
    if (all === null) {
      error.value = "This data root keeps no time-series index, so no chart.";
      return;
    }
    const byId = new Map(all.map((s) => [s.time_series_id, s]));
    picked.value = props.ids.flatMap((id) => {
      const s = byId.get(id);
      return s && s.value_type !== "text" ? [s] : [];
    });
    const s = span.value;
    if (s) {
      const got = await fetchPoints(picked.value, s, Math.max(50, Math.round(width.value)));
      answered.value = got.answered;
      if (got.unreadable.length) error.value = got.unreadable.map((u) => u.error).join("\n");
    }
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    loaded.value = true;
  }
}

onMounted(() => void load());
</script>

<template>
  <div class="ts-embed" :class="{ 'is-note': error || (loaded && !picked.length) }">
    <div v-if="error" class="ts-embed-note">{{ error }}</div>
    <div v-else-if="loaded && !picked.length" class="ts-embed-note">
      This chart's time series are not in the time-series index yet.
    </div>
    <TimeSeriesChart
      v-else
      :picked="picked"
      :answered="answered"
      :span="span"
      :color-of="colorOf"
      @resize="(w) => (width = w)"
    />
  </div>
</template>
