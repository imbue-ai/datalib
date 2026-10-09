<script setup lang="ts">
// Routed view for the card surface: the containers layout, and the
// status bar along the bottom — the data root and its size, the log,
// then the density and the edit toggle flush right.
import { onBeforeUnmount, onMounted, useTemplateRef } from "vue";
import ContainersView from "@/views/ContainersView.vue";
import RootStorageBar from "@/components/RootStorageBar.vue";
import { editMode } from "@/editMode";
import { density, larger, smaller } from "@/density";
import { MAX_STEP, MIN_STEP, STEP, STEPS, onScale, stepIndex } from "@/densityScale";
import { LOG_CARD, surface, type SurfaceCommands } from "@/surface";

// The toolbar's commands go to the layout.
const containers = useTemplateRef<SurfaceCommands>("containers");
onMounted(() => {
  surface.value = {
    addCard: () => containers.value?.addCard(),
    showCard: (source) => containers.value?.showCard(source),
  };
});
onBeforeUnmount(() => {
  surface.value = null;
});
</script>

<template>
  <div class="cards-root">
    <ContainersView ref="containers" />
    <div class="cards-statusbar">
      <RootStorageBar />
      <!-- What the system is doing belongs down here with the data
           root's size: the log over every run, revealed if a card
           already shows it. -->
      <button
        class="cards-logs"
        title="the run log: every line the runner, the steps and the server wrote"
        @click="containers?.showCard(LOG_CARD)"
      >
        Logs
      </button>
      <!-- Density: spacing only — rows, controls, padding — packed close
           or spread out, as the glyphs draw it; text keeps its size. The
           slider between them sets any step directly. -->
      <div class="cards-toggle cards-size" role="group" aria-label="density">
        <button
          aria-label="More compact"
          title="more compact: tighter spacing, more on screen"
          :disabled="density <= MIN_STEP"
          @click="smaller"
        >
          <svg viewBox="0 0 24 24" aria-hidden="true">
            <path d="M4 8h16M4 12h16M4 16h16" />
          </svg>
        </button>
        <input
          class="cards-size-slider"
          type="range"
          aria-label="Density"
          :min="MIN_STEP"
          :max="MAX_STEP"
          :step="STEP"
          :value="density"
          :aria-valuetext="`step ${stepIndex(density) + 1} of ${STEPS}`"
          :title="`density: step ${stepIndex(density) + 1} of ${STEPS}`"
          @input="density = onScale(($event.target as HTMLInputElement).value)"
        />
        <button
          aria-label="More spacious"
          title="more spacious: more room between things"
          :disabled="density >= MAX_STEP"
          @click="larger"
        >
          <svg viewBox="0 0 24 24" aria-hidden="true">
            <path d="M4 4h16M4 12h16M4 20h16" />
          </svg>
        </button>
      </div>
      <button
        class="cards-edit-toggle"
        :class="{ 'is-active': editMode }"
        :aria-pressed="editMode"
        aria-label="Edit"
        title="edit mode: show and edit each card's source, and every container, solidified ones included"
        @click="editMode = !editMode"
      >
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M4 20h4L19 9l-4-4L4 16z" />
          <path d="M13.5 6.5l4 4" />
        </svg>
      </button>
    </div>
  </div>
</template>

<style scoped>
.cards-root {
  display: flex;
  flex-direction: column;
  /* Fill whatever the shell's flex layout gives us (everything below
     the header); basis 0 + min-height 0 so intrinsic content height
     can't stretch the page. */
  flex: 1 1 0;
  min-height: 0;
}
.cards-statusbar {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: 10px;
  height: var(--datalib-statusbar-h);
  box-sizing: border-box;
  padding: 0 10px;
  border-top: 1px solid var(--datalib-border);
  background: var(--datalib-sidebar);
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
}
.cards-logs,
.cards-edit-toggle {
  flex: 0 0 auto;
  height: calc(var(--datalib-control-h) - 6px);
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface);
  color: var(--datalib-fg);
  cursor: pointer;
  font: inherit;
  padding: 0 8px;
}
.cards-logs:hover,
.cards-edit-toggle:hover {
  background: var(--datalib-hover);
}
.cards-edit-toggle {
  display: flex;
  align-items: center;
}
.cards-edit-toggle svg {
  width: var(--datalib-icon-size);
  height: var(--datalib-icon-size);
  fill: none;
  stroke: currentColor;
  stroke-width: 2;
  stroke-linecap: round;
  stroke-linejoin: round;
}
.cards-edit-toggle.is-active {
  background: var(--datalib-accent);
  border-color: var(--datalib-accent);
  color: var(--datalib-on-accent);
}
/* The toggles sit flush right as a cluster — the first one carries the
   auto margin. */
.cards-size {
  margin-left: auto;
  align-items: center;
}
.cards-size button {
  display: flex;
  align-items: center;
}
.cards-size svg {
  width: var(--datalib-icon-size);
  height: var(--datalib-icon-size);
  fill: none;
  stroke: currentColor;
  stroke-width: 2;
  stroke-linecap: round;
}
.cards-size-slider {
  width: 90px;
  margin: 0 6px;
  accent-color: var(--datalib-fg);
  cursor: pointer;
}
.cards-toggle button:disabled {
  color: var(--datalib-faint);
  cursor: default;
}
.cards-toggle {
  /* Always claim full intrinsic width (never shrink). */
  flex: 0 0 auto;
  display: flex;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  overflow: hidden;
}
.cards-toggle button {
  height: calc(var(--datalib-control-h) - 8px);
  border: none;
  background: var(--datalib-surface);
  color: var(--datalib-fg);
  cursor: pointer;
  font: inherit;
  padding: 0 8px;
}
.cards-toggle button + button {
  border-left: 1px solid var(--datalib-border);
}
.cards-toggle button:hover:not(:disabled) {
  background: var(--datalib-hover);
}
.cards-toggle button.is-active {
  background: var(--datalib-fg);
  color: var(--datalib-surface);
}
</style>
