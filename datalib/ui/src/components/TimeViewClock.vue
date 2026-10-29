<script setup lang="ts">
// The clock in the status bar: the time now in the zone every date in
// the app is drawn in, or the T0 every date chip is drawn from, and the
// menu that changes it — the zone, how a day is written, a 12- or 24-hour
// clock, setting T0 by hand or clearing it. A click or a right-click opens
// it; the setting is `config/timeView.ts`.
import { computed, nextTick, onMounted, onUnmounted, ref, useTemplateRef } from "vue";
import {
  LOCAL,
  UTC,
  WRITTEN,
  allZones,
  formatAt,
  inputOfInstant,
  instantOfInput,
  millisText,
  offsetAt,
  offsetText,
  recentZones,
  searchZones,
  setT0,
  setZone,
  timeView,
  zoneName,
  zonePlace,
  type DayStyle,
  type HourCycle,
  type TimeView,
  type Zone,
} from "@/config/timeView";

const now = ref(Date.now());
let timer: ReturnType<typeof setTimeout> | undefined;
function tick() {
  now.value = Date.now();
  timer = setTimeout(tick, 60_000 - (now.value % 60_000) + 50);
}
onMounted(tick);
onUnmounted(() => clearTimeout(timer));

const machineZone = Intl.DateTimeFormat().resolvedOptions().timeZone;

const at = computed(() => ({ ms: now.value, offsetMin: null }));
const clock = computed(() => formatAt(at.value, timeView.value, { clock: true }));

function zoneWord(zone: Zone): string {
  if (zone.kind === "local") return zoneName(at.value, LOCAL);
  if (zone.kind === "written") return "as written";
  return zone.name === "UTC" ? "UTC" : zonePlace(zone.name);
}

/** T0 to the millisecond in the view's zone, where one is set. */
const t0Text = computed(() => {
  const t0 = timeView.value.t0;
  return t0 ? millisText(t0, timeView.value.zone) : null;
});

const face = computed(() => {
  const z = timeView.value.zone;
  if (t0Text.value)
    return `T0=${t0Text.value} ${z.kind === "written" ? "as written" : zoneWord(z)}`;
  return z.kind === "written" ? "As written" : `${clock.value} ${zoneWord(z)}`;
});
const faceTitle = computed(() => {
  if (t0Text.value) {
    return "Every date is shown as its distance from T0, earlier ones negative. Click to change or clear T0, or change the zone.";
  }
  const z = timeView.value.zone;
  const where =
    z.kind === "local"
      ? `this computer's zone (${machineZone})`
      : z.kind === "written"
        ? "the zone each date was written in"
        : z.name;
  return `Dates are shown in ${where}. Click to change the zone or the format.`;
});

const open = ref(false);
const anchor = ref({ right: 0, bottom: 0 });
const button = useTemplateRef<HTMLButtonElement>("button");
const search = ref("");
const searchBox = useTemplateRef<HTMLInputElement>("searchBox");

async function show() {
  const r = button.value?.getBoundingClientRect();
  if (r)
    anchor.value = { right: window.innerWidth - r.right, bottom: window.innerHeight - r.top + 4 };
  search.value = "";
  picking.value = false;
  open.value = true;
  await nextTick();
  searchBox.value?.focus();
}

function close() {
  open.value = false;
}

function onKey(ev: KeyboardEvent) {
  if (open.value && ev.key === "Escape") close();
}
onMounted(() => window.addEventListener("keydown", onKey));
onUnmounted(() => window.removeEventListener("keydown", onKey));

const zoneKey = (z: Zone) => (z.kind === "named" ? z.name : z.kind);
const isZone = (z: Zone) => zoneKey(timeView.value.zone) === zoneKey(z);

function pickZone(zone: Zone) {
  setZone(zone);
  close();
}

function clearT0() {
  setT0(null);
  close();
}

// Setting T0 by hand: a date and time to the millisecond, read in the
// zone picked beside it. Changing the zone keeps the clock as typed, so
// it names a different instant.
const picking = ref(false);
const pickedText = ref("");
const pickedZone = ref("local");
const t0Input = useTemplateRef<HTMLInputElement>("t0Input");
const zoneOfKey = (key: string): Zone => (key === "local" ? LOCAL : { kind: "named", name: key });
const picked = computed(() => instantOfInput(pickedText.value, zoneOfKey(pickedZone.value)));

async function startPicking() {
  const z = timeView.value.zone;
  pickedZone.value = z.kind === "named" ? z.name : "local";
  const from = timeView.value.t0 ?? { ms: Date.now(), offsetMin: null };
  pickedText.value = inputOfInstant(from, zoneOfKey(pickedZone.value));
  picking.value = true;
  await nextTick();
  t0Input.value?.focus();
}

function setPickedT0() {
  if (!picked.value) return;
  setT0(picked.value);
  close();
}

function pick<K extends "days" | "hours">(key: K, value: TimeView[K]) {
  timeView.value = { ...timeView.value, [key]: value };
  close();
}

const offsetNow = (zone: Zone) => offsetText(offsetAt(at.value, zone));

const recent = computed(() =>
  recentZones.value.map((name) => ({ name, offset: offsetNow({ kind: "named", name }) })),
);

const zoneList = allZones();
const found = computed(() => searchZones(search.value, zoneList, now.value));

function pickFirstFound() {
  const first = found.value[0];
  if (first) pickZone({ kind: "named", name: first.name });
}

/** A fixed instant to show what each format looks like. */
const SAMPLE = { ms: Date.UTC(2026, 3, 15, 14, 5), offsetMin: null };
const sampleOf = (days: DayStyle, hours: HourCycle) =>
  formatAt(SAMPLE, { zone: UTC, days, hours }, { day: "short", clock: true });
</script>

<template>
  <div class="time-view">
    <button
      ref="button"
      class="time-view-clock"
      :class="{ 'is-elsewhere': timeView.zone.kind !== 'local' || !!timeView.t0 }"
      :title="faceTitle"
      aria-haspopup="menu"
      :aria-expanded="open"
      data-testid="time-view-clock"
      @click="show"
      @contextmenu.prevent="show"
    >
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <circle cx="12" cy="12" r="8.5" />
        <path d="M12 7.5V12l3 2" />
      </svg>
      <span>{{ face }}</span>
    </button>
    <button
      v-if="timeView.t0"
      class="time-view-clear"
      title="Clear T0: show every date as a date again"
      aria-label="Clear T0"
      data-testid="time-view-clear-t0"
      @click="clearT0"
    >
      ×
    </button>
    <Teleport v-if="open" to="body">
      <div class="tv-overlay" @click="close" @contextmenu.prevent="close">
        <div
          class="tv-menu"
          role="menu"
          aria-label="How dates are shown"
          :style="{ right: `${anchor.right}px`, bottom: `${anchor.bottom}px` }"
          @click.stop
          @contextmenu.stop
        >
          <div class="tv-heading">T0</div>
          <form
            v-if="picking"
            class="tv-t0-form"
            data-testid="time-view-t0-form"
            @submit.prevent="setPickedT0"
          >
            <input
              ref="t0Input"
              v-model="pickedText"
              class="tv-field"
              type="datetime-local"
              step="0.001"
              aria-label="T0 date and time"
            />
            <select v-model="pickedZone" class="tv-field" aria-label="T0 time zone">
              <option value="local">This computer ({{ zonePlace(machineZone) }})</option>
              <option value="UTC">UTC</option>
              <optgroup v-if="recent.length" label="Recent">
                <option v-for="z in recent" :key="z.name" :value="z.name">
                  {{ zonePlace(z.name) }} {{ z.offset }}
                </option>
              </optgroup>
              <optgroup label="Every zone">
                <option v-for="name in zoneList" :key="name" :value="name">
                  {{ name.replace(/_/g, " ") }}
                </option>
              </optgroup>
            </select>
            <div class="tv-t0-actions">
              <button type="button" class="tv-button" @click="picking = false">Cancel</button>
              <button type="submit" class="tv-button is-primary" :disabled="!picked">Set T0</button>
            </div>
          </form>
          <div
            v-else
            class="tv-item"
            role="menuitem"
            data-testid="time-view-pick-t0"
            @click="startPicking"
          >
            <span class="tv-check" />
            <span class="tv-label">{{ t0Text ? "Change T0…" : "Set T0…" }}</span>
            <span v-if="!t0Text" class="tv-detail">or right-click a date</span>
          </div>
          <div
            v-if="t0Text"
            class="tv-item"
            role="menuitem"
            data-testid="time-view-menu-clear-t0"
            @click="clearT0"
          >
            <span class="tv-check" />
            <span class="tv-label">Clear T0</span>
            <span class="tv-detail">{{ t0Text }}</span>
          </div>
          <div class="tv-divider" />
          <div class="tv-heading">Time zone</div>
          <div
            class="tv-item"
            role="menuitemradio"
            :aria-checked="isZone(LOCAL)"
            @click="pickZone(LOCAL)"
          >
            <span class="tv-check">{{ isZone(LOCAL) ? "✓" : "" }}</span>
            <span class="tv-label">This computer</span>
            <span class="tv-detail">{{ zonePlace(machineZone) }} {{ offsetNow(LOCAL) }}</span>
          </div>
          <div
            class="tv-item"
            role="menuitemradio"
            :aria-checked="isZone(WRITTEN)"
            title="Each date in the zone its source wrote it in"
            @click="pickZone(WRITTEN)"
          >
            <span class="tv-check">{{ isZone(WRITTEN) ? "✓" : "" }}</span>
            <span class="tv-label">As written</span>
            <span class="tv-detail">each date's own zone</span>
          </div>
          <div
            class="tv-item"
            role="menuitemradio"
            :aria-checked="isZone(UTC)"
            @click="pickZone(UTC)"
          >
            <span class="tv-check">{{ isZone(UTC) ? "✓" : "" }}</span>
            <span class="tv-label">UTC</span>
            <span class="tv-detail">+00:00</span>
          </div>
          <div
            v-for="z in recent"
            :key="z.name"
            class="tv-item"
            role="menuitemradio"
            :aria-checked="isZone({ kind: 'named', name: z.name })"
            :title="z.name"
            @click="pickZone({ kind: 'named', name: z.name })"
          >
            <span class="tv-check">{{ isZone({ kind: "named", name: z.name }) ? "✓" : "" }}</span>
            <span class="tv-label">{{ zonePlace(z.name) }}</span>
            <span class="tv-detail">{{ z.offset }}</span>
          </div>
          <input
            ref="searchBox"
            v-model="search"
            class="tv-search"
            type="search"
            placeholder="Find a zone: city or +05:30"
            aria-label="Find a time zone"
            @keydown.enter.prevent="pickFirstFound"
          />
          <div
            v-for="z in found"
            :key="z.name"
            class="tv-item"
            role="menuitem"
            @click="pickZone({ kind: 'named', name: z.name })"
          >
            <span class="tv-check" />
            <span class="tv-label">{{ z.name.replace(/_/g, " ") }}</span>
            <span class="tv-detail">{{ z.offset }}</span>
          </div>
          <div v-if="search.trim() && !found.length" class="tv-none">No zone matches</div>
          <div class="tv-divider" />
          <div class="tv-heading">Dates</div>
          <div
            v-for="d in ['locale', 'iso'] as const"
            :key="d"
            class="tv-item"
            role="menuitemradio"
            :aria-checked="timeView.days === d"
            @click="pick('days', d)"
          >
            <span class="tv-check">{{ timeView.days === d ? "✓" : "" }}</span>
            <span class="tv-label">{{ sampleOf(d, timeView.hours) }}</span>
          </div>
          <div class="tv-divider" />
          <div class="tv-heading">Clock</div>
          <div
            v-for="h in ['h23', 'h12'] as const"
            :key="h"
            class="tv-item"
            role="menuitemradio"
            :aria-checked="timeView.hours === h"
            @click="pick('hours', h)"
          >
            <span class="tv-check">{{ timeView.hours === h ? "✓" : "" }}</span>
            <span class="tv-label">{{ h === "h23" ? "24-hour" : "12-hour" }}</span>
            <span class="tv-detail">{{
              formatAt(SAMPLE, { zone: UTC, days: "iso", hours: h }, { clock: true })
            }}</span>
          </div>
        </div>
      </div>
    </Teleport>
  </div>
</template>

<style scoped>
.time-view {
  flex: 0 0 auto;
  display: flex;
}
.time-view-clock {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: 5px;
  height: calc(var(--datalib-control-h) - 6px);
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface);
  color: var(--datalib-fg);
  cursor: pointer;
  font: inherit;
  font-variant-numeric: tabular-nums;
  padding: 0 8px;
  white-space: nowrap;
}
.time-view-clock:hover,
.time-view-clear:hover {
  background: var(--datalib-hover);
}
/* Beside the clock while T0 is set: one click back to dates. */
.time-view-clear {
  flex: 0 0 auto;
  height: calc(var(--datalib-control-h) - 6px);
  margin-left: 2px;
  padding: 0 6px;
  border: 1px solid var(--datalib-accent);
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface);
  color: var(--datalib-accent);
  cursor: pointer;
  font: inherit;
}
/* Not this computer's zone: every date on screen reads differently from
   the clock on the wall, so the button says so. */
.time-view-clock.is-elsewhere {
  border-color: var(--datalib-accent);
  color: var(--datalib-accent);
}
.time-view-clock svg {
  width: var(--datalib-icon-size);
  height: var(--datalib-icon-size);
  fill: none;
  stroke: currentColor;
  stroke-width: 2;
  stroke-linecap: round;
  stroke-linejoin: round;
}
.tv-overlay {
  position: fixed;
  inset: 0;
  z-index: 1500;
}
.tv-menu {
  position: fixed;
  min-width: 260px;
  max-width: 340px;
  max-height: calc(100vh - 60px);
  overflow-y: auto;
  padding: 4px 0;
  background: var(--datalib-input-bg, #fff);
  color: var(--datalib-fg, #000);
  border: 1px solid var(--datalib-border, #ccc);
  border-radius: var(--datalib-radius, 6px);
  box-shadow: 0 2px 10px rgb(0 0 0 / 20%);
  font-size: 13px;
}
.tv-heading {
  padding: 6px 14px 2px;
  color: var(--datalib-muted);
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: 0.04em;
}
.tv-item {
  display: flex;
  align-items: baseline;
  gap: 6px;
  padding: 5px 14px 5px 8px;
  cursor: pointer;
  user-select: none;
  white-space: nowrap;
}
.tv-item:hover {
  background: var(--datalib-hover);
}
.tv-check {
  width: 14px;
  flex: 0 0 auto;
  text-align: center;
}
.tv-label {
  flex: 1 1 auto;
  overflow: hidden;
  text-overflow: ellipsis;
}
.tv-detail {
  flex: 0 0 auto;
  color: var(--datalib-muted);
  font-variant-numeric: tabular-nums;
}
.tv-search {
  display: block;
  box-sizing: border-box;
  width: calc(100% - 16px);
  margin: 4px 8px;
  padding: 4px 6px;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface);
  color: var(--datalib-fg);
  font: inherit;
}
.tv-t0-form {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 4px 8px;
}
.tv-field {
  box-sizing: border-box;
  width: 100%;
  padding: 4px 6px;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface);
  color: var(--datalib-fg);
  font: inherit;
  font-variant-numeric: tabular-nums;
}
.tv-t0-actions {
  display: flex;
  justify-content: flex-end;
  gap: 6px;
}
.tv-button {
  padding: 3px 10px;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface);
  color: var(--datalib-fg);
  cursor: pointer;
  font: inherit;
}
.tv-button.is-primary {
  border-color: var(--datalib-accent);
  color: var(--datalib-accent);
}
.tv-button:disabled {
  opacity: 0.5;
  cursor: default;
}
.tv-none {
  padding: 4px 14px 4px 28px;
  color: var(--datalib-muted);
}
.tv-divider {
  height: 1px;
  margin: 4px 0;
  background: var(--datalib-border);
}
</style>
