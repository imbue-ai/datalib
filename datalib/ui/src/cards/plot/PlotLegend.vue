<script setup lang="ts">
// A chart's legend: every line, its colour, and what the chart reads for
// it. A click turns a line on or off; a double-click shows it alone, and
// another shows them all again. Its styles are plot.css.
import { isolated, toggled } from "./plot";

export type LegendEntry = {
  key: string;
  label: string;
  /// As CSS, so a `var(--token)` works.
  color: string;
  /// What the chart reads for the line now, if it reads one.
  value?: string | null;
  /// A word after the label, such as which axis the line is on.
  note?: string;
};

defineProps<{ entries: LegendEntry[]; shown: boolean[] }>();
const emit = defineEmits<{ "update:shown": [shown: boolean[]] }>();
defineSlots<{ after?: (props: { entry: LegendEntry }) => unknown }>();
</script>

<template>
  <ul class="plot-legend">
    <li v-for="(e, i) in entries" :key="e.key">
      <button
        type="button"
        class="plot-legend-entry"
        :aria-pressed="shown[i] !== false"
        :title="`${e.label}: click to turn on or off, double-click to show alone`"
        @click="emit('update:shown', toggled(shown, i))"
        @dblclick="emit('update:shown', isolated(shown, i))"
      >
        <span class="plot-swatch" :style="{ background: e.color }" />
        <span class="plot-legend-label">{{ e.label }}</span>
        <span v-if="e.note" class="plot-legend-note">{{ e.note }}</span>
        <span v-if="e.value != null" class="plot-legend-value">{{ e.value }}</span>
      </button>
      <slot name="after" :entry="e" />
    </li>
  </ul>
</template>
