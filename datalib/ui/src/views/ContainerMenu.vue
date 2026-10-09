<script setup lang="ts">
// The panel a container's folder tab, a card's ⋯ or a sidebar row opens:
// a header naming what it is for, then sections — a row of tiles to
// choose among (a layout), a switch (Solidified), rows of actions. It is
// rebuilt from `build` as the tree changes, measures itself to stay on
// screen, and closes on a click outside it, on Escape, or when the
// window loses focus.
import { computed, nextTick, onBeforeUnmount, onMounted, ref, useTemplateRef } from "vue";
import type { Panel, PanelAction } from "@/views/containersApi";

const props = defineProps<{ build: () => Panel; x: number; y: number }>();
const emit = defineEmits<{ close: [] }>();

const panel = computed(props.build);
const el = useTemplateRef<HTMLDivElement>("el");
const pos = ref({ left: props.x, top: props.y });
const MARGIN = 4;

onMounted(async () => {
  await nextTick();
  const box = el.value?.getBoundingClientRect();
  if (box) {
    pos.value = {
      left: Math.max(MARGIN, Math.min(props.x, window.innerWidth - box.width - MARGIN)),
      top: Math.max(MARGIN, Math.min(props.y, window.innerHeight - box.height - MARGIN)),
    };
  }
  window.addEventListener("pointerdown", onPointerOutside, true);
  window.addEventListener("keydown", onKey, true);
  window.addEventListener("blur", close);
});
onBeforeUnmount(() => {
  window.removeEventListener("pointerdown", onPointerOutside, true);
  window.removeEventListener("keydown", onKey, true);
  window.removeEventListener("blur", close);
});

function close() {
  emit("close");
}
function onPointerOutside(ev: PointerEvent) {
  if (!el.value?.contains(ev.target as Node)) close();
}
function onKey(ev: KeyboardEvent) {
  if (ev.key === "Escape") close();
}
function run(action: Pick<PanelAction, "run" | "stay">) {
  if (!action.stay) close();
  action.run();
}
</script>

<template>
  <div
    ref="el"
    class="cp"
    role="menu"
    :aria-label="panel.title"
    :style="{ left: pos.left + 'px', top: pos.top + 'px' }"
  >
    <div class="cp-head">
      <svg viewBox="0 0 24 24" aria-hidden="true"><path :d="panel.icon" /></svg>
      <span class="cp-title">{{ panel.title }}</span>
    </div>
    <template v-for="(section, i) in panel.sections" :key="i">
      <div v-if="section.kind === 'tiles'" class="cp-section">
        <div class="cp-section-title">{{ section.title }}</div>
        <div class="cp-tiles" role="group" :aria-label="section.title">
          <button
            v-for="a in section.actions"
            :key="a.label"
            class="cp-tile"
            :class="{ 'is-current': a.current }"
            role="menuitemradio"
            :aria-checked="a.current === true"
            @click="run(a)"
          >
            <svg viewBox="0 0 24 24" aria-hidden="true"><path :d="a.icon" /></svg>
            <span>{{ a.label }}</span>
          </button>
        </div>
      </div>
      <button
        v-else-if="section.kind === 'toggle'"
        class="cp-toggle"
        role="menuitemcheckbox"
        :aria-checked="section.on"
        @click="run({ run: section.run, stay: true })"
      >
        <svg viewBox="0 0 24 24" aria-hidden="true"><path :d="section.icon" /></svg>
        <span class="cp-toggle-text">
          <span class="cp-toggle-label">{{ section.label }}</span>
          <span class="cp-hint">{{ section.hint }}</span>
        </span>
        <span class="cp-switch" :class="{ 'is-on': section.on }" aria-hidden="true" />
      </button>
      <div v-else class="cp-section">
        <div v-if="section.title" class="cp-section-title">{{ section.title }}</div>
        <button
          v-for="a in section.actions"
          :key="a.label"
          class="cp-row"
          :class="{ 'is-danger': a.danger }"
          role="menuitem"
          @click="run(a)"
        >
          <svg viewBox="0 0 24 24" aria-hidden="true"><path :d="a.icon" /></svg>
          {{ a.label }}
        </button>
      </div>
    </template>
  </div>
</template>

<style scoped>
.cp {
  position: fixed;
  z-index: 20;
  width: 290px;
  max-height: calc(100vh - 8px);
  overflow-y: auto;
  box-sizing: border-box;
  display: flex;
  flex-direction: column;
  padding: 6px;
  background: var(--datalib-surface);
  color: var(--datalib-fg);
  border: 1px solid var(--datalib-border);
  border-radius: calc(var(--datalib-radius) + 4px);
  box-shadow: 0 12px 32px rgba(0, 0, 0, 0.2);
  font-size: var(--datalib-font-size);
}
svg {
  flex: 0 0 auto;
  width: 15px;
  height: 15px;
  fill: none;
  stroke: currentColor;
  stroke-width: 2;
  stroke-linecap: round;
  stroke-linejoin: round;
}
.cp-head {
  display: flex;
  align-items: center;
  gap: 7px;
  padding: 4px 6px 8px;
  border-bottom: 1px solid var(--datalib-border-soft);
}
.cp-head svg {
  color: var(--datalib-accent);
}
.cp-title {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-weight: 600;
}
.cp-section {
  display: flex;
  flex-direction: column;
  padding: 6px 0 2px;
  border-bottom: 1px solid var(--datalib-border-soft);
}
.cp-section:last-child {
  border-bottom: none;
}
.cp-section-title {
  padding: 0 6px 4px;
  font-size: 10px;
  font-weight: 700;
  letter-spacing: 0.05em;
  text-transform: uppercase;
  color: var(--datalib-muted);
}
.cp-tiles {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(62px, 1fr));
  gap: 4px;
  padding: 0 4px 4px;
}
.cp-tile {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
  padding: 7px 2px 5px;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-bg);
  color: var(--datalib-fg);
  font: inherit;
  font-size: var(--datalib-font-size-small);
  cursor: pointer;
}
.cp-tile svg {
  width: 18px;
  height: 18px;
}
.cp-tile:hover {
  background: var(--datalib-hover);
}
.cp-tile.is-current {
  border-color: var(--datalib-accent);
  background: color-mix(in srgb, var(--datalib-accent) 12%, var(--datalib-bg));
  color: var(--datalib-accent);
  font-weight: 600;
}
.cp-toggle {
  display: flex;
  align-items: center;
  gap: 8px;
  margin: 6px 0 2px;
  padding: 6px;
  border: none;
  border-bottom: 1px solid var(--datalib-border-soft);
  background: transparent;
  color: inherit;
  font: inherit;
  text-align: left;
  cursor: pointer;
}
.cp-toggle:hover {
  background: var(--datalib-hover);
}
.cp-toggle-text {
  flex: 1 1 auto;
  display: flex;
  flex-direction: column;
  gap: 1px;
}
.cp-toggle-label {
  font-weight: 600;
}
.cp-hint {
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
  line-height: 1.35;
}
.cp-switch {
  flex: 0 0 auto;
  position: relative;
  width: 28px;
  height: 16px;
  border-radius: 8px;
  background: var(--datalib-border);
  transition: background 0.15s;
}
.cp-switch::after {
  content: "";
  position: absolute;
  top: 2px;
  left: 2px;
  width: 12px;
  height: 12px;
  border-radius: 50%;
  background: #fff;
  transition: left 0.15s;
}
.cp-switch.is-on {
  background: var(--datalib-accent);
}
.cp-switch.is-on::after {
  left: 14px;
}
.cp-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 5px 6px;
  border: none;
  border-radius: var(--datalib-radius);
  background: transparent;
  color: inherit;
  font: inherit;
  text-align: left;
  cursor: pointer;
}
.cp-row svg {
  color: var(--datalib-muted);
}
.cp-row:hover {
  background: var(--datalib-hover);
}
.cp-row.is-danger,
.cp-row.is-danger svg {
  color: var(--datalib-error-fg);
}
</style>
