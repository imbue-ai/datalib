<script setup lang="ts">
// One of the Dashboard's sections as a card of its own
// (libs/dashboardSections.ts). The card loads what its section reads;
// the section draws it.
import { ref, type Component } from "vue";
import { useDashboard } from "./useDashboard";
import type { CardCtx } from "./types";

const props = defineProps<{
  ctx: CardCtx;
  section: Component;
  title: string;
  // Load the newest documents, which only the activity section reads.
  recent?: boolean;
  // Edge to edge: the sync bar spans its card.
  flush?: boolean;
}>();

props.ctx.setTitle(props.title);
const el = ref<HTMLElement | null>(null);
const d = useDashboard(props.ctx, { recent: props.recent === true, el });
</script>

<template>
  <div ref="el" class="dash-section" :class="{ 'dash-section-flush': flush }">
    <component :is="section" :d="d" />
  </div>
</template>
