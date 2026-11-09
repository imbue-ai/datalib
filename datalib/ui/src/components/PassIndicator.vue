<script setup lang="ts">
// A pill where the sync indicator sits, for a pass over the steps: the
// launch's migrate pass or freeing disk space (`pass.ts` makes the view).
// Amber while it runs, red once it is over with a source it could not
// do, green for a finished pass worth reporting, until dismissed. Its
// popover lists each source and how it stands. The app stays usable
// throughout.
import { onBeforeUnmount, ref, watch } from "vue";
import type { PassView } from "@/pass";
import { keepsItsName } from "@/pass";
import { showCard, showDataSources } from "@/surface";
import { uriFromEntity } from "@/cards/chipLinks";
import ChipMenu from "@/cards/ChipMenu.ce.vue";
import EntityChip from "@/cards/EntityChip.ce.vue";
import { adoptStyles } from "@/cards/documentStyles";
import "@/cards/chip.css";

adoptStyles(EntityChip, ChipMenu);

const props = defineProps<{ view: PassView | null; testid: string }>();
const emit = defineEmits<{ (e: "dismiss"): void }>();

const open = ref(false);
const el = ref<HTMLElement | null>(null);

function openSources() {
  open.value = false;
  showDataSources();
}

function dismiss() {
  open.value = false;
  emit("dismiss");
}

// `pointerdown` in the capture phase, because the top bar is a
// window-drag region whose handler takes the `mousedown` first.
function onDocDown(e: PointerEvent) {
  if (el.value && !el.value.contains(e.target as Node)) open.value = false;
}
function onKey(e: KeyboardEvent) {
  if (e.key === "Escape") open.value = false;
}
watch(open, (now) => {
  if (now) {
    document.addEventListener("pointerdown", onDocDown, true);
    window.addEventListener("keydown", onKey);
  } else {
    document.removeEventListener("pointerdown", onDocDown, true);
    window.removeEventListener("keydown", onKey);
  }
});
watch(
  () => props.view,
  (now) => {
    if (!now) open.value = false;
  },
);
onBeforeUnmount(() => (open.value = false));
</script>

<template>
  <div v-if="view" ref="el" class="pass" :data-testid="testid">
    <button
      class="pass-pill"
      :class="[`is-${view.tone}`, { 'is-open': open }]"
      aria-haspopup="dialog"
      :aria-expanded="open"
      @click="open = !open"
    >
      <span class="dot" />
      {{ view.label }}
    </button>
    <div v-if="open" class="pass-menu" role="dialog" :aria-label="view.label">
      <h3>{{ view.heading }}</h3>
      <p>{{ view.blurb }}</p>
      <ul class="pass-list" role="status" aria-live="polite">
        <li v-for="r in view.rows" :key="r.group" :data-state="r.state">
          <span class="pass-source">
            <EntityChip
              :uri="uriFromEntity('group', r.group)"
              :shown="r.source"
              :short="keepsItsName(r.group)"
              quiet
              :open-card="showCard"
            />
          </span>
          <span class="pass-state">{{ r.said }}</span>
          <span v-if="r.error" class="pass-error">{{ r.error }}</span>
        </li>
      </ul>
      <div v-if="!view.running" class="pass-actions">
        <button class="pass-link" @click="openSources">Open Sources</button>
        <button class="pass-link" @click="dismiss">Dismiss</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.pass {
  position: relative;
  display: flex;
}
.pass-pill {
  display: inline-flex;
  align-items: center;
  gap: 0.4rem;
  height: calc(var(--datalib-control-h) - 2px);
  box-sizing: border-box;
  padding: 0 0.6rem;
  border: 1px solid var(--datalib-warn-border);
  border-radius: 9999px;
  background: var(--datalib-warn-bg);
  color: var(--datalib-warn-fg);
  font: inherit;
  font-size: var(--datalib-font-size-small);
  font-weight: 600;
  cursor: pointer;
  white-space: nowrap;
}
.pass-pill.is-failed {
  border-color: var(--datalib-error-border);
  background: var(--datalib-error-bg);
  color: var(--datalib-error-fg);
}
.pass-pill.is-done {
  border-color: var(--datalib-border);
  background: var(--datalib-bg);
  color: var(--datalib-ok);
}
.pass-pill:hover,
.pass-pill.is-open {
  filter: brightness(0.97);
}
.dot {
  width: 0.5rem;
  height: 0.5rem;
  border-radius: 50%;
  background: var(--datalib-warn-dot);
}
.is-updating .dot {
  animation: pass-pulse 1.2s ease-in-out infinite;
}
.is-failed .dot {
  background: var(--datalib-error-fg);
}
.is-done .dot {
  background: var(--datalib-ok-dot);
}
@keyframes pass-pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.25;
  }
}
.pass-menu {
  position: absolute;
  top: calc(100% + 4px);
  right: 0;
  z-index: 1000;
  width: 340px;
  max-width: calc(100vw - 20px);
  box-sizing: border-box;
  padding: 12px;
  background: var(--datalib-bg);
  border: 1px solid var(--datalib-border);
  border-radius: calc(var(--datalib-radius) + 2px);
  box-shadow: 0 12px 32px rgba(0, 0, 0, 0.16);
  font-size: var(--datalib-font-size-small);
}
.pass-menu h3 {
  margin: 0 0 6px;
  font-size: var(--datalib-font-size);
}
.pass-menu p {
  margin: 0 0 10px;
  line-height: 1.45;
  color: var(--datalib-muted);
}
/* About eight rows, then it scrolls: a library can have dozens of sources. */
.pass-list {
  list-style: none;
  margin: 0;
  padding: 0;
  max-height: 15rem;
  overflow-y: auto;
}
.pass-list li {
  display: flex;
  flex-wrap: wrap;
  gap: 2px 8px;
  padding: 4px 0;
}
.pass-source {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.pass-state {
  color: var(--datalib-muted);
}
li[data-state="done"] .pass-state {
  color: var(--datalib-ok);
}
li[data-state="running"] .pass-state {
  color: var(--datalib-warn-fg);
  font-weight: 600;
}
li[data-state="failed"] .pass-state,
.pass-error {
  color: var(--datalib-error-fg);
}
li[data-state="failed"] .pass-state {
  font-weight: 600;
}
.pass-error {
  flex-basis: 100%;
  overflow-wrap: anywhere;
}
.pass-actions {
  display: flex;
  gap: 12px;
  margin-top: 8px;
  padding-top: 8px;
  border-top: 1px solid var(--datalib-border-soft);
}
.pass-link {
  padding: 0;
  border: none;
  background: none;
  color: var(--datalib-accent);
  font: inherit;
  cursor: pointer;
}
.pass-link:hover {
  text-decoration: underline;
}
</style>
