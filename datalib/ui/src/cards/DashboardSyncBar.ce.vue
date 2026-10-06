<script setup lang="ts">
// The Dashboard's sync bar: when the library last synced, or its next
// step (add a source, start the first sync), and the button that syncs
// everything or stops it.
import type { Dashboard } from "./useDashboard";

defineProps<{ d: Dashboard }>();
</script>

<template>
  <header class="dashboard-head" :class="{ 'dashboard-head-next': d.stage === 'first_sync' }">
    <span class="dashboard-state" :class="`tone-${d.lastRun.tone}`">
      <span class="dot" />{{ d.lastRun.text }}
    </span>
    <button
      class="dashboard-btn"
      :class="{ 'dashboard-btn-strong': d.stage === 'first_sync' }"
      :disabled="!!d.syncAll.blocked"
      :title="d.syncAll.blocked ?? d.syncAll.label"
      @click="d.syncEverything"
    >
      {{ d.syncAll.stops.length > 0 ? "Stop syncing" : "Sync now" }}
    </button>
  </header>
</template>
