<script setup lang="ts">
// How big the library is: its items, its size on disk, and a bar of
// what takes the space.
import { formatBytes } from "@/config/bytes";
import type { Dashboard } from "./useDashboard";

defineProps<{ d: Dashboard }>();
</script>

<template>
  <section class="panel" aria-label="Your library">
    <h2 class="panel-head">
      <span class="section-title">Your library</span>
      <button class="link" @click="d.showRoot">
        {{ d.canReveal ? d.revealLabel : "Copy folder path" }}
      </button>
    </h2>
    <div class="library">
      <div class="stats">
        <span
          ><span class="stat">{{ d.totalItems.toLocaleString() }}</span> items</span
        >
        <span
          ><span class="stat">{{ formatBytes(d.rootBytes) }}</span> on disk</span
        >
      </div>
      <div
        v-if="d.segments.length"
        class="bar"
        role="img"
        :aria-label="`${formatBytes(d.rootBytes)} on disk`"
      >
        <span
          v-for="s in d.segments"
          :key="s.id"
          :class="s.tone"
          :style="{ width: s.pct + '%' }"
          :title="`${s.label}: ${formatBytes(s.bytes)}`"
        />
      </div>
      <div class="legend">
        <span v-for="s in d.segments" :key="s.id"
          ><span class="swatch" :class="s.tone" />{{ s.label }} {{ formatBytes(s.bytes) }}</span
        >
      </div>
    </div>
  </section>
</template>
