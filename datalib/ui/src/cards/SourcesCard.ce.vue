<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { TOPIC_CONFIG_WRITTEN, type CardCtx } from "./types";
import type { Column } from "@slickgrid-universal/common";
import {
  fetchConfig,
  fetchConfigScaffold,
  saveConfig,
  fetchAllJobs,
  fetchManageRows,
  fetchRuns,
  healthSnapshot,
  fetchTreeHistory,
  enqueueJob,
  cancelJob,
  type ManageResponse,
  type ManageRow,
  type SyncJob,
  type JobProgressEvent,
  type ColumnSpec,
} from "@/api";
import {
  listGroups,
  listSteps,
  appendSource,
  buildDiffSource,
  removeSteps,
  describeGroup,
  renameGroup,
  replaceSteps,
  sourceStepsOf,
  unwireFromFanIns,
  wireIntoFanIns,
  paramsAreRepresentable,
  entryForStep,
  emptyTableDiagnosis,
  type ConfiguredGroup,
  type ConfiguredStep,
  type SourceSteps,
  type StepPhase,
} from "@/config/sourceSteps";
import TableGrid from "./TableGrid.ce.vue";
import type { TableGridApi } from "./tableGridApi";
import type { MenuEntry } from "@/grid/menu";
import { catalogForStep, type CatalogEntry } from "@/config/catalog";
import { ingestLabel } from "@/config/ingestMethods";
import { copyToClipboard } from "@/clipboard";
import { browseColumns, browseQuery } from "@/config/browsePresets";
import { logSource } from "./libs/logView";
import { pushToast } from "@/toasts";
import { historyRows, truncatedStores, type HistoryRow } from "@/config/commitHistory";
import { rowMenu, type MenuAction, type MenuTarget } from "@/config/rowMenu";
import { formatRelative, formatStamp } from "@/config/timeFormat";
import { changed, subscribeLive } from "@/live";
import SourceWizard from "@/components/SourceWizard.vue";
import CompareDialog from "@/components/CompareDialog.vue";

const props = defineProps<{ ctx: CardCtx }>();
import { isDesktopApp, revealActionLabel, revealInFileManager } from "@/desktop";

props.ctx.setTitle("Sources");
props.ctx.setHelp(`
<p>Every top-level row is a <b>group</b> <code>config.toml</code> declares: a source
(Work Slack, Personal mail), or the unified index that makes them searchable. Open
its chevron for the <b>steps</b> that do the work — fetch, render, index — and the
<b>applets</b> the app spawns to serve it. Actions that don’t apply to a kind are
disabled and say why.</p>
<p>A group row reads off its steps: <b>Status</b> is running if any step is, failed if
any failed, and otherwise the last step’s in pipeline order; while a sync is in
flight it draws one segment per step. <b>Last synced</b> is the fetch step’s.
<b>Sync</b> runs the group’s source steps and everything downstream;
<b>Remove</b> takes the steps and applets with it.</p>
<p><b>Type</b> and <b>Status</b> are icons, and the mark after a step’s name says what
it does — hover any of them for the word. <b>Double-click a Status</b> to read that
step's log — from the run in flight while it runs, else from the run it last took
part in, with a picker for its other runs — as a grid you can sort, filter and
search; on a group row, the log of the step its status came from.
<b>Activity</b> is what a running step has reported: how much is queued ahead of
it, what it has counted so far, and how many warnings and errors it has logged.</p>
<p><b>Browse</b> and <b>Sync</b> are buttons: they are the two things a row does often.
<b>Right-click a row</b> for everything it can do — browse, edit, reveal, remove, the
log, a rename (on the Name cell), and its <b>commit history</b>: every store under it
is versioned, and the panel lists each commit — when, what it said, what it did to
each table, and the run that made it — newest first, updating while a sync runs.
Right-click inside a selection and the menu acts on all of it; outside one, on that
row alone, without changing the selection. An entry that doesn’t apply stays, greyed,
and says why on hover. <b>Sync</b> stays a button: it is the one thing a row does
often.</p>
<p><b>Bytes on disk</b> is a directory walk over each row’s tree — a group’s is its
whole folder, measured on the same walk — plotted over the last few minutes and drawn
against the largest row, so a row’s height means its size, and its shape means what
that size has been doing. Hover for the total and the breakdown.</p>
<p><b>Last synced</b> and <b>Status</b> are per step, read from the runner’s own
record — so a sync you start from a terminal shows up here too. A run whose record
never closed and whose lock nobody holds reads as <b>interrupted</b>: it was
killed, not lost. A step a queued sync will reach reads as <b>queued</b>, and its
Sync button becomes a Stop — one job is one runner process over a whole subgraph,
so stopping is per sync, not per row.</p>
<p>The bar along the bottom of the app is the <b>whole data root</b>, not the sum of
the rows: it includes <code>system/</code> — the stores, the job logs, the served
attachments — and anything a deleted step left behind. The config itself is the
<b>config.toml</b> card; <b>Show the config</b> opens it beside this one.</p>
`);

/// The config editor, as a card beside this one.
function openConfig() {
  props.ctx.host.openCards("configView()");
}

const configText = ref("");
const configPath = ref("");
// Two independent verdicts on the config, and both matter.
const parseError = ref<string | null>(null);
const configError = ref<string | null>(null);
// What the backend's own loader made of the same file. Held so the
// empty state can cross-check itself against it — see
// `emptyTableDiagnosis`.
const serverSourceCount = ref(0);
const configExists = ref(false);
const loadError = ref<string | null>(null);
const banner = ref<{ ok: boolean; text: string } | null>(null);
// The job a banner is about, when it is about one. A job-scoped banner
// retires the moment that job stops running, not on the next action.
const bannerJob = ref<string | null>(null);

/// Put up a banner, optionally tying it to a job's lifetime.
function say(ok: boolean, text: string, jobId: string | null = null) {
  banner.value = { ok, text };
  bannerJob.value = jobId;
}

/// Take the banner down, and with it any job it was tied to.
function clearBanner() {
  banner.value = null;
  bannerJob.value = null;
}

/// Take down a job-scoped banner once its job has stopped running. A
/// job told to stop is still running until the worker says otherwise —
/// the "Stopping…" banner is *for* that window.
function retireBanner(job: SyncJob) {
  if (bannerJob.value !== job.id) return;
  if (job.active) return;
  clearBanner();
}
const busy = ref(false);
const jobs = ref<SyncJob[]>([]);
/// The rows, joined server-side from the config, the runner's record,
/// the run store, the queue and the usage sampler — see
/// `datalib/backend/http/src/manage/`. Bytes are measured by the backend
/// on a tick *while a sync is running*, not walked per request; between
/// runs nothing walks, which is why the two loads that matter ask for a
/// fresh one.
const manage = ref<ManageResponse | null>(null);
const storage = computed(() => manage.value?.storage ?? null);
/// The config's entries as the browser parses them, for the wizard —
/// which edits the text — and the catalog lookups in `decorate`.
const sources = ref<ConfiguredStep[]>([]);
/// The `[[groups]]` entries, for what a new source may not collide with
/// and for taking a group with its last step.
const configGroups = ref<ConfiguredGroup[]>([]);

// Resolved once — the desktop bridge either exists for this window or
// it doesn't, and the label depends only on the platform.
const canReveal = isDesktopApp();
const revealLabel = revealActionLabel();

const wizardOpen = ref(false);
/// Bumped on every opening, and bound to the dialog's `key`, so a
/// reopened dialog is a fresh mount rather than a reused component
/// still holding the last one's refs.
const wizardKey = ref(0);
/// The source the wizard is editing: its group, the catalog entry that
/// describes it, and whichever of its two steps the config has.
const editing = ref<{
  group: ConfiguredGroup;
  entry: CatalogEntry;
  steps: SourceSteps;
} | null>(null);

/// Non-null when the table is empty for a reason worth shouting about
/// rather than the ordinary "you haven't added anything yet".
const emptyDiagnosis = computed(() =>
  emptyTableDiagnosis({
    parsedCount: sources.value.length,
    serverSourceCount: serverSourceCount.value,
    textLength: configText.value.length,
    exists: configExists.value,
    path: configPath.value,
  }),
);

/// Ids already spoken for: every group, plus the written id of every
/// step outside a group. A custom step's id is reserved whole, not by
/// its first path segment: the loader allows a group `exports` beside a
/// custom `exports/csv` (their trees differ), and nothing here splits an
/// id — the cost is that the group's measured folder then counts the
/// custom tree too. An applet id lives in another namespace and may
/// coincide.
const takenIds = computed(
  () =>
    new Set([
      ...configGroups.value.map((g) => g.id),
      ...sources.value.filter((s) => s.kind === "step").map((s) => s.group ?? s.id),
    ]),
);

/// The render step that reads a given fetch step, if the config has
/// one — what deleting the fetch step has to take with it.
function renderSiblingOf(fetchId: string): ConfiguredStep | undefined {
  return sources.value.find(
    (s) => s.kind === "step" && s.inputs.includes(fetchId) && s.phase === "render",
  );
}

/// What a row stands for: a `[[groups]]` entry, or one of the two
/// kinds of entry filed under it. The server assembles the row
/// (`GET /api/manage/rows`), typed by the columns it declares; what is
/// added here is what needs the wizard's descriptors, which live in
/// the browser.
type Row = ManageRow & {
  /// Null when the wizard can edit this row; otherwise why not.
  editBlocked: string | null;
  /// The group whose form Edit opens: the row's own group, or for a
  /// step under one, that group. Null where there is no form.
  editGroup: string | null;
  /// The card source a Browse of this row opens, or null where the
  /// row's `browse` action says there is nothing to browse.
  browseSource: string | null;
};

/// The tree the grid shows, as the server assembled it, with the
/// wizard's knowledge added per row.
const rows = computed<Row[]>(() => (manage.value?.rows ?? []).map(decorate));

function decorate(r: ManageRow): Row {
  // `system/` is not a config entry: nothing to edit, and Browse is
  // the run log over every run.
  if (r.kind === "system") {
    return {
      ...r,
      editBlocked: "Not a config entry.",
      editGroup: null,
      browseSource: browseAction(r)?.enabled ? "logView()" : null,
    };
  }
  if (r.kind === "group") {
    const editBlocked = groupEditBlocked(r.id);
    return {
      ...r,
      editBlocked,
      editGroup: editBlocked ? null : r.id,
      browseSource: groupBrowse(r),
    };
  }
  // Edit: the wizard's one form describes a source — a group and its two
  // steps — so a step under a group edits through its group. Everything
  // else is hand-written config, and the honest answer is to say so.
  // Deliberately no dropped-entry override: editing is how the entry
  // gets fixed.
  let editBlocked: string | null;
  if (r.kind === "applet") {
    editBlocked = "No form for applets — edit this one in Advanced below.";
  } else if (r.phase === "index") {
    editBlocked = "A shared index step has no options — its inputs are its whole config.";
  } else if (r.written_group !== null) {
    // The written group, not the declared one: a step naming a group
    // the config lacks should hear that, not "outside any group".
    editBlocked = groupEditBlocked(r.written_group);
  } else {
    editBlocked = "No guided form for a step outside a group — edit it in Advanced below.";
  }
  // "Download" or "Import", read off the step's params against what its
  // provider declares; the server's "Ingest" only when they name no method.
  const ingestLabelled =
    r.group && r.phase === "ingest" ? ingestLabel(r.type?.id ?? null, r.params) : null;
  return {
    ...r,
    name: ingestLabelled ? { ...r.name, label: ingestLabelled } : r.name,
    editBlocked,
    editGroup: editBlocked ? null : r.group,
    browseSource: null,
  };
}

/// What a Browse of this group opens. Whether it can — the group has a
/// render step, and is in the pipeline — is the server's word, carried
/// by the row's `browse` action; this is only the card behind it. The
/// index group has no type: browsing it is the unified projection
/// across every source, which is the card the app already opens on.
function groupBrowse(g: ManageRow): string | null {
  if (!browseAction(g)?.enabled) return null;
  const type = g.type?.id ?? null;
  if (!type) return "gridView()";
  const columns = browseColumns(type);
  const args: string[] = [`q: ${JSON.stringify(browseQuery(g.id, type))}`];
  if (columns) args.push(`columns: ${JSON.stringify(columns)}`);
  return `gridView({ ${args.join(", ")} })`;
}

const browseAction = (r: ManageRow) => r.actions.find((a) => a.id === "browse");

/// Why a group has no form, or null when the wizard can edit it. A
/// source is edited as one thing, so the verdict is the group's and
/// every step under it shows the same one.
function groupEditBlocked(groupId: string): string | null {
  const g = configGroups.value.find((x) => x.id === groupId);
  if (!g) return "This step names a group the config doesn't declare.";
  const { ingest, render } = sourceStepsOf(g.id, sources.value);
  const entry = groupEntry(g, { ingest, render });
  if (!g.type) return "No guided form for this group — edit its entries in Advanced below.";
  if (g.type === "diff") {
    return (
      "A diff group compares two commits of its source; change them under " +
      "`params.diff` in Advanced below, or remove the group."
    );
  }
  if (!entry) return `No guided form: the catalog doesn't know the type "${g.type}".`;
  if (!entry.wizard) return `No guided form for ${entry.label} yet — edit it in Advanced below.`;
  for (const step of [ingest, render]) {
    if (!step) continue;
    const rep = paramsAreRepresentable(step, entry);
    if (!rep.ok) {
      return (
        `The form doesn't model ${rep.unknown.join(", ")} on ${step.id}, and saving would ` +
        `drop it. Edit this one in Advanced below.`
      );
    }
  }
  return null;
}

/// The catalog entry describing a group. Its `type` names the provider,
/// but *which* descriptor — Gmail or Fastmail, both `email` — is read
/// off its ingest step's params, the way the step row does it.
function groupEntry(g: ConfiguredGroup, steps: SourceSteps): CatalogEntry | undefined {
  const step = steps.ingest ?? steps.render;
  return step ? entryForStep(step, sources.value) : catalogForStep(g.type, {});
}

/// What each Actions-column button does. The rows say which buttons a
/// row carries and whether each is enabled; this is the code behind
/// the id.
const rowActions: Record<string, (row: Row) => void> = {
  browse: (row) => openBrowse(row),
  sync: (row) => void runRow(row),
  stop: (row) => {
    if (row.stop_job_id) void stopJob(row.stop_job_id);
  },
};

const tableGrid = ref<{ refreshCells: (fields?: string[]) => void } | null>(null);

let gridApi: TableGridApi<Row> | null = null;
function onGridReady(api: TableGridApi<Row>) {
  gridApi = api;
}

/// Commit only the newest answer, whatever order the answers arrive in.
///
/// Not theoretical: a job list fetched before a sync was enqueued but landing
/// after it drops the new job, and the row reads as *never synced* one frame
/// after being queued. `data-sources-sync`'s monotonicity test catches it.
function freshest<T>(commit: (value: T) => void) {
  let issued = 0;
  let committed = 0;
  const run = async (load: () => Promise<T>) => {
    const seq = ++issued;
    const value = await load();
    if (seq <= committed) return;
    committed = seq;
    commit(value);
  };
  /// Drop everything already in flight.
  run.invalidate = () => {
    committed = issued;
  };
  return run as typeof run & { invalidate: () => void };
}

// ── One step's log. A red Status says *that* a step failed; the next
// question is always what it was doing. Double-clicking the cell opens
// the run store's lines for that step, in the run it last took part in
// — or the one in flight — as a grid that follows the run while it goes.

/// The run whose log answers "what was this step doing": the one in
/// flight if the step is in it, else the one its record names, else —
/// for a record from before runs had ids — the newest run the store says
/// it took part in.
async function runFor(row: Row): Promise<{ runId: string; live: boolean } | null> {
  if (row.live_run_id) return { runId: row.live_run_id, live: true };
  if (row.last_run_id) return { runId: row.last_run_id, live: false };
  const [newest] = await fetchRuns({ step: row.id, limit: 1 });
  return newest ? { runId: newest.run_id, live: !newest.finished_at_utc } : null;
}

/// The server's log — what `datalib-http` itself said: the worker, the
/// applets, every request that failed — as a card beside this one,
/// opened on the launch serving this page; when the server is
/// recording nothing (no store), on everything, narrowed by the query
/// bar.
function openServerLog() {
  const launch = healthSnapshot()?.process_id ?? null;
  props.ctx.host.openCards(logSource(launch ? { launch } : { q: "process:http min_level:info" }));
}

/// A step's log as a card beside this one. With `runId`, that run's;
/// without, the run in flight if the step is in it, else the one it
/// last took part in.
async function openStepLog(row: Row, runId: string | null = null) {
  try {
    const run = runId
      ? { runId, live: !!manage.value?.run?.live && manage.value.run.run_id === runId }
      : await runFor(row);
    if (!run) {
      pushToast("This step has not taken part in any run the store remembers.");
      return;
    }
    // Open at the line that says how the step ended, for a row whose
    // status is the outcome of a run — the hover on Failed or Stopped
    // promises exactly that.
    const jumpToEnd = !run.live && !runId && ["failed", "stopped"].includes(row.status.key);
    props.ctx.host.openCards(logSource({ run: run.runId, step: row.id, jumpToEnd }));
  } catch (e) {
    pushToast((e as Error).message);
  }
}

// ── The status bar ───────────────────────────────────────────────────

/// The root's series, scaled to its own range rather than to zero.
function onCellDoubleClicked(data: Row, field: string) {
  if (field === "problems") {
    openProblems(data);
    return;
  }
  if (field !== "status") return;
  // A group's status is one child's, and that child's log is the answer.
  const row =
    data.kind === "group"
      ? rows.value.find((r) => r.kind !== "group" && r.id === data.status_from)
      : data;
  if (row) void openStepLog(row);
}

/// The problems behind a row's count, as a grid over the index's
/// `problems` table filtered to the row's source. A step's problems are
/// its group's — the render store is where a source's live — so a step
/// row opens the same grid as its group. The index group shows every
/// source's.
function openProblems(row: Row) {
  const sourceId = row.kind === "group" ? row.id : (row.group ?? row.id);
  const q = sourceId === "unified_index" ? "" : `source_id:${sourceId}`;
  const url = `/applet/unified_index/problems?q=${encodeURIComponent(q)}`;
  const source =
    row.kind === "group" ? row : rows.value.find((r) => r.kind === "group" && r.id === sourceId);
  const title =
    sourceId === "unified_index" ? "Problems" : `Problems: ${source?.name.label ?? sourceId}`;
  props.ctx.host.openCards(
    `tableView({ url: ${JSON.stringify(url)}, title: ${JSON.stringify(title)} })`,
  );
}

/// An in-place edit of the Name cell: a group's rename.
function onCellEdit(row: Row, field: string, value: string) {
  if (field === "name" && row.kind === "group") void renameRow(row, value);
}

// ── A tree's commit history. Every doltlite store keeps its own log —
// one commit per sync, checkpoint or render pass — and this is the first
// place the app shows it: one row per commit, with what it did to each
// table. Read on demand, and re-read while open whenever the runner's
// record moves, which is the same push that keeps the size column live.

/// The rows whose history is open — several, when several were
/// selected — or empty when the panel is closed.
const historyFor = ref<Row[]>([]);
const historyLines = ref<HistoryRow[]>([]);
const historyTruncated = ref<string[]>([]);
const historyBusy = ref(false);
const historyError = ref<string | null>(null);
const loadHistory = freshest<{ rows: HistoryRow[]; truncated: string[] } | Error>((v) => {
  historyBusy.value = false;
  if (v instanceof Error) historyError.value = v.message;
  else {
    historyError.value = null;
    historyLines.value = v.rows;
    historyTruncated.value = v.truncated;
  }
});

async function fetchHistoryRows(trees: string[]) {
  try {
    const hs = await Promise.all(trees.map((t) => fetchTreeHistory(t)));
    return { rows: historyRows(hs), truncated: truncatedStores(hs) };
  } catch (e) {
    return e as Error;
  }
}

function openHistory(targets: Row[]) {
  historyFor.value = targets;
  historyLines.value = [];
  historyTruncated.value = [];
  historyError.value = null;
  historyBusy.value = true;
  loadHistory.invalidate();
  void loadHistory(() => fetchHistoryRows(targets.map((r) => r.id)));
}

/// While the panel is open, a step that just committed shows up without
/// a reopen. Cheap enough to do on every `dag` frame: the walk is
/// bounded and the answer is small.
function refreshHistory() {
  const trees = historyFor.value.map((r) => r.id);
  if (trees.length === 0) return;
  void loadHistory(() => fetchHistoryRows(trees));
}

/// What the panel is titled: one row's name, or the names joined.
const historyTitle = computed(() => historyFor.value.map((r) => r.name.label).join(", "));

/// Where the open rows' history is kept, for the panel's subtitle.
const historyStoreNote = computed(() => {
  const rows = historyFor.value;
  if (rows.length !== 1) return `every store under ${rows.map((r) => `${r.id}/`).join(", ")}`;
  const [row] = rows;
  return row.kind === "group" ? `every store under ${row.id}/` : `the stores in ${row.id}/`;
});

/// A commit names its run, and that run's log is the "how" behind the
/// commit's "what" — from the run store, so a run started from a
/// terminal has one too. Filtered to the step that writes the store,
/// as the Status double-click does.
function openRunLog(row: HistoryRow) {
  if (!row.run) return;
  const step = rows.value.find((r) => r.kind !== "group" && r.id === row.stepId);
  if (!step) return;
  historyFor.value = [];
  void openStepLog(step, row.run);
}

// ── The right-click menu. Every action a row offers, in one place,
// with Lightroom semantics: right-click a row inside the selection and
// the whole selection is the target; outside it, that row alone, and
// the selection stays as it was. An entry that does not apply stays,
// disabled, with the reason as its tooltip — see `config/rowMenu.ts`.

/// The rows a right-click acts on, in table order: the selection when
/// the row under the pointer is in it, that row alone when it is not.
/// The selection itself is never touched — as in Lightroom, a
/// right-click aims the action, it does not re-select.
function menuTarget(row: Row): MenuTarget {
  return {
    id: row.id,
    name: row.name.label,
    kind: row.kind,
    type: row.type?.id ?? null,
    func: row.function,
    runBlocked: row.actions.find((a) => a.id === "sync")?.disabled_reason ?? null,
    editBlocked: row.editBlocked,
    revealBlocked: row.reveal_blocked,
    browseBlocked: browseAction(row)?.disabled_reason ?? null,
    stopJobId: row.stop_job_id,
    statusFrom: row.status_from,
    revealPath: row.reveal_path,
  };
}

function contextMenuItems(anchor: Row, targets: Row[], column: string): MenuEntry[] {
  if (targets.length === 0) return [];
  return rowMenu(targets.map(menuTarget), { column, canReveal, revealLabel }).map((entry) =>
    entry.separator
      ? { name: "", separator: true }
      : {
          name: entry.name,
          disabled: entry.disabled,
          danger: ["remove", "reset", "reset_blobs"].includes(entry.action),
          action: () => void runMenuAction(entry.action, targets, anchor),
        },
  );
}

async function runMenuAction(action: MenuAction, targets: Row[], anchor: Row) {
  const [first] = targets;
  switch (action) {
    case "browse":
      openBrowse(first);
      return;
    case "sync":
      await runRows(targets);
      return;
    case "stop": {
      // One stop per job: several rows can be claimed by the same one.
      const jobs = new Set<string>();
      for (const t of targets) {
        if (t.stop_job_id && !jobs.has(t.stop_job_id)) {
          jobs.add(t.stop_job_id);
          await stopJob(t.stop_job_id);
        }
      }
      return;
    }
    case "edit":
      if (first.editGroup) openEdit(first.editGroup);
      return;
    case "compare":
      compareFor.value = { id: first.id, name: first.name.label };
      return;
    case "rename":
      gridApi?.startEditing(anchor, "name");
      return;
    case "copy_id":
      await copyToClipboard(targets.map((t) => t.id).join("\n"));
      return;
    case "copy_path":
      await copyToClipboard(
        targets
          .map((t) => t.reveal_path)
          .filter((p): p is string => !!p)
          .join("\n"),
      );
      return;
    case "log": {
      const row =
        first.kind === "group"
          ? rows.value.find((r) => r.kind !== "group" && r.id === first.status_from)
          : first;
      if (row) void openStepLog(row);
      return;
    }
    case "history":
      openHistory(targets);
      return;
    case "reveal":
      for (const t of targets) await reveal(t.key);
      return;
    case "reset":
      await resetRows(targets, false);
      return;
    case "reset_blobs":
      await resetRows(targets, true);
      return;
    case "remove":
      await deleteRows(targets);
      return;
  }
}

/// Write a group's new name, or drop the line when it is blank or is
/// the id again — `renameGroup` treats both as "no name".
async function renameRow(row: Row, name: string) {
  if (row.kind !== "group") return;
  const next = renameGroup(configText.value, row.id, name);
  if (next === configText.value) return;
  await writeConfig(
    next,
    name ? `Renamed ${row.id} to ${name}.` : `Cleared the name of ${row.id}.`,
  );
}

/// The history panel's columns: what each is, by type, and how the
/// ones a type cannot draw alone are drawn.
const historyColumns: ColumnSpec[] = [
  // The tree column: a store, the commits under it, the tables under
  // each commit. The label is the store's file name, the commit's
  // message, or the table's name; the level says which it is.
  { field: "label", header: "Commit", type: "text", default_visible: true, editable: false },
  // Relative on top, exact underneath — stacked like the size cell,
  // because a sync commits several times inside one minute and ten
  // "18 hours ago"s in a row say nothing about their order.
  { field: "date", header: "When", type: "timestamp", default_visible: true, editable: false },
  {
    field: "rows",
    header: "Rows",
    type: "count",
    description: "Rows after this commit — across the data tables, or in the one table",
    default_visible: true,
    editable: false,
  },
  { field: "added", header: "Added", type: "count", default_visible: true, editable: false },
  { field: "deleted", header: "Deleted", type: "count", default_visible: true, editable: false },
  { field: "modified", header: "Modified", type: "count", default_visible: true, editable: false },
  // The run that made the commit, when the message names one, as the
  // way to its log: the commit is what the run did, the log is how.
  { field: "run", header: "Run", type: "text", default_visible: true, editable: false },
  { field: "hash", header: "Hash", type: "text", default_visible: true, editable: false },
];

const historyOverrides: Record<string, Partial<Column<HistoryRow>>> = {
  label: {
    width: 360,
    minWidth: 320,
    params: {
      innerFormatter: (_r: number, _c: number, _v: unknown, _col: unknown, row: HistoryRow) => {
        const wrap = document.createElement("span");
        wrap.className = `m2-history-label m2-history-${row?.level ?? "commit"}`;
        wrap.textContent = row?.label ?? "";
        if (row?.level === "store") {
          const dir = document.createElement("span");
          dir.className = "m2-cell-dir";
          dir.textContent = row.storePath.slice(0, row.storePath.lastIndexOf("/"));
          wrap.appendChild(dir);
        }
        return wrap;
      },
    },
  },
  date: {
    width: 170,
    minWidth: 170,
    formatter: (_r, _c, value) => {
      const wrap = document.createElement("span");
      if (!value) return wrap;
      wrap.className = "m2-history-when";
      const rel = document.createElement("span");
      rel.textContent = formatRelative(String(value), Date.now());
      const abs = document.createElement("span");
      abs.className = "m2-cell-dir";
      abs.textContent = formatStamp(String(value));
      wrap.append(rel, abs);
      return wrap;
    },
  },
  rows: {
    width: 100,
    minWidth: 100,
    formatter: (_r, _c, value) => formatCount(value as number | null),
  },
  added: {
    width: 90,
    minWidth: 90,
    formatter: (_r, _c, value) => formatDelta(value as number | null, "+"),
  },
  deleted: {
    width: 90,
    minWidth: 90,
    formatter: (_r, _c, value) => formatDelta(value as number | null, "−"),
  },
  modified: {
    width: 96,
    minWidth: 96,
    formatter: (_r, _c, value) => formatDelta(value as number | null, "~"),
  },
  run: {
    width: 120,
    minWidth: 120,
    formatter: (_r, _c, _v, _col, row) => {
      const wrap = document.createElement("span");
      if (!row?.run) return wrap;
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "m2-history-run";
      btn.textContent = row.run.slice(0, 8);
      btn.title = `Show the log of run ${row.run}`;
      btn.addEventListener("click", () => void openRunLog(row));
      wrap.appendChild(btn);
      return wrap;
    },
  },
  hash: {
    width: 130,
    minWidth: 130,
    formatter: (_r, _c, value, _col, row) => {
      const wrap = document.createElement("span");
      if (row?.level !== "commit" || !value) return { html: wrap, toolTip: "" };
      const hash = String(value);
      wrap.className = "m2-history-hash";
      wrap.textContent = hash.slice(0, 10);
      wrap.appendChild(copyIdButton(hash, "Copy the commit hash"));
      return { html: wrap, toolTip: hash };
    },
  },
};

/// The 🆔 button the chat views put beside every uuid, for a commit
/// hash: the full 40 characters, where the cell shows ten.
function copyIdButton(id: string, label: string): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "m2-copy-id";
  btn.title = `${label} (${id})`;
  btn.setAttribute("aria-label", label);
  btn.textContent = "🆔";
  btn.addEventListener("click", async (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    if (await copyToClipboard(id)) {
      btn.textContent = "✓";
      btn.classList.add("copied");
    } else {
      btn.classList.add("copy-failed");
    }
    setTimeout(() => {
      btn.textContent = "🆔";
      btn.classList.remove("copied", "copy-failed");
    }, 900);
  });
  return btn;
}

const COUNT_FMT = new Intl.NumberFormat();
function formatCount(n: number | null | undefined): string {
  return typeof n === "number" ? COUNT_FMT.format(n) : "";
}
/// A zero reads as nothing rather than as "0": a column of zeros with
/// the odd number in it is easier to scan than a column of numbers.
function formatDelta(n: number | null | undefined, sign: string): string {
  return n ? `${sign}${COUNT_FMT.format(n)}` : "";
}

// ── Which groups are open. Remembered per browser, so a reload — or
// the remount a sync's end does — puts the table back the way it was.
// A convenience, not state: nothing breaks when it is empty.
const EXPANDED_STORE = "datalib.manage.expanded";

function readExpanded(): Set<string> {
  try {
    const raw = localStorage.getItem(EXPANDED_STORE);
    const list: unknown = raw ? JSON.parse(raw) : [];
    return new Set(
      Array.isArray(list) ? list.filter((x): x is string => typeof x === "string") : [],
    );
  } catch {
    return new Set();
  }
}
const expandedGroups = readExpanded();

function isGroupOpenByDefault(row: Row): boolean {
  return expandedGroups.has(row.key);
}

function onRowGroupOpened(row: Row, expanded: boolean) {
  const key = row.key;
  if (!key) return;
  if (expanded) expandedGroups.add(key);
  else expandedGroups.delete(key);
  try {
    localStorage.setItem(EXPANDED_STORE, JSON.stringify([...expandedGroups]));
  } catch {
    // Storage refused — private mode, quota — and the chevron still
    // works; only the memory across reloads is lost.
  }
}

/// Escape closes the commit history, which is what a modal owes its
/// reader.
function onWindowKeydown(e: KeyboardEvent) {
  if (e.key !== "Escape") return;
  if (historyFor.value.length) historyFor.value = [];
}

function reparse() {
  try {
    sources.value = listSteps(configText.value);
    configGroups.value = listGroups(configText.value);
    parseError.value = null;
  } catch (e) {
    parseError.value = (e as Error).message;
  }
  // A hand-edit that makes a source editable again has to reach its
  // Edit button, whose face is decided outside the row.
  tableGrid.value?.refreshCells(["actions"]);
}

async function loadConfig() {
  loadError.value = null;
  try {
    let cfg = await fetchConfig();
    if (!cfg.exists) cfg = await fetchConfigScaffold();
    configPath.value = cfg.path;
    // `parsed_ok` false means the file is not a config at all — in
    // which case `App.vue`'s gate is showing instead of this view, and
    // this is belt and braces. Ordinary per-entry problems are not
    // errors of the whole config and live in `configDiagnostics`.
    configError.value = cfg.parsed_ok ? null : (cfg.error ?? "The config was rejected.");
    serverSourceCount.value = cfg.source_count;
    configExists.value = cfg.exists;
    configText.value = cfg.text;
    reparse();
    if (sources.value.length === 0 && cfg.source_count > 0) {
      // The inspector is the only channel when someone hits this in the
      // desktop app and can't copy text out of a banner.
      console.warn(
        "sources card: parsed 0 entries from a config the server reads",
        cfg.source_count,
        "sources from —",
        { path: cfg.path, textLength: cfg.text.length, parsedOk: cfg.parsed_ok },
      );
    }
  } catch (e) {
    loadError.value = (e as Error).message;
  }
}

/// The rows the loader dropped, for the banner above the table.
const droppedRows = computed(() => rows.value.filter((r) => r.dropped));

/// Repaint the columns whose content is a `cellRenderer` over state
/// that lives outside the row's identity.
function repaint() {
  tableGrid.value?.refreshCells();
}

const commitJobs = freshest<SyncJob[]>((list) => {
  jobs.value = list;
  // The queue decides "Queued" and the Run/Stop face, so a new job is
  // a repaint even when the runner's record hasn't moved.
  repaint();
  // Both paths retire the banner, because either can be the one that
  // learns the job stopped: the push covers a sync this server ran,
  // the poll covers a dropped SSE connection and a run started from a
  // terminal.
  if (bannerJob.value) {
    const j = list.find((x) => x.id === bannerJob.value);
    // A job that has fallen off the end of the queue we hold is not
    // running either, so the banner goes.
    if (j) retireBanner(j);
    else clearBanner();
  }
});

async function loadJobs() {
  try {
    await commitJobs(() => fetchAllJobs(100));
  } catch {
    // The grid is still useful without status; leave the columns empty.
  }
}

/// A job the worker is about to start, or has started. Gates the
/// Sync-everything button, and marks the window in which the runner's
/// record has nothing to say yet: between the click and its first
/// written state there is nothing there to read.
const anyJobActive = computed(() => jobs.value.some((j) => j.active));

const commitRows = freshest<ManageResponse>((m) => {
  manage.value = m;
  // Several columns are `cellRenderer`s over data outside the row's
  // identity, so a new answer only reaches the screen if the cells are
  // told to repaint.
  repaint();
});

/// Read the rows. `refresh` asks the backend to walk the disk before
/// answering rather than serving its last tick — see `fetchManageRows`.
async function loadRows(refresh = false) {
  try {
    await commitRows(() => fetchManageRows(refresh));
  } catch {
    // The last answer stands; an empty table over an error banner would
    // read as "no sources".
  }
}

/// Write new config text and adopt whatever the backend then reports.
/// The backend validates with the real loader — including the duplicate
/// and reserved-name checks — so a rejection comes back as `ok:false`
/// with the loader's message rather than a thrown error.
async function writeConfig(text: string, what: string) {
  busy.value = true;
  clearBanner();
  try {
    const res = await saveConfig(text);
    if (!res.ok) {
      banner.value = { ok: false, text: res.error ?? "The config was rejected." };
      return false;
    }
    configText.value = text;
    reparse();
    props.ctx.bus.publish(TOPIC_CONFIG_WRITTEN, null);
    // A warning saves — nothing is dropped — but it is still advice
    // the file would otherwise only give on the command line.
    banner.value = { ok: true, text: res.error ? `${what} Warning: ${res.error}` : what };
    return true;
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
    return false;
  } finally {
    busy.value = false;
  }
}

function closeWizard() {
  wizardOpen.value = false;
  editing.value = null;
}

function openAdd() {
  editing.value = null;
  wizardKey.value++;
  wizardOpen.value = true;
}

/// Open the wizard on a source: its group, with both its steps' values
/// in one form.
function openEdit(groupId: string) {
  const group = configGroups.value.find((g) => g.id === groupId);
  if (!group) return;
  const steps = sourceStepsOf(group.id, sources.value);
  const entry = groupEntry(group, steps);
  if (!entry) return;
  editing.value = { group, entry, steps };
  wizardKey.value++;
  wizardOpen.value = true;
}

async function onWizardSubmit(payload: {
  id: string;
  name: string;
  description: string;
  entry: CatalogEntry;
  groupBody: string | null;
  stepsBody: string;
  renderId: string | null;
}) {
  const current = editing.value;
  let next: string;
  if (current) {
    // Both steps are replaced in one cut-and-append, and a step the
    // source was missing is simply appended with the other. The name
    // and the description live on the group, which is edited in place.
    const existing = [current.steps.ingest, current.steps.render].filter(
      (s): s is ConfiguredStep => !!s,
    );
    next = replaceSteps(configText.value, existing, payload.stepsBody);
    next = renameGroup(next, current.group.id, payload.name);
    next = describeGroup(next, current.group.id, payload.description);
    // A render step the provider does not write back — hand-written
    // under a download-only type — leaves with the cut above, so its
    // edges have to go too, or the fan-ins name a step that no longer
    // exists and the loader refuses the whole file.
    if (current.steps.render && !payload.renderId) {
      next = unwireFromFanIns(next, current.steps.render.id);
    }
  } else {
    next = appendSource(
      configText.value,
      payload.groupBody ? `${payload.groupBody}\n\n${payload.stepsBody}` : payload.stepsBody,
    );
  }

  // The fan-ins name their inputs, so a render step added without this
  // renders happily and is never indexed. Idempotent, so re-saving an
  // edit doesn't duplicate the entry.
  if (payload.renderId) next = wireIntoFanIns(next, payload.renderId);

  // Banners are for a person, so they say the name; the id is what the
  // config and the disk use.
  const shown = payload.name || payload.id;
  const ok = await writeConfig(next, current ? `Saved ${shown}.` : `Added ${shown}.`);
  if (!ok) return;
  closeWizard();
}

// ── "Compare two syncs…": a diff group written from a source and two
// commits of its raw store (docs/dev/plans/completed/diff_renderer.md), wired into
// the fan-ins like any render step, then the source synced so the diff
// renders — its step is downstream of the source's ingest.
const compareFor = ref<{ id: string; name: string } | null>(null);

async function onCompareSubmit(payload: {
  id: string;
  name: string;
  source: string;
  from: string;
  to: string;
  maxDocuments: number;
}) {
  const built = buildDiffSource(payload);
  let next = appendSource(configText.value, `${built.groupBody}\n\n${built.stepsBody}`);
  next = wireIntoFanIns(next, built.renderId);
  const ok = await writeConfig(next, `Added ${payload.name}.`);
  if (!ok) return;
  compareFor.value = null;
  const source = rows.value.find((r) => r.kind === "group" && r.id === payload.source);
  if (source) await runRows([source]);
}

async function deleteSource(id: string) {
  const step = sources.value.find((s) => s.id === id);
  if (!step) return;
  const name = step.name;

  // Deleting a fetch step takes its render step too. Leaving the render
  // step behind would leave an input naming a step that no longer
  // exists, which the loader refuses outright — a whole config broken
  // by a partial delete.
  const sibling = step.phase === "ingest" ? renderSiblingOf(step.id) : undefined;
  const doomed = sibling ? [step, sibling] : [step];

  const what =
    step.kind === "applet"
      ? `Remove the "${name}" applet from the config?\n\n` +
        `The server stops it. Anything in the app that its components or endpoints ` +
        `serve will stop working until you add it back.`
      : step.phase === "index"
        ? `Remove the "${name}" index step from the config?\n\n` +
          `Its output stays on disk but stops being refreshed, so search results go stale.`
        : sibling
          ? `Remove "${name}" and the render step that reads it ("${sibling.name}")?\n\n` +
            `Both have to go together: a render step whose input is gone is a config ` +
            `datalib refuses to load.\n\n` +
            `The data stays on disk. Re-adding later resumes from what's already there.`
          : `Remove "${name}" from the config?\n\n` +
            `Its data stays on disk — only this step stops running. Re-adding it later ` +
            `resumes from what's already there.`;
  if (!window.confirm(what)) return;

  // A group with nothing left under it goes too: the loader would only
  // warn about it, but a `[[groups]]` entry naming a source that is
  // gone is litter someone has to explain.
  const goneIds = new Set(doomed.map((d) => d.id));
  const emptied = configGroups.value.filter(
    (g) =>
      step.group === g.id && !sources.value.some((s) => s.group === g.id && !goneIds.has(s.id)),
  );
  // Cut first: the entries' offsets are into the text as parsed, and
  // unwiring a fan-in above the source would shift them. Unwiring is a
  // regex over the result, so it needs no offsets.
  let next = removeSteps(configText.value, [...doomed, ...emptied]);
  for (const d of doomed) {
    if (d.phase === "render") next = unwireFromFanIns(next, d.id);
  }
  await writeConfig(next, `Removed ${name}.`);
}

/// Remove a group with everything filed under it. Its render steps
/// leave the fan-ins too, or the config would name inputs that no
/// longer exist and the loader would refuse the whole file.
async function deleteGroup(id: string) {
  const group = configGroups.value.find((g) => g.id === id);
  if (!group) return;
  const name = group.name ?? group.id;
  const members = sources.value.filter((s) => s.group === id);
  const steps = members.filter((s) => s.kind === "step").length;
  const applets = members.filter((s) => s.kind === "applet").length;
  const count = (n: number, word: string) => `${n} ${word}${n === 1 ? "" : "s"}`;
  const under = [steps ? count(steps, "step") : "", applets ? count(applets, "applet") : ""]
    .filter(Boolean)
    .join(" and ");
  const what =
    `Remove "${name}" from the config${under ? `, with the ${under} under it` : ""}?\n\n` +
    `The data stays on disk — these entries just stop running. Adding the source ` +
    `back later resumes from what's already there.`;
  if (!window.confirm(what)) return;

  let next = removeSteps(configText.value, [...members, group]);
  for (const m of members) {
    if (m.phase === "render") next = unwireFromFanIns(next, m.id);
  }
  await writeConfig(next, `Removed ${name}.`);
}

/// Several rows at once: one question, one write. A single row keeps
/// its own wording, which says what else goes with it.
async function deleteRows(targets: Row[]) {
  if (targets.length === 1) {
    const [row] = targets;
    if (row.kind === "group") await deleteGroup(row.id);
    else await deleteSource(row.id);
    return;
  }
  const doomed = new Map<string, ConfiguredStep | ConfiguredGroup>();
  const groups = new Set<string>();
  for (const t of targets) {
    if (t.kind === "group") {
      const group = configGroups.value.find((g) => g.id === t.id);
      if (!group) continue;
      groups.add(group.id);
      doomed.set(`group:${group.id}`, group);
      for (const m of sources.value.filter((s) => s.group === group.id)) doomed.set(m.id, m);
    } else {
      const step = sources.value.find((s) => s.id === t.id);
      if (!step) continue;
      doomed.set(step.id, step);
      const sibling = step.phase === "ingest" ? renderSiblingOf(step.id) : undefined;
      if (sibling) doomed.set(sibling.id, sibling);
    }
  }
  // A group with nothing left under it goes too, as in `deleteSource`.
  for (const g of configGroups.value) {
    if (groups.has(g.id)) continue;
    const left = sources.value.some((s) => s.group === g.id && !doomed.has(s.id));
    const had = sources.value.some((s) => s.group === g.id);
    if (had && !left) doomed.set(`group:${g.id}`, g);
  }
  const names = targets.map((t) => `"${t.name}"`).join(", ");
  const what =
    `Remove ${names} from the config, with everything under them?\n\n` +
    `The data stays on disk — these entries just stop running. Adding a source ` +
    `back later resumes from what's already there.`;
  if (!window.confirm(what)) return;
  const entries = [...doomed.values()];
  let next = removeSteps(configText.value, entries);
  for (const d of entries) {
    if ("phase" in d && d.phase === "render") next = unwireFromFanIns(next, d.id);
  }
  await writeConfig(next, `Removed ${targets.length} entries.`);
}

/// Leave the Manage screen for this row's data: one card, the grid,
/// already filtered to the source and carrying its type's columns.
///
/// A card stack IS the URL (see router/columns.ts), so this is an
/// ordinary navigation — the card is bookmarkable, shareable, and the
/// back button returns here.
function openBrowse(row: Row) {
  if (!row.browseSource) return;
  // Beside this card, in whatever layout is showing it.
  props.ctx.host.openCards(row.browseSource);
}

async function reveal(key: string) {
  const path = rows.value.find((r) => r.key === key)?.reveal_path;
  if (!path) return;
  await revealPath(path);
}

/// Show a path where it lives. Shared by the row action and the config
/// file's own button — both fail the same way, and both should say so
/// rather than doing nothing.
async function revealPath(path: string) {
  const ok = await revealInFileManager(path);
  if (!ok) {
    banner.value = { ok: false, text: `Could not open ${path} in the file manager.` };
  }
}

/// Sync what a row stands for. A step is its own seed; a group's seeds
/// are its source steps, and the worker takes them comma-joined, one
/// `--sync` each, so the whole group runs as one job.
function runRow(row: Row) {
  return runRows([row]);
}

/// Several rows as one job, so their downstream steps run once.
async function runRows(targets: Row[]) {
  const seeds = [...new Set(targets.flatMap((r) => r.seeds))];
  if (seeds.length === 0) return;
  const shown = targets
    .map((row) => {
      const step = sources.value.find((s) => s.id === row.id);
      return row.kind === "group" ? row.name.label : (step?.name ?? row.id);
    })
    .join(", ");
  busy.value = true;
  clearBanner();
  try {
    const job = await enqueueJob({ kind: "all", source_ids: seeds.join(",") });
    adoptJob(job);
    say(true, `Queued a sync for ${shown}.`, job.id);
    // Before returning: the queue is what puts this row and everything
    // downstream of it into "Queued" and flips the button to Stop, and
    // the whole complaint this answers is that pressing play looked
    // like nothing happened. The job frame will refetch too; this is
    // for a page whose stream is down.
    await Promise.all([loadJobs(), loadRows()]);
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  } finally {
    busy.value = false;
  }
}

/// The steps a reset of these rows drops: a step is itself, a group is
/// every step under it that keeps a store; with `blobs`, a download
/// step's blob store goes with it (`docs/dev/step_protocol.md` § Reset).
function resetTargets(targets: Row[], blobs: boolean): string[] {
  const keeps = (r: Row) =>
    r.kind === "step" && r.function !== "grid_index" && r.function !== "qmd_index";
  const steps = targets.flatMap((t) =>
    t.kind === "step" ? [t] : rows.value.filter((r) => keeps(r) && r.id.startsWith(`${t.id}/`)),
  );
  const ids = steps
    .filter(keeps)
    .map((r) => (blobs && r.function === "ingest" ? `${r.id}+blobs` : r.id));
  return [...new Set(ids)];
}

/// Drop what these rows wrote, keeping the history: a job the worker
/// turns into `datalib-dag --reset`. Nothing syncs until someone asks.
async function resetRows(targets: Row[], blobs: boolean) {
  const ids = resetTargets(targets, blobs);
  const shown = targets.map((t) => t.name.label).join(", ");
  if (ids.length === 0 || (blobs && !ids.some((id) => id.endsWith("+blobs")))) {
    say(false, `Nothing under ${shown} keeps ${blobs ? "attachments" : "a store"} to reset.`);
    return;
  }
  const what =
    `Reset ${shown}${blobs ? ", attachments included" : ""}?\n\n` +
    `Its stores are dropped and the next sync starts from scratch. The rows stay ` +
    `in the doltlite history` +
    (blobs
      ? `; attachments already downloaded are deleted and fetched again.`
      : `, and attachments already downloaded are kept.`);
  if (!window.confirm(what)) return;
  busy.value = true;
  clearBanner();
  try {
    const job = await enqueueJob({ kind: "reset", source_ids: ids.join(",") });
    adoptJob(job);
    say(true, `Queued a reset of ${shown}. Sync it when you are ready.`, job.id);
    await Promise.all([loadJobs(), loadRows()]);
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  } finally {
    busy.value = false;
  }
}

/// Fold a job we have in hand into the queue we hold.
function adoptJob(job: SyncJob) {
  // Newest truth wins: anything already in flight predates this job.
  commitJobs.invalidate();
  const at = jobs.value.findIndex((j) => j.id === job.id);
  jobs.value =
    at >= 0 ? [...jobs.value.slice(0, at), job, ...jobs.value.slice(at + 1)] : [job, ...jobs.value];
  repaint();
}

/// Sync everything the config declares, in one run.
async function runEverything() {
  busy.value = true;
  clearBanner();
  try {
    const job = await enqueueJob({ kind: "all" });
    adoptJob(job);
    say(true, "Queued a sync of everything.", job.id);
    await Promise.all([loadJobs(), loadRows()]);
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  } finally {
    busy.value = false;
  }
}

/// Call off the job that has a row claimed.
async function stopJob(jobId: string) {
  const sourceIds = jobs.value.find((j) => j.id === jobId)?.source_ids;
  busy.value = true;
  clearBanner();
  try {
    await cancelJob(jobId);
    say(
      true,
      `Stopping the sync of ${sourceIds || "everything"}. Steps in flight ` +
        `checkpoint what they have and exit.`,
      jobId,
    );
    await Promise.all([loadJobs(), loadRows()]);
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  } finally {
    busy.value = false;
  }
}

/// One pushed job update. The queue decides "Queued" and the Run/Stop
/// face, and the row is written before the frame is published, so the
/// refetch sees it.
function onJobEvent(e: JobProgressEvent) {
  mergeJob(e);
  const job = jobs.value.find((j) => j.id === e.id);
  if (job) retireBanner(job);
  const active = e.active;
  // A job ending is exactly when the size on screen is about to be
  // read and is about to be wrong — so that one asks for a fresh walk.
  // It is also the last chance for a while: the backend's own tick
  // stops as soon as the run lets go of the root.
  void loadRows(!active);
}

/// Fold a pushed job update into the queue we hold, so the Run/Stop
/// face and every Queued row move on the push rather than on the next
/// `GET /api/sync/jobs/all`.
function mergeJob(e: JobProgressEvent) {
  const now = new Date().toISOString();
  const at = jobs.value.findIndex((j) => j.id === e.id);
  if (at >= 0) {
    const prev = jobs.value[at];
    const next: SyncJob = {
      ...prev,
      state: e.state,
      active: e.active,
      stopping: prev.stopping && e.active,
      progress_msg: e.progress_msg,
      started_at_utc: prev.started_at_utc ?? (e.state === "running" ? now : null),
      finished_at_utc:
        prev.finished_at_utc ??
        (e.state === "done" || e.state === "failed" || e.state === "canceled" ? now : null),
    };
    jobs.value = [...jobs.value.slice(0, at), next, ...jobs.value.slice(at + 1)];
    return;
  }
  jobs.value = [
    {
      id: e.id,
      kind: e.kind,
      source_ids: e.source_ids,
      state: e.state,
      active: e.active,
      stopping: false,
      progress_pct: null,
      progress_msg: e.progress_msg,
      error: null,
      created_at_utc: now,
      started_at_utc: e.state === "running" ? now : null,
      finished_at_utc: null,
    },
    ...jobs.value,
  ];
}

let unsubscribe: (() => void) | null = null;

/// Everything this table shows, refetched together — which is the point, and
/// why this is one function rather than three calls at three cadences. Rows come
/// from the config, Status and Last synced from the runner's record; fetch the
/// first without the second and a row that has run paints as "Never run".
async function reloadAll(freshStorage = false) {
  await Promise.all([loadConfig(), loadJobs(), loadRows(freshStorage)]);
}

onMounted(async () => {
  // Fresh sizes on the first paint. The backend only walks the disk
  // while a run is in flight, so on an idle root — the usual state —
  // this is the walk that produces the numbers on screen.
  await reloadAll(true);
  window.addEventListener("keydown", onWindowKeydown);

  // Two push channels, and the split matters.
  unsubscribe = subscribeLive({
    job: onJobEvent,
    root: (e) => {
      if (changed(e, "manage.rows")) {
        // Deliberately *not* a fresh walk: this fires a few times a second
        // while a run is going. The sampler is already walking on its own
        // cadence; this just reads what it found.
        void loadRows();
      }
      // The runner's record moving is the nearest thing to "a step
      // committed" — nothing watches the stores themselves.
      if (changed(e, "dag")) refreshHistory();
      if (e.kind === "config_changed") {
        // Config and record together, for the "Never run" reason above.
        void reloadAll();
      }
    },
    // A reconnect means we may have slept through a whole run, and the
    // sampler's own last walk with it. Ask for a fresh one.
    resync: () => void reloadAll(true),
  });
});

onUnmounted(() => {
  window.removeEventListener("keydown", onWindowKeydown);
  unsubscribe?.();
  unsubscribe = null;
  gridApi = null;
});
</script>

<template>
  <section class="m2 m2-card">
    <header class="m2-head">
      <div class="m2-head-actions">
        <button
          class="m2-btn m2-runall"
          :disabled="busy || !!parseError || !!configError || anyJobActive || rows.length === 0"
          :title="
            anyJobActive
              ? 'A sync is already running.'
              : rows.length === 0
                ? 'Nothing configured yet.'
                : 'Run every step the config declares, in one sync.'
          "
          @click="runEverything"
        >
          Sync everything
        </button>
        <button class="m2-add" :disabled="busy || !!parseError || !!configError" @click="openAdd">
          + Data Source
        </button>
        <button
          class="m2-btn"
          title="What the app’s own server has been saying — the sync worker, the applets, requests that failed."
          @click="openServerLog"
        >
          Server log
        </button>
      </div>
    </header>

    <p v-if="loadError" class="m2-msg bad">Could not load the config: {{ loadError }}</p>
    <p v-if="parseError" class="m2-msg bad">
      The config doesn’t parse, so the table below can’t be trusted: {{ parseError }}
    </p>
    <div v-else-if="configError" class="m2-msg bad m2-invalid">
      <b>datalib won’t run this config.</b>
      <span>{{ configError }}</span>
      <span class="m2-invalid-why">
        It parses as TOML, so the table below still reflects it — but nothing will sync, and applets
        won’t start, until this is fixed. Open the config to edit it.
      </span>
      <button class="m2-btn" @click="openConfig">Show the config</button>
    </div>
    <!-- Entries the loader dropped. Not a whole-config error: the rest
         of the pipeline is running, which is why this is a note above a
         working table rather than a screen in front of it. The per-row
         Status column carries each reason; this says how many and where
         to look, because a dropped row is easy to scroll past. -->
    <div v-else-if="droppedRows.length" class="m2-msg bad m2-invalid">
      <b>
        {{ droppedRows.length }}
        {{ droppedRows.length === 1 ? "entry isn’t" : "entries aren’t" }} in the pipeline.
      </b>
      <span class="m2-invalid-why">
        The rest of this config loaded and still syncs. These are in the file and were not loaded —
        each one’s Status cell says why. Open the config to fix them, or run
        <code>datalib-dag --check {{ configPath }}</code
        >.
      </span>
      <ul class="m2-dropped">
        <li v-for="r in droppedRows" :key="r.id">
          <code>{{ r.id }}</code> — {{ r.dropped?.message }}
        </li>
      </ul>
      <button class="m2-btn" @click="openConfig">Show the config</button>
    </div>
    <p v-if="banner" class="m2-msg" :class="banner.ok ? 'good' : 'bad'">{{ banner.text }}</p>

    <div class="m2-grid">
      <!-- The typed viewer over the rows the server assembled: a tree,
           one group row with its steps and applets under it. Every row
           is rendered: a config is tens of rows, and a source just
           added lands at the bottom, where a virtualized grid would
           have no row for it until scrolled to. -->
      <TableGrid
        ref="tableGrid"
        :columns="manage?.columns ?? []"
        :rows="rows"
        :tree="true"
        :virtualizeRows="false"
        :windowSecs="storage?.window_secs ?? 300"
        :actions="rowActions"
        :menu="contextMenuItems"
        :selectable="true"
        :openByDefault="isGroupOpenByDefault"
        @ready="onGridReady"
        @cellDoubleClick="onCellDoubleClicked"
        @edit="onCellEdit"
        @rowGroupOpened="onRowGroupOpened"
      />
    </div>

    <div class="m2-foot">
      <div v-if="emptyDiagnosis && !parseError" class="m2-msg bad m2-invalid">
        <b>This table is empty, and it shouldn’t be.</b>
        <span>{{ emptyDiagnosis }}</span>
        <button class="m2-btn" @click="openConfig">Show the config</button>
      </div>
      <p v-else-if="rows.length === 0 && !parseError" class="m2-empty">
        Nothing configured yet. The <b>+ Data Source</b> button walks you through one.
      </p>
    </div>

    <!-- The panels, out of the shadow root: their components' scoped
         styles live in the head, and a modal belongs over the whole
         page anyway. -->
    <Teleport to="body">
      <div v-if="historyFor.length" class="m2-logs-backdrop" @click.self="historyFor = []">
        <div class="m2-logs m2-history" role="dialog" aria-modal="true" aria-label="Commit history">
          <header class="m2-logs-head">
            <div>
              <h3>{{ historyTitle }} — commit history</h3>
              <p>
                Each commit in {{ historyStoreNote }}, newest first; open one for what it did to
                each table.
                <span v-if="historyTruncated.length">
                  Only the newest commits are shown for
                  <code>{{ historyTruncated.join(", ") }}</code
                  >.
                </span>
              </p>
            </div>
            <button class="m2-btn" @click="historyFor = []">Close</button>
          </header>

          <p v-if="historyBusy && historyLines.length === 0" class="m2-logs-note">
            Reading the commit log…
          </p>
          <p v-else-if="historyError" class="m2-logs-note bad">{{ historyError }}</p>
          <p v-else-if="historyLines.length === 0" class="m2-logs-note">
            No doltlite store under <code>{{ historyStoreNote }}</code> yet. A step that has never
            run has written nothing, and the QMD index keeps no store of its own.
          </p>
          <div v-else class="m2-history-grid">
            <!-- The stores' commit log as a tree: stores open, commits
               closed until asked. -->
            <TableGrid
              :columns="historyColumns"
              :rows="historyLines"
              :tree="true"
              :openByDefault="(r: HistoryRow) => r.level === 'store'"
              :columnOverrides="historyOverrides"
            />
          </div>
        </div>
      </div>

      <SourceWizard
        v-if="wizardOpen"
        :key="wizardKey"
        :taken-ids="takenIds"
        :editing="editing"
        @close="closeWizard"
        @submit="onWizardSubmit"
      />
      <CompareDialog
        v-if="compareFor"
        :key="compareFor.id"
        :source="compareFor"
        :taken-ids="takenIds"
        @close="compareFor = null"
        @submit="onCompareSubmit"
      />
    </Teleport>
  </section>
</template>
