<script setup lang="ts">
// The common controls every card carries in its chrome bar, regardless
// of layout: a link to open the card alone (↗), the agent hand-off
// button (🤖), and the close button (✕). All three are pure functions
// of the card's source and its CardCtx, so the layouts (miller, tiling,
// tree) all render this same component instead of duplicating the
// markup and CSS. Close goes through ctx.host.close() — the host
// command built for exactly this — so nothing here knows the layout.
import { computed } from "vue";
import { encodeColumns } from "@/router/columns";
import { handOffToAgent } from "@/cards/handoff";
import { devMode } from "@/devMode";
import type { CardCtx } from "@/cards/types";

const props = defineProps<{
  source: string;
  ctx: CardCtx;
}>();

// Standalone view: a miller URL containing just this card, at its
// current state (initialState is a live getter in every layout).
const aloneHref = computed(() =>
  encodeColumns([{ code: props.source, state: props.ctx.initialState }]),
);
</script>

<template>
  <a
    v-if="source.trim() !== ''"
    class="card-control card-control--alone"
    :href="aloneHref"
    target="_blank"
    rel="noopener"
    title="open this card alone"
    >↗</a
  >
  <!-- The agent hand-off rewrites the card's source — a dev-mode
       affordance, hidden alongside the source box. -->
  <button
    v-if="devMode"
    class="card-control card-control--agent"
    title="let a coding agent work on this card"
    @click="handOffToAgent(ctx.host)"
  >
    🤖
  </button>
  <button
    class="card-control card-control--close"
    title="close card"
    @click="ctx.host.close()"
  >
    ✕
  </button>
</template>

<style scoped>
/* The controls sit directly in the layout's chrome bar (a flex row).
   No wrapping element — the component's template renders the controls
   as siblings — so they keep the bar's existing gap and alignment. */
.card-control {
  flex: 0 0 auto;
  border: none;
  background: transparent;
  color: inherit;
  opacity: 0.6;
  cursor: pointer;
  font-size: 0.8rem;
  line-height: 1.5;
  text-decoration: none;
  padding: 0.2rem 0;
}
.card-control:hover {
  opacity: 1;
}
</style>
