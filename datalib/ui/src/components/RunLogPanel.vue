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
// The grid is slickgrid-vue (MIT), not AG Grid: this panel is the
// spike for replacing the enterprise modules, and the drag-to-group
// bar, the header menu and the right-click menu all come from it.
import { computed, onMounted, onUnmounted, ref, shallowRef, watch } from "vue";
import {
  SlickgridVue,
  type Column,
  type Formatter,
  type GridOption,
  type GroupingFormatterItem,
  type MenuCommandItem,
  type MenuFromCellCallbackArgs,
  type SlickGrid,
  type SlickgridVueInstance,
} from "slickgrid-vue";
import { filterToken, withToken } from "@/grid/query";
import { fetchLog, fetchRuns, type RunInfo, type RunLogLine } from "@/api";
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
/// The lines handed to the grid as its dataset. A fresh load replaces
/// it; a tail appends through the grid instead (see `load`), so this
/// holds the first page and `lineCount` the true total.
const lines = shallowRef<RunLogLine[]>([]);
const lineCount = ref(0);
/// The grid is created on the first line and kept from then on, hidden
/// while a reload leaves nothing to show: a grid created inside a
/// hidden box measures no width and fits its columns to that.
const gridWanted = ref(false);
const busy = ref(false);
const error = ref<string | null>(null);
/// The newest `seq` in the grid, which the next fetch resumes after.
let lastSeq = 0;
let vueGrid: SlickgridVueInstance | null = null;
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
      gridWanted.value = true;
      if (fresh || !vueGrid) {
        lines.value = fresh ? got : [...lines.value, ...got];
      } else {
        // Appended through the grid rather than by replacing `lines`:
        // a new dataset would re-render every row and lose the scroll.
        vueGrid.gridService.addItems(got, {
          position: "bottom",
          highlightRow: false,
          scrollRowIntoView: false,
          resortGrid: true,
          triggerEvent: false,
        });
        // Follow the tail only while the reader is already at it: a
        // scroll up to read something must not be yanked back down.
        if (atBottom) {
          const grid = vueGrid.slickGrid;
          grid.scrollRowIntoView(grid.getDataLength() - 1);
        }
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

/// The grid's own id in the page: the resizer finds its container by
/// selector, and the panel can be opened more than once per session.
const gridId = `rl-grid-${Math.random().toString(36).slice(2, 8)}`;

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

/// The grid owns this list too (a plugin can push a column into it),
/// so it is a ref the column set below is written into, not a computed.
const columns = shallowRef<Column<RunLogLine>[]>([]);
watch([allRuns, stepOnly], () => (columns.value = buildColumns()), { immediate: true });

function buildColumns(): Column<RunLogLine>[] {
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

/// A menu entry drawn like the built-in ones (icon slot, then text),
/// with a title that names the cell's value. The grid copies the
/// options it is given, so the title cannot be set on the item from
/// outside once the menu exists; a renderer is handed the cell instead.
function titled(label: (cell: NonNullable<ReturnType<typeof cellUnderMenu>>) => string) {
  return (_item: unknown, args: unknown): HTMLElement => {
    const cell = cellUnderMenu(args as MenuFromCellCallbackArgs);
    // The menu item lays its icon and text out itself; the wrapper only
    // exists because a renderer returns one element, so it takes no box.
    const li = document.createElement("div");
    li.style.display = "contents";
    const icon = document.createElement("div");
    icon.className = "slick-menu-icon";
    icon.textContent = "◦";
    const text = document.createElement("span");
    text.className = "slick-menu-content";
    text.textContent = cell ? label(cell) : "";
    li.append(icon, text);
    return li;
  };
}

const keepItem: MenuCommandItem = {
  command: "keep",
  slotRenderer: titled((c) => `Keep only ${c.header}=${c.shown}`),
  itemVisibilityOverride: (args) => cellUnderMenu(args as MenuFromCellCallbackArgs) !== null,
  action: (_e, args) => {
    const cell = cellUnderMenu(args as MenuFromCellCallbackArgs);
    if (cell) setQuery(withToken(query.value, filterToken(cell.key, cell.value, false)));
  },
};
const excludeItem: MenuCommandItem = {
  command: "exclude",
  slotRenderer: titled((c) => `Exclude all ${c.header}=${c.shown}`),
  itemVisibilityOverride: (args) => cellUnderMenu(args as MenuFromCellCallbackArgs) !== null,
  action: (_e, args) => {
    const cell = cellUnderMenu(args as MenuFromCellCallbackArgs);
    if (cell) setQuery(withToken(query.value, filterToken(cell.key, cell.value, true)));
  },
};
const clearItem: MenuCommandItem = {
  command: "clear-query",
  title: "Clear the query",
  itemVisibilityOverride: () => !!query.value.trim(),
  action: () => setQuery(""),
};

function isDark(): boolean {
  return document.documentElement.dataset.theme === "dark";
}

const gridOptions = shallowRef<GridOption>({
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
    container: `#${gridId}-box`,
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
    hideToggleAllButton: true,
    // The theme ships these icons but draws nothing for the plugin's
    // default classes; the chip's controls are invisible without them.
    deleteIconCssClass: "mdi mdi-close",
    sortAscIconCssClass: "mdi mdi-arrow-up",
    sortDescIconCssClass: "mdi mdi-arrow-down",
  },
  enableContextMenu: true,
  contextMenu: {
    commandItems: [keepItem, excludeItem, clearItem, "divider"],
  },
});

function onGridCreated(instance: SlickgridVueInstance) {
  vueGrid = instance;
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
  themeWatch = new MutationObserver(() => {
    vueGrid?.slickGrid.setOptions({ darkMode: isDark() });
  });
  themeWatch.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
});

onUnmounted(() => {
  unsubscribe?.();
  unsubscribe = null;
  themeWatch?.disconnect();
  themeWatch = null;
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
    <div v-show="lineCount > 0" :id="gridId + '-box'" class="rl-grid">
      <SlickgridVue
        v-if="gridWanted"
        :grid-id="gridId"
        v-model:columns="columns"
        v-model:options="gridOptions"
        v-model:dataset="lines"
        @onVueGridCreated="onGridCreated($event.detail)"
        @onScroll="onScroll($event.detail.eventData, $event.detail.args)"
      />
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
.rl-grid .slick-cell {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12px;
}
</style>
