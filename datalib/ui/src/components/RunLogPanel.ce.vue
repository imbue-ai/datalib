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
import { computed, nextTick, onMounted, onUnmounted, ref } from "vue";
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
import { filterToken, replaceToken, tokenValue, withToken } from "@/grid/query";
import { KEEP_COLUMN_WIDTHS } from "@/grid/columnLayout";
import { menuSlots, type MenuEntry } from "@/grid/menu";
// The column rules and cell helpers every slickgrid here shares.
import "@/cards/tableGrid.css";
import {
  fetchLog,
  fetchProcesses,
  fetchRuns,
  healthSnapshot,
  type ProcessInfo,
  type RunInfo,
  type RunLogLine,
} from "@/api";
import {
  fieldsWithoutSource,
  SOURCE_DEFAULT_REF,
  sourceLabel,
  sourceOf,
  sourceUrl,
} from "./runLogSource";
import { page as thisPage } from "@/telemetry";
import { changed, subscribeLive } from "@/live";
import { compareStamps, formatDateTime, formatRelative } from "@/config/timeFormat";

const props = defineProps<{
  /// The run the panel opens on, or `*` for every run.
  runId: string;
  /// The step the panel opens on: its newest attempt's process, once
  /// the run has one, else its lines by name.
  step: string | null;
  /// A launch of the server to open on instead of a run — the one
  /// serving the page, for its own log.
  launchId?: string | null;
  /// What the query bar starts with — `process:http` for the server's
  /// log. Editable like anything typed there.
  initialQuery?: string;
  /// Open scrolled to the line that says how the step ended — the
  /// runner writes its error as the step's last error-level line, a
  /// stop as its last warning — and keep that line marked.
  jumpToEnd?: boolean;
}>();

/// What the panel is showing, for the caller's header.
export type LogScope =
  | { kind: "all" }
  | { kind: "run"; run: RunInfo; process: ProcessInfo | null }
  | { kind: "launch"; launch: ProcessInfo };

const emit = defineEmits<{
  /// The pickers moved, so the caller can say what is on screen.
  (e: "scope-changed", scope: LogScope): void;
  /// A line was selected — by a click, or the arrow keys moving on —
  /// for the caller to open in full.
  (e: "line-selected", seq: number): void;
}>();

/// The picker's "every run" entry. Not a run id: the store's ids are
/// UUIDs, and the job ids that double as run ids are too.
const ALL_RUNS = "*";
const LAUNCH_PREFIX = "launch:";

/// The first picker: a run, a launch of the server or a page of the
/// app, or everything. A run's value is its id; a launch's or a page's
/// is prefixed, since all are UUIDs.
const picked = ref(props.launchId ? `${LAUNCH_PREFIX}${props.launchId}` : props.runId);
const allRuns = computed(() => picked.value === ALL_RUNS);
const launchId = computed(() =>
  picked.value.startsWith(LAUNCH_PREFIX) ? picked.value.slice(LAUNCH_PREFIX.length) : null,
);
const runId = computed(() => (allRuns.value || launchId.value ? null : picked.value));
/// The second picker, within a run: one of its processes — the runner
/// or a step's attempt — or the whole run (`null`).
const processId = ref<string | null>(null);
/// The runs the picker offers: the ones this step took part in, newest
/// first, or every recent run when the panel is not about one step.
const runs = ref<RunInfo[]>([]);
/// The server's launches, newest first.
const launches = ref<ProcessInfo[]>([]);
/// The app's pages — one per load in a tab — newest first.
const pages = ref<ProcessInfo[]>([]);
/// The launch or page picked, when one is.
const launch = computed(() =>
  launchId.value
    ? ([...launches.value, ...pages.value].find((x) => x.process_id === launchId.value) ?? null)
    : null,
);
/// The processes of the run on screen: its runner and its steps'
/// attempts, newest first.
const runProcesses = ref<ProcessInfo[]>([]);
const currentProcess = computed(
  () => runProcesses.value.find((p) => p.process_id === processId.value) ?? null,
);
/// Whether what is on screen may still be writing — tail while it
/// may: by whether the store has closed it, and until the lists say,
/// a run or launch is taken as still going (tailing a finished one
/// costs nothing).
const live = computed(() => {
  if (launchId.value) return !launch.value || launch.value.finished_at_utc == null;
  if (allRuns.value) return runs.value.some((x) => x.finished_at_utc == null);
  if (currentProcess.value) return currentProcess.value.finished_at_utc == null;
  const r = runs.value.find((x) => x.run_id === runId.value);
  return !r || r.finished_at_utc == null;
});
/// The levels, quietest first — the order the picker offers and the
/// order `min_level:` ranks.
const LEVELS = ["trace", "debug", "info", "warn", "error"] as const;
const MIN_LEVEL_KEY = "min_level";
const DEFAULT_QUERY = `${MIN_LEVEL_KEY}:info`;
/// The query bar. Sent to the server as typed; a change reloads from
/// the start, since the lines it drops are exactly the ones wanted back.
/// Starts at `info` and above: a step logs its commits and batches at
/// `debug`, which is there when asked for and noise otherwise.
const query = ref(props.initialQuery ?? DEFAULT_QUERY);
/// What the picker shows: the query's own `min_level:` word, so typing
/// one and picking one are the same thing; `trace` when there is none.
const minLevel = computed(() => tokenValue(query.value, MIN_LEVEL_KEY) ?? "trace");

function pickLevel(ev: Event) {
  const level = (ev.target as HTMLSelectElement).value;
  const token = level === "trace" ? null : `${MIN_LEVEL_KEY}:${level}`;
  setQuery(replaceToken(query.value, MIN_LEVEL_KEY, token));
}
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
/// A load asked for while another was in flight — a query typed while
/// the tail was appending, or a line logged while a press held the tail
/// back — runs once that one is done, instead of being lost.
let freshPending = false;
let tailPending = false;
/// Set from a press on the grid until just after its release. Any
/// change in the row count makes the bundle re-render every row
/// (`grid.invalidate()`), and a release that lands on a re-rendered row
/// fires no click on the old one: on a busy log, a click selected
/// nothing and a right-click opened no menu. So the grid is not
/// touched while a button is down.
let pressed: Promise<void> | null = null;
let release: (() => void) | null = null;

function onGridPointerDown() {
  if (pressed) return;
  pressed = new Promise<void>((resolve) => {
    release = resolve;
  });
}

function onPointerUp() {
  const done = release;
  if (!done) return;
  pressed = null;
  release = null;
  // A task later, so the click or context menu the release fires has
  // been handled before the rows are rebuilt.
  setTimeout(done);
}
/// The `seq` of the line the panel opened on, which its cells mark.
let jumpedTo: number | null = null;

async function load(fresh: boolean) {
  if (inflight) {
    if (fresh) freshPending = true;
    else tailPending = true;
    return;
  }
  inflight = true;
  if (fresh) {
    lastSeq = 0;
    lineCount.value = 0;
    busy.value = true;
  }
  error.value = null;
  try {
    // A step's attempt is shown by subject — what came out of it and
    // what the runner said about it; a launch or the runner by author.
    // A step opened before its first attempt started has no process to
    // pick yet; its lines are still its own by name.
    const attempt = currentProcess.value?.step ? currentProcess.value : null;
    const byName = runId.value && !processId.value && props.step && !runProcesses.value.length;
    const got = await fetchLog({
      run: runId.value ?? undefined,
      process: launchId.value ?? (attempt ? undefined : (processId.value ?? undefined)),
      step: attempt?.step ?? (byName ? props.step! : undefined),
      attempt: attempt?.attempt ?? undefined,
      q: query.value,
      afterSeq: lastSeq,
    });
    while (pressed) await pressed;
    if (got.length > 0) {
      lastSeq = got[got.length - 1].seq;
      lineCount.value += got.length;
      // The box is shown once there is a count; the grid must be built
      // or resized after that paint, not before it.
      await nextTick();
      if (!bundle) {
        createGrid(got);
        if (props.jumpToEnd) jumpToEnd(got);
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
    if (freshPending) {
      freshPending = false;
      tailPending = false;
      void load(true);
    } else if (tailPending) {
      tailPending = false;
      void load(false);
    }
  }
}

/// The line that says how the step ended: the one the runner wrote at
/// finish, else the last warning or error. Scrolled to on the first
/// paint, with a few lines of what led up to it above.
function jumpToEnd(lines: RunLogLine[]) {
  const isFinish = (l: RunLogLine) => !!l.fields && /"finished"/.test(l.fields);
  const isProblem = (l: RunLogLine) => l.level === "error" || l.level === "warn";
  const target = [...lines].reverse().find(isFinish) ?? [...lines].reverse().find(isProblem);
  if (!target || !bundle) return;
  jumpedTo = target.seq;
  const row = bundle.dataView.getRowById(target.seq);
  if (row == null) return;
  const grid = bundle.slickGrid;
  const vp = grid.getViewportNode();
  const visible = vp ? Math.floor(vp.clientHeight / ROW_HEIGHT) : 10;
  grid.scrollRowToTop(Math.max(0, row - Math.floor(visible / 2)));
  grid.invalidateRow(row);
  grid.render();
}

let atBottom = true;
function onScroll(_e: unknown, args: { grid: SlickGrid }) {
  const vp = args.grid.getViewportNode();
  if (!vp) return;
  atBottom = vp.scrollTop + vp.clientHeight >= vp.scrollHeight - 2 * ROW_HEIGHT;
}

function setQuery(q: string) {
  query.value = q;
  void load(true);
}

/// A token added from outside — the inspector's keep / exclude.
function addToken(token: string) {
  setQuery(withToken(query.value, token));
}

defineExpose({ addToken });

/// Typing waits for a pause; a token from the menu applies at once.
function onQueryInput(ev: Event) {
  const q = (ev.target as HTMLInputElement).value;
  if (queryTimer) clearTimeout(queryTimer);
  queryTimer = setTimeout(() => setQuery(q), 250);
}

async function loadRuns() {
  try {
    [runs.value, launches.value, pages.value] = await Promise.all([
      fetchRuns({ step: props.step ?? undefined, limit: 30 }),
      fetchProcesses({ process: "http", limit: 20 }),
      fetchProcesses({ process: "ui", limit: 20 }),
    ]);
  } catch {
    // The pickers are a convenience; the opened run still shows.
  }
}

/// The run's processes, and — on a run just picked, or opened on a
/// step — which of them to show: the step's newest attempt.
async function loadProcesses(pickStep: string | null) {
  if (!runId.value) {
    runProcesses.value = [];
    return;
  }
  try {
    runProcesses.value = await fetchProcesses({ run: runId.value, limit: 1000 });
  } catch {
    runProcesses.value = [];
  }
  if (pickStep) {
    processId.value = runProcesses.value.find((p) => p.step === pickStep)?.process_id ?? null;
  } else if (processId.value && !currentProcess.value) {
    processId.value = null;
  }
}

function announce() {
  if (launchId.value) {
    if (launch.value) emit("scope-changed", { kind: "launch", launch: launch.value });
    return;
  }
  const run = runs.value.find((r) => r.run_id === runId.value);
  emit(
    "scope-changed",
    run ? { kind: "run", run, process: currentProcess.value } : { kind: "all" },
  );
}

async function pickScope(ev: Event) {
  picked.value = (ev.target as HTMLSelectElement).value;
  processId.value = null;
  await loadProcesses(props.step);
  announce();
  void load(true);
}

function pickProcess(ev: Event) {
  const v = (ev.target as HTMLSelectElement).value;
  processId.value = v === "" ? null : v;
  announce();
  void load(true);
}

/// How a process reads in its picker: which step and attempt, or the
/// runner, and how it ended.
function processLabel(p: ProcessInfo): string {
  const who = p.step ? `${p.step} · attempt ${p.attempt ?? "?"}` : "the runner";
  return `${who} · ${processEnd(p)}`;
}

function processEnd(p: ProcessInfo): string {
  if (p.finished_at_utc == null) return "running";
  if (p.signal != null) return `ended by signal ${p.signal}`;
  if (p.exit_code != null) return p.exit_code === 0 ? "exited 0" : `exited ${p.exit_code}`;
  return "finished";
}

/// How a launch reads: when it started, and whether it is the server
/// serving this page.
function launchLabel(l: ProcessInfo): string {
  const when = formatRelative(l.started_at_utc, Date.now());
  const state = l.finished_at_utc == null ? "running" : "ended";
  const mine = l.process_id === healthSnapshot()?.process_id ? " · this server" : "";
  return `server started ${when} · ${state}${mine}`;
}

/// How a page reads: when it opened, whether it is still open, and
/// whether it is the one this panel is on.
function pageLabel(p: ProcessInfo): string {
  const when = formatRelative(p.started_at_utc, Date.now());
  const state = p.finished_at_utc == null ? "open" : "closed";
  const mine = p.process_id === thisPage.process_id ? " · this page" : "";
  return `page opened ${when} · ${state}${mine}`;
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
  const level = l === "error" ? "rl-error" : l === "warn" ? "rl-warn" : "";
  return line && line.seq === jumpedTo ? `${level} rl-jumped` : level;
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

/// The line's structured fields, less the two the Source column shows.
const otherFields: Formatter<RunLogLine> = (_r, _c, value, _col, line) => {
  const text = fieldsWithoutSource(value == null ? null : String(value));
  return { text, toolTip: text, addClasses: levelClass(line) };
};

/// The commit's first ten characters; the whole hash on hover.
const commitShort: Formatter<RunLogLine> = (_r, _c, value, _col, line) => ({
  text: value ? String(value).slice(0, 10) : "",
  toolTip: value ? String(value) : "",
  addClasses: levelClass(line),
});

/// What a `rl-clip-left` cell holds: an isolated left-to-right run, so
/// the cell's right-to-left direction clips the text's start and does
/// not reorder it (see the class's CSS).
function clippedFromLeft(text: string): HTMLElement {
  const el = document.createElement("span");
  el.className = "rl-ltr";
  el.textContent = text;
  return el;
}

const dateTime: Formatter<RunLogLine> = (_r, _c, value) => {
  const text = formatDateTime(value ? String(value) : null);
  return { html: clippedFromLeft(text), toolTip: text };
};

const runIdShort: Formatter<RunLogLine> = (_r, _c, value) => ({
  text: shortRunId(String(value ?? "")),
  toolTip: String(value ?? ""),
});

/// `file:line`, as a link to that line on GitHub at the process's
/// commit when one is known, else at `main`; text only for a file
/// outside the repo. Clipped from the left like Time: the file's name
/// and the line tell the lines apart, the directories are the same for
/// most.
const source: Formatter<RunLogLine> = (_r, _c, _value, _col, line) => {
  const src = sourceOf(line?.fields);
  if (!src) return { text: "", toolTip: "", addClasses: levelClass(line) };
  const shown = sourceLabel(src);
  const commit = line?.git_hash ?? null;
  const href = sourceUrl(commit, src);
  if (!href) return { html: clippedFromLeft(shown), toolTip: shown, addClasses: levelClass(line) };
  const a = document.createElement("a");
  a.className = "rl-source rl-ltr";
  a.textContent = shown;
  a.href = href;
  a.target = "_blank";
  a.rel = "noopener";
  const at = commit ? commit.slice(0, 10) : `${SOURCE_DEFAULT_REF} (commit unknown)`;
  return { html: a, toolTip: `${shown} at ${at}`, addClasses: levelClass(line) };
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

/// A column the drag-to-group bar accepts. Only a column carrying
/// `grouping` can be dropped there, which is how Time and Fields (one of
/// a kind per line: a group per row) stay out of it.
function groupable(name: string, field: keyof RunLogLine) {
  return { grouping: { getter: field, formatter: groupTitle(name), collapsed: false } };
}

function buildColumns(): Column<RunLogLine>[] {
  // `col-id` on every header and cell, for a test to find a column by.
  return columnSet().map((c) => ({
    ...c,
    cellAttrs: { "col-id": String(c.id) },
    headerCellAttrs: { "col-id": String(c.id) },
  }));
}

/// Every column the log has, in the order they sit. The seven a reader
/// wants on every line are shown; the rest — which run, which process,
/// which commit, which thread, which module — say the same thing on
/// line after line of one process's log, so they start hidden. The
/// grid menu puts any of them back, and the line opened beside the
/// grid carries them all whether or not their column is up.
function columnSet(): Column<RunLogLine>[] {
  return [
    {
      id: "ts_utc",
      name: "Time",
      field: "ts_utc",
      width: 120,
      // The whole stamp to the millisecond, in the viewer's zone (a
      // step's own lines are stamped in UTC, the runner's in local time).
      // Clipped from the left, and 120px shows the time of day: the
      // seconds and milliseconds are what tell one line from the next,
      // the date is the same for most; widen the column for it.
      cssClass: "rl-clip-left",
      formatter: dateTime,
      sortable: true,
      sortComparer: (a, b, dir) => compareStamps(a, b) * (dir ?? 1),
    },
    {
      id: "run_id",
      name: "Run",
      field: "run_id",
      width: 100,
      hidden: true,
      formatter: runIdShort,
      sortable: true,
      ...groupable("Run", "run_id"),
    },
    {
      id: "process",
      name: "Process",
      field: "process",
      width: 90,
      hidden: true,
      formatter: plain,
      sortable: true,
      ...groupable("Process", "process"),
    },
    {
      id: "git_hash",
      name: "Commit",
      field: "git_hash",
      width: 100,
      hidden: true,
      formatter: commitShort,
      sortable: true,
      ...groupable("Commit", "git_hash"),
    },
    {
      id: "step",
      name: "Step",
      field: "step",
      width: 100,
      formatter: plain,
      sortable: true,
      ...groupable("Step", "step"),
    },
    {
      id: "level",
      name: "Level",
      field: "level",
      width: 80,
      formatter: plain,
      sortable: true,
      ...groupable("Level", "level"),
    },
    {
      id: "stream",
      name: "Stream",
      field: "stream",
      width: 84,
      formatter: plain,
      sortable: true,
      ...groupable("Stream", "stream"),
    },
    {
      id: "thread",
      name: "Thread",
      field: "thread",
      width: 150,
      hidden: true,
      formatter: plain,
      sortable: true,
      ...groupable("Thread", "thread"),
    },
    {
      id: "target",
      name: "Target",
      field: "target",
      width: 200,
      hidden: true,
      formatter: plain,
      sortable: true,
      ...groupable("Target", "target"),
    },
    {
      id: "source",
      name: "Source",
      // The value is read out of `fields`; the column has no field of its
      // own, and the id is what the header and the test find it by.
      field: "fields",
      width: 120,
      cssClass: "rl-clip-left",
      formatter: source,
      sortable: false,
    },
    {
      id: "msg",
      name: "Message",
      field: "msg",
      width: 600,
      formatter: plain,
      sortable: true,
      ...groupable("Message", "msg"),
    },
    {
      id: "fields",
      name: "Fields",
      field: "fields",
      width: 220,
      formatter: otherFields,
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
  git_hash: "commit",
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
    Column<RunLogLine> | undefined;
  const line = (args.dataContext ?? args.grid.getDataItem(args.row ?? -1)) as
    RunLogLine | undefined;
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
    // A row selects on click and the arrow keys move the selection;
    // the line opens in full beside the card either way.
    enableCellNavigation: true,
    enableSelection: true,
    multiSelect: false,
    selectionOptions: { selectActiveRow: true },
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
    ...KEEP_COLUMN_WIDTHS,
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
      dropPlaceHolderText: "Drag a column here to group the lines by it — Step, Level, Stream",
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
    // The five columns `columnSet` starts hidden are put back from
    // here, the way the Explore grid's are.
    enableGridMenu: true,
    enableColumnPicker: true,
    enableContextMenu: true,
    contextMenu: {
      commandItems: menuSlots(4, menuEntries),
    },
  };
}

let groupingPlugin: SlickDraggableGrouping | null = null;

function createGrid(first: RunLogLine[]) {
  if (bundle || !boxEl.value) return;
  const options = gridOptions();
  // Inside a card the grid's own stylesheet — row heights, column
  // widths — has to land in the shadow root, or the rows have no
  // height.
  const root = boxEl.value.getRootNode();
  if (root instanceof ShadowRoot) options.shadowRoot = root;
  const b = new SlickVanillaGridBundle<RunLogLine>(
    boxEl.value,
    buildColumns(),
    options,
    first,
  ) as Grid;
  bundle = b;
  b.slickGrid.onScroll.subscribe(onScroll);
  b.slickGrid.onSelectedRowsChanged.subscribe((_e, args) => {
    const row = args.rows[args.rows.length - 1];
    if (row == null) return;
    const line = b.dataView.getItem(row) as RunLogLine | undefined;
    // A group row selects nothing.
    if (line && typeof line.seq === "number") emit("line-selected", line.seq);
  });
  // What the bar's drop does, without the mouse, for the e2e tests:
  // a drag dispatched by hand dies inside SortableJS under load, and
  // the grid card exposes the same thing as `__fwGridApi.groupBy`.
  (window as unknown as { __fwRunLogApi?: unknown }).__fwRunLogApi = {
    groupBy: (ids: string[]) => groupingPlugin?.setDroppedGroups(ids),
  };
}

/// The app's theme is an attribute on `<html>`; the grid's is an option.
let themeWatch: MutationObserver | null = null;

onMounted(async () => {
  // On the window, so a release outside the grid still ends the press.
  window.addEventListener("pointerup", onPointerUp, true);
  window.addEventListener("pointercancel", onPointerUp, true);
  // The run's processes first, so a step opens on its attempt rather
  // than on the run and then jumps; the pickers' lists with them, so
  // the header can say what opened.
  await Promise.all([loadProcesses(props.step), loadRuns()]);
  announce();
  void load(true);
  unsubscribe = subscribeLive({
    root: (e) => {
      if (changed(e, "log") && live.value) void load(false);
      // A step's new attempt is a new process for the picker to offer.
      if (changed(e, "runs")) void loadProcesses(null);
    },
    resync: () => {
      void loadRuns();
      void loadProcesses(null);
      if (live.value) void load(false);
    },
  });
  themeWatch = new MutationObserver(() => bundle?.setDarkMode(isDark()));
  themeWatch.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ["data-theme"],
  });
});

onUnmounted(() => {
  window.removeEventListener("pointerup", onPointerUp, true);
  window.removeEventListener("pointercancel", onPointerUp, true);
  onPointerUp();
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
      <label class="rl-level">
        at least
        <select
          class="rl-run"
          :value="minLevel"
          aria-label="Lowest level to show"
          @change="pickLevel"
        >
          <option v-for="l in LEVELS" :key="l" :value="l">{{ l }}</option>
        </select>
      </label>
      <select class="rl-run" :value="picked" aria-label="Which run or launch" @change="pickScope">
        <optgroup v-if="runs.length" label="Runs">
          <option v-for="r in runs" :key="r.run_id" :value="r.run_id">
            {{ runLabel(r) }}
          </option>
        </optgroup>
        <optgroup v-if="launches.length" label="The server">
          <option v-for="l in launches" :key="l.process_id" :value="LAUNCH_PREFIX + l.process_id">
            {{ launchLabel(l) }}
          </option>
        </optgroup>
        <optgroup v-if="pages.length" label="Pages of the app">
          <option v-for="p in pages" :key="p.process_id" :value="LAUNCH_PREFIX + p.process_id">
            {{ pageLabel(p) }}
          </option>
        </optgroup>
        <option :value="ALL_RUNS">everything the store holds</option>
      </select>
      <select
        v-if="runId && runProcesses.length"
        class="rl-run"
        :value="processId ?? ''"
        aria-label="Which process of the run"
        @change="pickProcess"
      >
        <option value="">the whole run</option>
        <option v-for="p in runProcesses" :key="p.process_id" :value="p.process_id">
          {{ processLabel(p) }}
        </option>
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
        Nothing logged yet<span v-if="launch?.process === 'ui'"> by this page</span
        ><span v-else-if="launchId"> by this server</span
        ><span v-else-if="currentProcess"> by this process</span
        ><span v-else-if="runId"> for this run</span>.
      </template>
    </p>
    <div v-show="lineCount > 0" class="rl-grid" @pointerdown="onGridPointerDown">
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
  min-width: 180px;
  padding: 4px 8px;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
  background: var(--datalib-bg);
  color: inherit;
  font: inherit;
  font-size: 13px;
}
.rl-run {
  max-width: 28vw;
  text-overflow: ellipsis;
  padding: 4px 8px;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
  background: var(--datalib-bg);
  color: inherit;
  font: inherit;
  font-size: 13px;
}
.rl-level {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--datalib-muted);
  white-space: nowrap;
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
   runs right-to-left, so the ellipsis lands on the left. The text
   itself sits in an isolated left-to-right run (`.rl-ltr`, what the
   cell's formatter emits): without the isolate the bidi algorithm
   reads "2026-09-22 14:07:47.190" as two numbers in a right-to-left
   line and draws the time before the date. */
.rl-grid .slick-cell.rl-clip-left {
  direction: rtl;
  text-align: left;
}
.rl-grid .rl-ltr {
  direction: ltr;
  unicode-bidi: isolate;
}
.rl-grid .slick-cell.rl-warn {
  color: var(--datalib-log-warn);
}
.rl-grid .slick-cell.rl-error {
  color: var(--datalib-log-error);
}
/* The line the panel opened on: the one that says how the step ended. */
.rl-grid .slick-cell.rl-jumped {
  background: color-mix(in srgb, var(--datalib-log-error) 14%, transparent);
  font-weight: 600;
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
