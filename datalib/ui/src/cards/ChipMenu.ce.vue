<script setup lang="ts">
// The right-click menu on a chip, drawn over the document frame. It
// shows the entries `chipMenu` or `entityMenu` built and reports which
// one was picked;
// what each does is the document view's business. A click anywhere
// else, a second right-click or Escape closes it.
import { computed, onMounted, onUnmounted } from "vue";

import type { ChipMenuEntry, ChipMenuId } from "./contacts";
import type { EntityMenuEntry, EntityMenuId } from "./entities";

const props = defineProps<{ entries: (ChipMenuEntry | EntityMenuEntry)[]; x: number; y: number }>();
const emit = defineEmits<{ pick: [id: ChipMenuId | EntityMenuId]; close: [] }>();

const style = computed(() => ({
  left: `${Math.min(props.x, window.innerWidth - 260)}px`,
  top: `${Math.min(props.y, window.innerHeight - 40 * (props.entries.length + 1))}px`,
}));

function onKey(ev: KeyboardEvent) {
  if (ev.key === "Escape") emit("close");
}
onMounted(() => window.addEventListener("keydown", onKey));
onUnmounted(() => window.removeEventListener("keydown", onKey));
</script>

<template>
  <div class="chip-menu-overlay" @click="emit('close')" @contextmenu.prevent="emit('close')">
    <div class="chip-menu" role="menu" :style="style" @click.stop>
      <template v-for="e in entries" :key="e.id">
        <div v-if="e.separator" class="chip-menu-divider" />
        <div class="chip-menu-item" role="menuitem" @click="emit('pick', e.id)">{{ e.label }}</div>
      </template>
    </div>
  </div>
</template>

<style>
.chip-menu-overlay {
  position: fixed;
  inset: 0;
  z-index: 1500;
  background: transparent;
}
.chip-menu {
  position: fixed;
  min-width: 180px;
  max-width: 320px;
  padding: 4px 0;
  background: var(--datalib-input-bg, #fff);
  color: var(--datalib-fg, #000);
  border: 1px solid var(--datalib-border, #ccc);
  border-radius: var(--datalib-radius, 6px);
  box-shadow: 0 2px 10px rgb(0 0 0 / 20%);
  font-size: 13px;
}
.chip-menu-item {
  padding: 6px 14px;
  cursor: pointer;
  user-select: none;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.chip-menu-item:hover {
  background: var(--datalib-hover, #eee);
}
.chip-menu-divider {
  height: 1px;
  background: var(--datalib-border, #ccc);
  margin: 4px 0;
}
</style>
