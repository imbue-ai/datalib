<script setup lang="ts">
// One run's log, as a grid: every line the run store holds for it,
// sortable and groupable, appended to as the run goes — and
// a picker for the other runs the step took part in, since "what did
// it do last time" is the question right after "what is it doing". The
// picker's last entry is every run at once, with a column saying which.
// The same grid, opened on every run with `process:http` typed in,
// is the server's own log: the store holds that too.
//
// The search box is the same query bar the unified grid has —
// `level:warn -target:sqlx "history"` — read by the server, so a query
// is a string a person can keep, and right-click on a cell adds a
// token to it the same way there.
//
// The tail is a cursor, not a stream: the store assigns each line a
// monotone `seq`, and each `log` frame (someone wrote a line — the
// runner, or the server itself) asks for the lines after the last one
// seen. A run that has finished is read once.
//
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from "vue";
import { SlickVanillaGridBundle } from "@slickgrid-universal/vanilla-bundle";
import type {
  Column,
  Formatter,
  GridOption,
  GroupingFormatterItem,
  MenuFromCellCallbackArgs,
  SlickDraggableGrouping,
  SlickGrid,
} from "@slickgrid-universal/common";
import { filterToken, withToken } from "@/grid/query";
import { menuSlots, type MenuEntry } from "@/grid/menu";
// The column rules and cell helpers every slickgrid here shares.
import "@/cards/tableGrid.css";
import { fetchLog, fetchRuns, type RunInfo, type RunLogLine } from "@/api";
import { sourceLabel, sourceOf, sourceUrl } from "./runLogSource";
import { changed, subscribeLive } from "@/live";
import {
  compareStamps,
  formatRelative,
  formatStamp,
  formatTimeOfDay,
} from "@/config/timeFormat";

const props = defineProps<{
  /// The run the panel opens on.
  runId: string;
  /// The step the panel opens narrowed to. Cleared from the panel to see
  /// the whole run.
  step: string | null;
  /// Whether that run may still be writing: tail while true.
  live: boolean;
  /// What the query bar starts with — `process:http` for the server's
  /// log. Editable like anything typed there.
  initialQuery?: string;
}>();

const emit = defineEmits<{
  /// The picker moved to another run, so the caller can say which;
  /// `null` when it moved to every run at once.
  (e: "run-changed", run: RunInfo | null): void;
}>();

/// The picker's "every run" entry. Not a run id: the store's ids are
/// UUIDs, and the job ids that double as run ids are too.
const ALL_RUNS = "*";

/// The run on screen; starts as the one opened, moves with the picker.
const runId = ref(props.runId);
const allRuns = computed(() => runId.value === ALL_RUNS);
/// The runs the picker offers: the ones this step took part in, newest
/// first, or every recent run when the panel is not about one step.
const runs = ref<RunInfo[]>([]);
/// Whether the run on screen may still be writing. The opened run says
/// so by prop; a picked one by whether the store has closed it.
const live = computed(() => {
  if (runId.value === props.runId) return props.live;
  if (allRuns.value) return props.live || runs.value.some((x) => x.finished_at_utc == null);
  const r = runs.value.find((x) => x.run_id === runId.value);
  return !!r && r.finished_at_utc == null;
});

const stepOnly = ref(true);
/// The query bar. Sent to the server as typed; a change reloads from
/// the start, since the lines it drops are exactly the ones wanted back.
const query = ref(props.initialQuery ?? "");
let queryTimer: ReturnType<typeof setTimeout> | null = null;
/// How many lines the grid holds: the dataset lives in the grid, and a
/// tail appends there rather than replacing it (see `load`).
const lineCount = ref(0);
const busy = ref(false);
const error = ref<string | null>(null);
/// The newest `seq` in the grid, which the next fetch resumes after.
let lastSeq = 0;
const boxEl = ref<HTMLDivElement | null>(null);
// The bundle types its grid and view as optional because they can be
// asked for before `init`; here neither is handed out before both exist.
type Grid = SlickVanillaGridBundle<RunLogLine> & {
  dataView: NonNullable<SlickVanillaGridBundle<RunLogLine>["dataView"]>;
  slickGrid: NonNullable<SlickVanillaGridBundle<RunLogLine>["slickGrid"]>;
};
/// Created on the first line and kept from then on, hidden while a
/// reload leaves nothing to show: a grid created inside a hidden box
/// measures no width and fits its columns to that.
let bundle: Grid | null = null;
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
    lineCount.value = 0;
    busy.value = true;
  }
  error.value = null;
  try {
    const got = await fetchLog({
      run: allRuns.value ? undefined : runId.value,
      step: stepFilter(),
      q: query.value,
      afterSeq: lastSeq,
    });
    if (got.length > 0) {
      lastSeq = got[got.length - 1].seq;
      lineCount.value += got.length;
      // The box is shown once there is a count; the grid must be built
      // or resized after that paint, not before it.
      await nextTick();
      if (!bundle) {
        createGrid(got);
      } else if (fresh) {
        bundle.dataset = got;
      } else {
        // Appended through the grid rather than as a new dataset, which
        // would re-render every row and lose the scroll.
        bundle.gridService.addItems(got, {
          position: "bottom",
          highlightRow: false,
          scrollRowIntoView: false,
          resortGrid: true,
          triggerEvent: false,
        });
        // Follow the tail only while the reader is already at it: a
        // scroll up to read something must not be yanked back down.
        if (atBottom) {
          const grid = bundle.slickGrid;
          grid.scrollRowIntoView(grid.getDataLength() - 1);
        }
      }
    } else if (fresh && bundle) {
      bundle.dataset = [];
    }
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busy.value = false;
    inflight = false;
  }
}

let atBottom = true;
function onScroll(_e: unknown, args: { grid: SlickGrid }) {
  const vp = args.grid.getViewportNode();
  if (!vp) return;
  atBottom = vp.scrollTop + vp.clientHeight >= vp.scrollHeight - 2 * ROW_HEIGHT;
}

function toggleScope() {
  stepOnly.value = !stepOnly.value;
  void load(true);
}

function setQuery(q: string) {
  query.value = q;
  void load(true);
}

/// Typing waits for a pause; a token from the menu applies at once.
function onQueryInput(ev: Event) {
  const q = (ev.target as HTMLInputElement).value;
  if (queryTimer) clearTimeout(queryTimer);
  queryTimer = setTimeout(() => setQuery(q), 250);
}

async function loadRuns() {
  try {
    runs.value = await fetchRuns({ step: props.step ?? undefined, limit: 30 });
    // A run's lines link to source at the run's commit, which arrives
    // here; lines drawn before it did are drawn again.
    bundle?.slickGrid.invalidateAllRows();
    bundle?.slickGrid.render();
  } catch {
    // The picker is a convenience; the opened run still shows.
  }
}

function pickRun(ev: Event) {
  runId.value = (ev.target as HTMLSelectElement).value;
  if (allRuns.value) {
    // Every run is only meaningful for one step.
    stepOnly.value = true;
    emit("run-changed", null);
  } else {
    const picked = runs.value.find((r) => r.run_id === runId.value);
    if (picked) emit("run-changed", picked);
  }
  void load(true);
}

/// The run id as the column shows it: the first block of the UUID, which
/// is what someone reads off the header to tell two runs apart. The whole
/// id is in the tooltip.
function shortRunId(id: string): string {
  return id.split("-")[0] ?? id;
}

/// How a run reads in the picker: when it started, and whether it is
/// still going — the id itself is in the header for whoever needs it.
function runLabel(r: RunInfo): string {
  const when = formatRelative(r.started_at_utc, Date.now());
  return r.finished_at_utc == null ? `${when} · running` : when;
}

function levelClass(line: RunLogLine | undefined): string {
  const l = line?.level;
  return l === "error" ? "rl-error" : l === "warn" ? "rl-warn" : "";
}

const ROW_HEIGHT = 24;

/// A cell's text, with the row's level colour and the whole value on
/// hover. Text, never HTML: `enableHtmlRendering` is off below, so a
/// log line that contains markup is shown as the characters it is.
const plain: Formatter<RunLogLine> = (_r, _c, value, _col, line) => ({
  text: value == null ? "" : String(value),
  toolTip: value == null ? "" : String(value),
  addClasses: levelClass(line),
});

const timeOfDay: Formatter<RunLogLine> = (_r, _c, value) => ({
  text: formatTimeOfDay(value ? String(value) : null),
  toolTip: value ? formatStamp(String(value)) : "",
});

const runIdShort: Formatter<RunLogLine> = (_r, _c, value) => ({
  text: shortRunId(String(value ?? "")),
  toolTip: String(value ?? ""),
});

/// The commit a line's source is relative to: the line's own for a
/// process's line, the run's for a run's, when either was known.
function commitOf(line: RunLogLine): string | null {
  if (line.git_hash) return line.git_hash;
  if (line.run_id) return runs.value.find((r) => r.run_id === line.run_id)?.git_hash ?? null;
  return null;
}

/// `file:line`, as a link to that line on GitHub at the right commit
/// when one is known, else as text. Clipped from the left like Time:
/// the file's name and the line tell the lines apart, the directories
/// are the same for most.
const source: Formatter<RunLogLine> = (_r, _c, _value, _col, line) => {
  const src = sourceOf(line?.fields);
  if (!src) return { text: "", toolTip: "", addClasses: levelClass(line) };
  const shown = sourceLabel(src);
  const commit = line ? commitOf(line) : null;
  const href = commit && sourceUrl(commit, src);
  if (!commit || !href) return { text: shown, toolTip: shown, addClasses: levelClass(line) };
  const a = document.createElement("a");
  a.className = "rl-source";
  a.textContent = shown;
  a.href = href;
  a.target = "_blank";
  a.rel = "noopener";
  return { html: a, toolTip: `${shown} at ${commit.slice(0, 10)}`, addClasses: levelClass(line) };
};

/// What a group row says: the column, its value and how many lines
/// share it. An element rather than a string for the reason `plain`
/// gives.
function groupTitle(name: string) {
  return (g: GroupingFormatterItem) => {
    const el = document.createElement("span");
    el.className = "rl-group";
    el.textContent = `${name}: ${g.value === "" || g.value == null ? "—" : String(g.value)}`;
    const count = document.createElement("span");
    count.className = "rl-group-count";
    count.textContent = ` (${g.count})`;
    el.appendChild(count);
    return el as unknown as string;
  };
}

/// A column that keeps its width when the grid fits itself to the
/// panel: the fit pass shrinks a column down to `minWidth`, and these
/// are already as narrow as they read. Message, the one flexible
/// column, is the one that gives.
function fixed(width: number) {
  return { width, minWidth: width };
}

/// A column the drag-to-group bar accepts. Only a column carrying
/// `grouping` can be dropped there, which is how Time and Fields (one of
/// a kind per line: a group per row) stay out of it.
function groupable(name: string, field: keyof RunLogLine) {
  return { grouping: { getter: field, formatter: groupTitle(name), collapsed: false } };
}

watch([allRuns, stepOnly], () => {
  if (bundle) bundle.columnDefinitions = buildColumns();
});

function buildColumns(): Column<RunLogLine>[] {
  // `col-id` on every header and cell, for a test to find a column by.
  return columnSet().map((c) => ({
    ...c,
    cellAttrs: { "col-id": String(c.id) },
    headerCellAttrs: { "col-id": String(c.id) },
  }));
}

function columnSet(): Column<RunLogLine>[] {
  return [
  {
    id: "ts_utc",
    name: "Time",
    field: "ts_utc",
    ...fixed(110),
    // The time of day to the millisecond, in the viewer's zone (a
    // step's own lines are stamped in UTC, the runner's in local time);
    // the date is in the tooltip, since every line of one run shares it.
    // Clipped from the left: the seconds and milliseconds are what tell
    // one line from the next, the hour is the same for all.
    cssClass: "rl-clip-left",
    formatter: timeOfDay,
    sortable: true,
    sortComparer: (a, b, dir) => compareStamps(a, b) * (dir ?? 1),
  },
  {
    id: "run_id",
    name: "Run",
    field: "run_id",
    ...fixed(100),
    hidden: !allRuns.value,
    formatter: runIdShort,
    sortable: true,
    ...groupable("Run", "run_id"),
  },
  {
    id: "process",
    name: "Process",
    field: "process",
    ...fixed(90),
    // One step's lines are all the runner's; the column says something
    // only once the server's can be in the grid too.
    hidden: stepOnly.value && !!props.step,
    formatter: plain,
    sortable: true,
    ...groupable("Process", "process"),
  },
  {
    id: "step",
    name: "Step",
    field: "step",
    ...fixed(180),
    hidden: stepOnly.value && !!props.step,
    formatter: plain,
    sortable: true,
    ...groupable("Step", "step"),
  },
  {
    id: "level",
    name: "Level",
    field: "level",
    ...fixed(80),
    formatter: plain,
    sortable: true,
    ...groupable("Level", "level"),
  },
  {
    id: "stream",
    name: "Stream",
    field: "stream",
    ...fixed(84),
    formatter: plain,
    sortable: true,
    ...groupable("Stream", "stream"),
  },
  {
    id: "thread",
    name: "Thread",
    field: "thread",
    ...fixed(150),
    formatter: plain,
    sortable: true,
    ...groupable("Thread", "thread"),
  },
  {
    id: "target",
    name: "Target",
    field: "target",
    ...fixed(200),
    formatter: plain,
    sortable: true,
    ...groupable("Target", "target"),
  },
  {
    id: "msg",
    name: "Message",
    field: "msg",
    width: 600,
    minWidth: 320,
    formatter: plain,
    sortable: true,
    ...groupable("Message", "msg"),
  },
  {
    id: "source",
    name: "Source",
    // The value is read out of `fields`; the column has no field of its
    // own, and the id is what the header and the test find it by.
    field: "fields",
    ...fixed(180),
    cssClass: "rl-clip-left",
    formatter: source,
    sortable: false,
  },
  {
    id: "fields",
    name: "Fields",
    field: "fields",
    ...fixed(220),
    formatter: plain,
    sortable: true,
  },
  ];
}

// Right-click on a cell: keep only the lines sharing its value, or drop
// them — a token on the query bar, as in the unified grid. The key is the
// column's name in the log's vocabulary (`datalib_runs::query::KEYS`);
// a column with none, like Time, offers nothing.
const QUERY_KEYS: Partial<Record<keyof RunLogLine, string>> = {
  run_id: "run",
  process: "process",
  step: "step",
  level: "level",
  stream: "stream",
  target: "target",
  thread: "thread",
  msg: "msg",
};

/// The cell under the right-click, as the menu needs it: the query key
/// for its column, the raw value and the value as shown. Null when the
/// column has no key or the cell is empty.
function cellUnderMenu(args: MenuFromCellCallbackArgs): {
  header: string;
  key: string;
  value: string;
  shown: string;
} | null {
  // `onBeforeMenuShow` is handed the cell's coordinates and nothing
  // else; the command callbacks get the column and the row as well.
  const column = (args.column ?? args.grid.getColumns()[args.cell ?? -1]) as
    | Column<RunLogLine>
    | undefined;
  const line = (args.dataContext ?? args.grid.getDataItem(args.row ?? -1)) as
    | RunLogLine
    | undefined;
  const field = column?.field as keyof RunLogLine | undefined;
  const key = field && QUERY_KEYS[field];
  if (!column || !line || !field || !key) return null;
  const raw = line[field];
  if (raw == null || raw === "") return null;
  const value = String(raw);
  const shown = field === "run_id" ? shortRunId(value) : value;
  return { header: String(column.name ?? key), key, value, shown };
}

/// The menu for the cell under the right-click. Read on each opening,
/// since the entries name the cell's value.
function menuEntries(args: MenuFromCellCallbackArgs): MenuEntry[] {
  const cell = cellUnderMenu(args);
  const entries: MenuEntry[] = [];
  if (cell) {
    entries.push(
      {
        name: `Keep only ${cell.header}=${cell.shown}`,
        action: () => setQuery(withToken(query.value, filterToken(cell.key, cell.value, false))),
      },
      {
        name: `Exclude all ${cell.header}=${cell.shown}`,
        action: () => setQuery(withToken(query.value, filterToken(cell.key, cell.value, true))),
      },
    );
  }
  if (query.value.trim()) {
    if (entries.length) entries.push({ name: "", separator: true });
    entries.push({ name: "Clear the query", action: () => setQuery("") });
  }
  return entries;
}

function isDark(): boolean {
  return document.documentElement.dataset.theme === "dark";
}

function gridOptions(): GridOption {
  return {
    datasetIdPropertyName: "seq",
    // Cells and group rows are text (see `plain`), never markup.
    enableHtmlRendering: false,
    enableCellNavigation: false,
    enableTextSelectionOnCells: true,
    enableAutoTooltip: false,
    enableEmptyDataWarningMessage: false,
    multiColumnSort: false,
    rowHeight: ROW_HEIGHT,
    headerRowHeight: 30,
    darkMode: isDark(),
    // The grid fills its container, whatever the panel's size, rather
    // than measuring the window: the panel is a dialog over the page.
    enableAutoResize: true,
    autoResize: {
      // The frame around the box, not the box: the resizer sizes the box
      // to what it measures, and a box it also measured would then stop
      // following the panel.
      container: boxEl.value!.parentElement!,
      calculateAvailableSizeBy: "container",
      resizeDetection: "container",
      autoHeight: false,
      bottomPadding: 0,
      minHeight: 200,
    },
    // Grouping by run, by process, by level, by target — the questions a
    // log answers once it holds more than one run. Groups open expanded:
    // the point is to organise the lines, not to hide them, and the counts
    // on the group rows read the same either way.
    enableGrouping: true,
    enableDraggableGrouping: true,
    createPreHeaderPanel: true,
    showPreHeaderPanel: true,
    preHeaderPanelHeight: 30,
    draggableGrouping: {
      dropPlaceHolderText:
        "Drag a column here to group the lines by it — Run, Process, Level, Target",
      hideToggleAllButton: false,
      toggleAllButtonText: "Expand / collapse all",
      // The theme ships these icons but draws nothing for the plugin's
      // default classes; the chip's controls are invisible without them.
      deleteIconCssClass: "mdi mdi-close",
      sortAscIconCssClass: "mdi mdi-arrow-up",
      sortDescIconCssClass: "mdi mdi-arrow-down",
      onExtensionRegistered: (plugin) => {
        groupingPlugin = plugin;
      },
    },
    enableContextMenu: true,
    contextMenu: {
      commandItems: menuSlots(4, menuEntries),
    },
  };
}

let groupingPlugin: SlickDraggableGrouping | null = null;

function createGrid(first: RunLogLine[]) {
  if (bundle || !boxEl.value) return;
  const b = new SlickVanillaGridBundle<RunLogLine>(
    boxEl.value,
    buildColumns(),
    gridOptions(),
    first,
  ) as Grid;
  bundle = b;
  b.slickGrid.onScroll.subscribe(onScroll);
  // What the bar's drop does, without the mouse, for the e2e tests:
  // a drag dispatched by hand dies inside SortableJS under load, and
  // the grid card exposes the same thing as `__fwGridApi.groupBy`.
  (window as unknown as { __fwRunLogApi?: unknown }).__fwRunLogApi = {
    groupBy: (ids: string[]) => groupingPlugin?.setDroppedGroups(ids),
  };
}

/// The app's theme is an attribute on `<html>`; the grid's is an option.
let themeWatch: MutationObserver | null = null;

onMounted(() => {
  void load(true);
  void loadRuns();
  unsubscribe = subscribeLive({
    root: (e) => {
      if (changed(e, "log") && live.value) void load(false);
    },
    resync: () => {
      void loadRuns();
      if (live.value) void load(false);
    },
  });
  themeWatch = new MutationObserver(() => bundle?.setDarkMode(isDark()));
  themeWatch.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
});

onUnmounted(() => {
  unsubscribe?.();
  unsubscribe = null;
  themeWatch?.disconnect();
  themeWatch = null;
  bundle?.dispose();
  bundle = null;
  groupingPlugin = null;
  delete (window as unknown as { __fwRunLogApi?: unknown }).__fwRunLogApi;
});
</script>

<template>
  <div class="rl-panel">
    <div class="rl-bar">
      <input
        class="rl-search"
        type="search"
        placeholder='Search the log — words, or level:warn -target:sqlx "a phrase"'
        aria-label="Search the log"
        :value="query"
        @input="onQueryInput"
      />
      <button v-if="props.step && !allRuns" class="m2-btn" @click="toggleScope">
        {{ stepOnly ? "Show the whole run" : "Only this step" }}
      </button>
      <select
        v-if="runs.length > 1 || props.step"
        class="rl-run"
        :value="runId"
        aria-label="Which run"
        @change="pickRun"
      >
        <option v-for="r in runs" :key="r.run_id" :value="r.run_id">
          {{ runLabel(r) }}
        </option>
        <option v-if="props.step" :value="ALL_RUNS">every run</option>
      </select>
      <span class="rl-count">
        {{ lineCount }} line{{ lineCount === 1 ? "" : "s" }}
        <span v-if="live"> · following</span>
      </span>
    </div>
    <p v-if="error" class="rl-note bad">{{ error }}</p>
    <p v-else-if="busy" class="rl-note">Reading the run store…</p>
    <p v-else-if="lineCount === 0" class="rl-note">
      <template v-if="query.trim()">Nothing matches the query.</template>
      <template v-else>
        Nothing logged yet<span v-if="!allRuns"> for this run</span
        ><span v-if="stepOnly && props.step"> by this step</span>.
      </template>
    </p>
    <div v-show="lineCount > 0" class="rl-grid">
      <div ref="boxEl" class="rl-box" />
    </div>
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
.rl-run {
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
  min-width: 0;
  position: relative;
}
.rl-box {
  position: absolute;
  inset: 0;
}
</style>

<style>
/* Cell classes are set by the grid, so they can't be scoped. */
/* Overflow hides the start of the text rather than its end: the cell
   runs right-to-left, so the ellipsis lands on the left. A time of day
   is digits and separators only, which the bidi algorithm keeps as one
   left-to-right run, so the text itself is unchanged. */
.rl-grid .slick-cell.rl-clip-left {
  direction: rtl;
  text-align: left;
}
.rl-grid .slick-cell.rl-warn {
  color: var(--datalib-log-warn);
}
.rl-grid .slick-cell.rl-error {
  color: var(--datalib-log-error);
}
.rl-grid .rl-group-count {
  color: var(--datalib-muted);
}
.rl-grid .rl-source {
  color: inherit;
  text-decoration: underline dotted;
}
.rl-grid .slick-cell {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12px;
}
</style>
