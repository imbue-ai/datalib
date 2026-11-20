<script setup lang="ts">
// The screen in front of the library while it is upgraded for this
// version: the launch's pass over the steps, then the steps it found in a
// shape this version does not read, which the server re-runs by itself.
// Nothing reads those stores meanwhile, so the cards would only show
// refusals. Sources stays reachable, for a source whose re-run fails.
import { computed } from "vue";
import type { Upgrade } from "@/api";
import { keepsItsName, upgradingView } from "@/pass";
import { showCard } from "@/surface";
import { uriFromEntity } from "@/cards/chipLinks";
import ChipMenu from "@/cards/ChipMenu.ce.vue";
import EntityChip from "@/cards/EntityChip.ce.vue";
import { adoptStyles } from "@/cards/documentStyles";
import "@/cards/chip.css";

adoptStyles(EntityChip, ChipMenu);

const props = defineProps<{ upgrade: Upgrade }>();
const emit = defineEmits<{ (e: "open-sources"): void }>();

const view = computed(() => upgradingView(props.upgrade));
</script>

<template>
  <section class="upgrading notice" data-testid="upgrading">
    <div class="card">
      <h2>{{ view.heading }}</h2>
      <p>{{ view.blurb }}</p>
      <ul class="rows" role="status" aria-live="polite">
        <li v-for="r in view.rows" :key="r.group" :data-state="r.state">
          <EntityChip
            :uri="uriFromEntity('group', r.group)"
            :shown="r.source"
            :short="keepsItsName(r.group)"
            quiet
            :open-card="showCard"
          />
          <span class="said">{{ r.said }}</span>
          <span v-if="r.error" class="error">{{ r.error }}</span>
        </li>
      </ul>
      <p class="sources">
        A source that cannot be re-run keeps the library waiting.
        <button class="btn ghost" @click="emit('open-sources')">Open Sources</button>
        to see its log, fix it or turn it off.
      </p>
    </div>
  </section>
</template>

<style scoped src="./notice.css"></style>
<style scoped>
.card {
  width: 100%;
  max-width: 48rem;
}
.rows {
  list-style: none;
  margin: 0.75rem 0;
  padding: 0;
}
.rows li {
  display: flex;
  align-items: baseline;
  gap: 0.75rem;
  padding: 0.25rem 0;
}
.said {
  color: var(--datalib-muted);
}
.error {
  color: var(--datalib-error-fg);
}
</style>
