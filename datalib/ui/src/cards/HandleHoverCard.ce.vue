<script setup lang="ts">
// What hovering a handle chip shows: who it is, the identifier behind
// the short name, and how this message showed it. Read-only; a click on
// the chip opens `HandlePopover` to change anything.
import { computed } from "vue";

import { iconUrl } from "@/config/icons";
import type { HoverCard } from "./contacts";

const props = defineProps<{ card: HoverCard; x: number; y: number }>();

const mark = computed(() => iconUrl(props.card.icon));
const style = computed(() => ({
  left: `${Math.min(props.x, window.innerWidth - 300)}px`,
  top: `${props.y + 6}px`,
}));
</script>

<template>
  <div class="handle-hover-card" :style="style" role="tooltip">
    <div class="hh-name">{{ card.name }}</div>
    <div class="hh-value">
      <img v-if="mark" :src="mark" alt="" class="hh-mark" />
      <span>{{ card.value }}</span>
    </div>
    <div v-for="line in card.lines" :key="line" class="hh-line">{{ line }}</div>
  </div>
</template>

<style>
.handle-hover-card {
  position: fixed;
  z-index: 49;
  max-width: 280px;
  padding: 8px 10px;
  background: var(--datalib-input-bg, #fff);
  border: 1px solid var(--datalib-border, #d8d8d8);
  border-radius: 8px;
  box-shadow: 0 4px 16px rgb(0 0 0 / 16%);
  font-size: 12px;
  pointer-events: none;
}
.handle-hover-card .hh-name {
  font-weight: 600;
  font-size: 13px;
}
.handle-hover-card .hh-value {
  display: flex;
  align-items: center;
  gap: 5px;
  margin: 2px 0 4px;
  overflow-wrap: anywhere;
}
.handle-hover-card .hh-mark {
  width: 12px;
  height: 12px;
}
.handle-hover-card .hh-line {
  color: var(--datalib-muted, #94a3b8);
}
</style>
