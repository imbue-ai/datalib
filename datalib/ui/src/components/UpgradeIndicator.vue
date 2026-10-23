<script setup lang="ts">
// The toolbar's upgrade pill, where the sync indicator sits: amber while
// the first launch of a build asks every step to migrate
// (docs/dev/plans/upgrade_on_launch.md), red once the pass is over with a
// source it could not update. The app stays usable throughout; only
// syncing waits, and the Sources card's Sync buttons say so.
import { computed, onBeforeUnmount, ref, watch } from "vue";
import type { MigrateState, Upgrade } from "@/api";
import { showCard, showDataSources } from "@/surface";
import { keepsItsName, sourceRows, upgradePill } from "@/upgrade";
import { uriFromEntity } from "@/cards/chipLinks";
import ChipMenu from "@/cards/ChipMenu.ce.vue";
import EntityChip from "@/cards/EntityChip.ce.vue";
import { adoptStyles } from "@/cards/documentStyles";
import "@/cards/chip.css";

adoptStyles(EntityChip, ChipMenu);

const props = defineProps<{ upgrade: Upgrade }>();

const dismissed = ref(false);
const open = ref(false);
const el = ref<HTMLElement | null>(null);

const pill = computed(() => upgradePill(props.upgrade, dismissed.value));
const rows = computed(() => {
  const all = sourceRows(props.upgrade);
  return props.upgrade.migrating ? all : all.filter((r) => r.state === "failed");
});
const sourceCount = computed(() => sourceRows(props.upgrade).length);
const failedCount = computed(() => rows.value.filter((r) => r.state === "failed").length);

const said: Record<MigrateState, string> = {
  waiting: "Waiting",
  running: "Updating…",
  done: "Done",
  failed: "Could not update",
};

function openSources() {
  open.value = false;
  showDataSources();
}

function dismiss() {
  open.value = false;
  dismissed.value = true;
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
watch(pill, (now) => {
  if (!now) open.value = false;
});
onBeforeUnmount(() => (open.value = false));
</script>

<template>
  <div v-if="pill" ref="el" class="upgrade">
    <button
      class="upgrade-pill"
      :class="[`is-${pill.tone}`, { 'is-open': open }]"
      aria-haspopup="dialog"
      :aria-expanded="open"
      @click="open = !open"
    >
      <span class="dot" />
      {{ pill.label }}
    </button>
    <div v-if="open" class="upgrade-menu" role="dialog" :aria-label="pill.label">
      <template v-if="upgrade.migrating">
        <h3>Updating your data for this version</h3>
        <p>
          Each source is bringing what it stored up to date. Search and reading work as usual;
          syncing waits until this is done. Nothing is downloaded while it runs.
        </p>
      </template>
      <template v-else>
        <h3>Your data is updated for this version</h3>
        <p>
          {{ sourceCount - failedCount }} of {{ sourceCount }} sources are up to date and can sync
          again. {{ failedCount === 1 ? "One" : failedCount }} could not be updated; the Sources
          card has the log.
        </p>
      </template>
      <ul class="upgrade-list" role="status" aria-live="polite">
        <li v-for="r in rows" :key="r.group" :data-state="r.state">
          <span class="upgrade-source">
            <EntityChip
              :uri="uriFromEntity('group', r.group)"
              :shown="r.source"
              :short="keepsItsName(r.group)"
              quiet
              :open-card="showCard"
            />
          </span>
          <span class="upgrade-state">{{ said[r.state] }}</span>
          <span v-if="r.error" class="upgrade-error">{{ r.error }}</span>
        </li>
      </ul>
      <div v-if="!upgrade.migrating" class="upgrade-actions">
        <button class="upgrade-link" @click="openSources">Open Sources</button>
        <button class="upgrade-link" @click="dismiss">Dismiss</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.upgrade {
  position: relative;
  display: flex;
}
.upgrade-pill {
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
.upgrade-pill.is-failed {
  border-color: var(--datalib-error-border);
  background: var(--datalib-error-bg);
  color: var(--datalib-error-fg);
}
.upgrade-pill:hover,
.upgrade-pill.is-open {
  filter: brightness(0.97);
}
.dot {
  width: 0.5rem;
  height: 0.5rem;
  border-radius: 50%;
  background: var(--datalib-warn-dot);
}
.is-updating .dot {
  animation: upgrade-pulse 1.2s ease-in-out infinite;
}
.is-failed .dot {
  background: var(--datalib-error-fg);
}
@keyframes upgrade-pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.25;
  }
}
.upgrade-menu {
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
.upgrade-menu h3 {
  margin: 0 0 6px;
  font-size: var(--datalib-font-size);
}
.upgrade-menu p {
  margin: 0 0 10px;
  line-height: 1.45;
  color: var(--datalib-muted);
}
/* About eight rows, then it scrolls: a library can have dozens of sources. */
.upgrade-list {
  list-style: none;
  margin: 0;
  padding: 0;
  max-height: 15rem;
  overflow-y: auto;
}
.upgrade-list li {
  display: flex;
  flex-wrap: wrap;
  gap: 2px 8px;
  padding: 4px 0;
}
.upgrade-source {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.upgrade-state {
  color: var(--datalib-muted);
}
li[data-state="done"] .upgrade-state {
  color: var(--datalib-ok);
}
li[data-state="running"] .upgrade-state {
  color: var(--datalib-warn-fg);
  font-weight: 600;
}
li[data-state="failed"] .upgrade-state,
.upgrade-error {
  color: var(--datalib-error-fg);
}
li[data-state="failed"] .upgrade-state {
  font-weight: 600;
}
.upgrade-error {
  flex-basis: 100%;
  overflow-wrap: anywhere;
}
.upgrade-actions {
  display: flex;
  gap: 12px;
  margin-top: 8px;
  padding-top: 8px;
  border-top: 1px solid var(--datalib-border-soft);
}
.upgrade-link {
  padding: 0;
  border: none;
  background: none;
  color: var(--datalib-accent);
  font: inherit;
  cursor: pointer;
}
.upgrade-link:hover {
  text-decoration: underline;
}
</style>
