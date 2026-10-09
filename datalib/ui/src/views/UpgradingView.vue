<script setup lang="ts">
// The blocking screen while the first launch of a build asks every step
// to migrate (docs/dev/plans/upgrade_on_launch.md), one row per source.
// Nothing syncs until it is done; the page drops it on the
// `upgrade_changed` that says so.
import { computed } from "vue";
import type { ConfigResponse, MigrateState } from "@/api";
import { sourceRows } from "@/upgrade";

const props = defineProps<{ config: ConfigResponse }>();
const rows = computed(() => sourceRows(props.config.upgrade));

const said: Record<MigrateState, string> = {
  waiting: "Waiting",
  running: "Updating…",
  done: "Done",
  failed: "Could not update",
};
</script>

<template>
  <section class="upgrading notice">
    <div class="card" role="status" aria-live="polite">
      <h2>Updating your data for this version of datalib</h2>
      <p>
        This is the first time this version has opened this library, so each source is bringing what
        it stored up to date before anything syncs. Nothing is downloaded while this runs.
      </p>
      <ul class="stores">
        <li v-for="s in rows" :key="s.source" :data-state="s.state">
          <span class="source">{{ s.source }}</span>
          <span class="state">{{ said[s.state] }}</span>
          <span v-if="s.error" class="error">{{ s.error }}</span>
        </li>
      </ul>
    </div>
  </section>
</template>

<style scoped src="./notice.css"></style>
<style scoped>
.card {
  width: 100%;
  max-width: 48rem;
}
.stores {
  list-style: none;
  margin: 0.75rem 0 0;
  padding: 0;
}
.stores li {
  display: flex;
  flex-wrap: wrap;
  gap: 0.25rem 0.75rem;
  padding: 0.25rem 0;
}
.source {
  min-width: 12rem;
  font-weight: 500;
}
.state {
  color: var(--datalib-muted);
}
li[data-state="failed"] .state,
.error {
  color: var(--datalib-error-fg);
}
.error {
  flex-basis: 100%;
  font-size: var(--datalib-font-size-small);
  overflow-wrap: anywhere;
}
</style>
