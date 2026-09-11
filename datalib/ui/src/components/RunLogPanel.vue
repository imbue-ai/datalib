<script setup lang="ts">
// One run's log, as a grid: every line the run store holds for it,
// sortable and filterable by AG Grid, appended to as the run goes.
//
// The tail is a cursor, not a stream: the store assigns each line a
// monotone `seq`, and each `dag_changed` frame (the runner touched the
// store) asks for the lines after the last one seen. A run that has
// finished is read once.
import { computed, onMounted, onUnmounted, ref, shallowRef } from "vue";
import { AgGridVue } from "ag-grid-vue3";
import {
  ModuleRegistry,
  AllCommunityModule,
  themeQuartz,
  colorSchemeVariable,
  type CellClassParams,
  type ColDef,
  type GridApi,
  type GridReadyEvent,
  type ITooltipParams,
  type ValueFormatterParams,
} from "ag-grid-community";
import { fetchRunLog, type RunLogLine } from "@/api";
import { subscribeLive } from "@/live";
import { compareStamps, formatStamp, formatTimeOfDay } from "@/config/timeFormat";

ModuleRegistry.registerModules([AllCommunityModule]);
const gridTheme = themeQuartz.withPart(colorSchemeVariable);

const props = defineProps<{
  runId: string;
  /// The step the panel opens narrowed to. Cleared from the panel to see
  /// the whole run.
  step: string | null;
  /// Whether the run may still be writing: tail while true.
  live: boolean;
}>();

const stepOnly = ref(true);
const lines = shallowRef<RunLogLine[]>([]);
const busy = ref(false);
const error = ref<string | null>(null);
/// The newest `seq` in the grid, which the next fetch resumes after.
let lastSeq = 0;
let gridApi: GridApi<RunLogLine> | null = null;
let unsubscribe: (() => void) | null = null;
let inflight = false;

/// The filter as sent to the server: the step while narrowed, none once
/// widened. Widening restarts from the beginning, because the lines the
/// narrow view skipped are exactly the ones wanted.
function stepFilter(): string | undefined {
  return stepOnly.value && props.step ? props.step : undefined;
}

async function load(fresh: boolean) {
  if (inflight) return;
  inflight = true;
  if (fresh) {
    lastSeq = 0;
    lines.value = [];
    busy.value = true;
  }
  error.value = null;
  try {
    const got = await fetchRunLog(props.runId, { step: stepFilter(), afterSeq: lastSeq });
    if (got.length > 0) {
      lastSeq = got[got.length - 1].seq;
      lines.value = fresh ? got : [...lines.value, ...got];
      if (!fresh && gridApi) {
        gridApi.applyTransaction({ add: got });
        // Follow the tail only while the reader is already at it: a
        // scroll up to read something must not be yanked back down.
        const last = gridApi.getDisplayedRowCount() - 1;
        if (atBottom) gridApi.ensureIndexVisible(last, "bottom");
      }
    }
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busy.value = false;
    inflight = false;
  }
}

let atBottom = true;
function onBodyScrollEnd() {
  if (!gridApi) return;
  const last = gridApi.getDisplayedRowCount() - 1;
  const lastVisible = gridApi.getLastDisplayedRowIndex();
  atBottom = last < 0 || lastVisible >= last - 1;
}

function toggleScope() {
  stepOnly.value = !stepOnly.value;
  void load(true);
}

function levelClass(p: CellClassParams<RunLogLine>): string {
  const l = p.data?.level;
  return l === "error" ? "rl-error" : l === "warn" ? "rl-warn" : "";
}

const columnDefs = computed((): ColDef<RunLogLine>[] => [
  {
    headerName: "Time",
    field: "ts",
    width: 110,
    // The time of day to the millisecond, in the viewer's zone (a
    // step's own lines are stamped in UTC, the runner's in local time);
    // the date is in the tooltip, since every line of one run shares it.
    valueFormatter: (p: ValueFormatterParams<RunLogLine>) =>
      formatTimeOfDay(p.value ? String(p.value) : null),
    tooltipValueGetter: (p: ITooltipParams<RunLogLine>) =>
      p.value ? formatStamp(String(p.value)) : "",
    comparator: compareStamps,
    sortable: true,
    filter: false,
  },
  {
    headerName: "Step",
    field: "step",
    width: 180,
    hide: stepOnly.value && !!props.step,
    filter: true,
  },
  { headerName: "Level", field: "level", width: 80, filter: true, cellClass: levelClass },
  { headerName: "Stream", field: "stream", width: 84, filter: true },
  { headerName: "Thread", field: "thread", width: 150, filter: true },
  { headerName: "Target", field: "target", width: 200, filter: true },
  {
    headerName: "Message",
    field: "msg",
    flex: 1,
    minWidth: 320,
    filter: true,
    wrapText: false,
    cellClass: levelClass,
    tooltipField: "msg",
  },
  {
    headerName: "Fields",
    field: "fields",
    width: 220,
    filter: true,
    tooltipField: "fields",
  },
]);

const defaultColDef: ColDef<RunLogLine> = {
  resizable: true,
  sortable: true,
  suppressHeaderMenuButton: false,
};

function onGridReady(e: GridReadyEvent<RunLogLine>) {
  gridApi = e.api;
}

function onQuickFilter(ev: Event) {
  gridApi?.setGridOption("quickFilterText", (ev.target as HTMLInputElement).value);
}

onMounted(() => {
  void load(true);
  if (props.live) {
    unsubscribe = subscribeLive({
      root: (e) => {
        if (e.kind === "dag_changed") void load(false);
      },
      resync: () => void load(false),
    });
  }
});

onUnmounted(() => {
  unsubscribe?.();
  unsubscribe = null;
});
</script>

<template>
  <div class="rl-panel">
    <div class="rl-bar">
      <input
        class="rl-search"
        type="search"
        placeholder="Search the log…"
        aria-label="Search the log"
        @input="onQuickFilter"
      />
      <button v-if="props.step" class="m2-btn" @click="toggleScope">
        {{ stepOnly ? "Show the whole run" : "Only this step" }}
      </button>
      <span class="rl-count">
        {{ lines.length }} line{{ lines.length === 1 ? "" : "s" }}
        <span v-if="props.live"> · following</span>
      </span>
    </div>
    <p v-if="error" class="rl-note bad">{{ error }}</p>
    <p v-else-if="busy" class="rl-note">Reading the run store…</p>
    <p v-else-if="lines.length === 0" class="rl-note">
      Nothing logged yet for this run<span v-if="stepOnly && props.step"> by this step</span>.
    </p>
    <AgGridVue
      v-show="lines.length > 0"
      class="rl-grid"
      :theme="gridTheme"
      :columnDefs="columnDefs"
      :defaultColDef="defaultColDef"
      :rowData="lines"
      :getRowId="(p: { data: RunLogLine }) => String(p.data.seq)"
      :tooltipShowDelay="300"
      :rowHeight="24"
      :headerHeight="30"
      :enableCellTextSelection="true"
      :suppressCellFocus="true"
      @grid-ready="onGridReady"
      @body-scroll-end="onBodyScrollEnd"
    />
  </div>
</template>

<style scoped>
.rl-panel {
  display: flex;
  flex-direction: column;
  flex: 1 1 auto;
  min-height: 0;
}
.rl-bar {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 16px;
  border-bottom: 1px solid var(--datalib-border);
  flex: 0 0 auto;
}
.rl-search {
  flex: 1 1 auto;
  min-width: 0;
  padding: 4px 8px;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
  background: var(--datalib-bg);
  color: inherit;
  font: inherit;
  font-size: 13px;
}
.rl-count {
  font-size: 12px;
  color: var(--datalib-muted);
  white-space: nowrap;
}
.rl-note {
  margin: 0;
  padding: 16px;
  font-size: 13px;
  color: var(--datalib-muted);
}
.rl-note.bad {
  color: var(--datalib-log-error);
}
.rl-grid {
  flex: 1 1 auto;
  min-height: 320px;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12px;
}
</style>

<style>
/* Cell classes are set by the grid, so they can't be scoped. */
.rl-grid .rl-warn {
  color: var(--datalib-log-warn);
}
.rl-grid .rl-error {
  color: var(--datalib-log-error);
}
</style>
