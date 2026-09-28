<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { TOPIC_CONFIG_WRITTEN, type CardCtx } from "./types";
import { UNIFIED_INDEX, type Action, type ManageResponse, type ManageRow } from "@/api";
import { useApi } from "@/cards/cardApi";
import {
  listGroups,
  listSteps,
  insertEntries,
  removeSteps,
  describeGroup,
  renameGroup,
  replaceSteps,
  sourceStepsOf,
  removedWith,
  setQmdSteps,
  qmdIndexingOf,
  type QmdIndexing,
  unwireFromFanIns,
  wireIntoFanIns,
  paramsAreRepresentable,
  entryForStep,
  emptyTableDiagnosis,
  DIFF_TYPE,
  type ConfiguredGroup,
  type ConfiguredStep,
  type SourceSteps,
  type StepPhase,
} from "@/config/sourceSteps";
import TableGrid from "./TableGrid.ce.vue";
import { ACTION_ICONS } from "./typedColumns";
import { syncAllButton } from "@/config/syncAll";
import type { TableGridApi } from "./tableGridApi";
import type { MenuEntry } from "@/grid/menu";
import { catalogForStep, type CatalogEntry } from "@/config/catalog";
import { ingestLabel } from "@/config/ingestMethods";
import { copyToClipboard } from "@/clipboard";
import { browseColumns, browseName, browseQuery } from "@/config/browsePresets";
import { logSource } from "./libs/logView";
import { historySource } from "./libs/historyView";
import { pushToast } from "@/toasts";
import {
  RAW_STORE_BROWSE_LABEL,
  notComparableReason,
  rowMenu,
  type MenuAction,
  type MenuTarget,
} from "@/config/rowMenu";
import { changed, subscribeLive } from "@/live";
import SourceWizard from "@/components/SourceWizard.vue";
import ConfirmDialog from "@/components/ConfirmDialog.vue";

const props = defineProps<{ ctx: CardCtx }>();
import {
  confirmAction,
  isDesktopApp,
  openRawStore,
  revealActionLabel,
  revealInFileManager,
} from "@/desktop";

const {
  fetchConfig,
  fetchConfigScaffold,
  saveConfig,
  fetchManageRows,
  fetchRequests,
  fetchRuns,
  openRequest,
  stopRequest,
  turnOffStep,
  turnOnStep,
  resetSteps,
  purgeGroups,
} = useApi();

props.ctx.setTitle("Sources");
props.ctx.setHelp(`
<p>Every top-level row is a <b>group</b> <code>config.toml</code> declares: a source
(Work Slack, Personal mail), or the unified index that makes them searchable. Open
its chevron for the <b>steps</b> that do the work — fetch, render, index — and the
<b>applets</b> the app spawns to serve it. Actions that don’t apply to a kind are
disabled and say why.</p>
<p>Each row says what its step is doing now. <b>Sync</b> on a source fetches what’s
new, then rebuilds everything downstream: its own steps, and the index every source
feeds. Each of those rows then shows the sync in its own <b>Last update</b> — queued,
and what it waits for; running; then how it ended — so pressing Sync on one row moves
others too. Syncs run side by side: a source synced while another syncs starts at
once.</p>
<p>On a step that reads another — a render, the index — Sync reruns it on what its
inputs already hold, then rebuilds everything downstream; nothing upstream runs. It
is offered while that step is out of date: its code, its settings or what it reads
changed since it last succeeded, as after an upgrade that changes how a source
renders.</p>
<p>While a row has work left in a sync, its Sync button is a <b>Stop</b> that names
the sync — “Stop the sync of Work Gmail” — and who started it, if not you. Each source
syncs on its own, even under Sync everything, so a source’s Stop stops that source.
The index every source feeds is part of each of their syncs, and its Stop stops all
of them. The steps in flight checkpoint what they have and exit; until they do the
button reads Stopping. The button at the top syncs everything, and while anything
syncs it stops everything.</p>
<p>The <b>switch</b> at the end of a row says whether it runs in syncs. Turned off,
every sync skips it, and if it is running it stops; what reads it waits. Turned back
on, it runs in the next sync — turning it on starts nothing by itself. On a group it
turns every step under it off or on. Hover it to see who turned it off.</p>
<p>A group row reads off its steps: <b>Last update</b> shows the liveliest of them —
running if any step is, else queued, off, failed or stopped if any is, and otherwise
the last step’s in pipeline order. <b>Last synced</b> and
<b>Last success</b> are the fetch step’s. <b>Remove</b> takes the steps and applets
with it.</p>
<p>Rows come in the order <code>config.toml</code> lists them. Click a header to sort by
that column; a third click puts the config's order back.</p>
<p><b>Name</b> stays in view while the table scrolls sideways. <b>Last update</b> leads
with an icon for what the row is doing or did last, then says when it got there. That
icon, and the mark before a name — the service a source mirrors, or what a step
does — give their word on hover. Hovering a name shows its id: for a group, the
folder its data is in.
<b>Right-click a header</b> to show or hide columns: <b>Last synced</b> and
<b>Last success</b> start hidden. <b>Double-click a Last update</b> to read that
step's log — from the run in flight while it runs, else from the run it last took
part in, with a picker for its other runs — as a grid you can sort, filter and
search; on a group row, the log of the step its status came from.
A <b>red or yellow number</b> after a name counts the errors (records dropped) and
warnings (records kept with something lost) its store holds as of its last run; a row
with none shows nothing. <b>Double-click the number</b> for the list.
<b>Activity</b> is what a running step has reported: how much is queued ahead of
it, what it has counted so far, and how many warnings and errors it has logged.</p>
<p><b>Browse</b>, <b>Sync</b> and the switch are on the row: they are what a row
does often. <b>Right-click a row</b> for everything it can do — sync, edit, rename,
the log, reveal, <b>Reset</b>, remove — and its <b>commit history</b>: every store
under it is versioned, and the history opens beside this card with each commit — when,
what it said, what it did to each table, and the run that made it — newest first,
updating while a sync runs. On a source, <b>Compare two versions</b> opens it ready to
compare the last two syncs.
Right-click inside a selection and the menu acts on all of it; outside one, on that
row alone, without changing the selection. An entry that doesn’t apply stays, greyed,
and says why on hover.</p>
<p><b>Reset</b> empties what a step holds: every row goes, and the history keeps
them, so a wrong click is a revert. Reset a source, or its download, and what reads
it catches up at once, so its documents leave the grid; the next Sync downloads it
all again from nothing. Reset a render and it renders its documents again from what
is downloaded, at once.</p>
<p><b>Documents</b> is how many things this source holds — what <b>Browse</b> opens —
counted over the whole store, not this run, by the render step: on its own row and on
the group above it. It moves while a render runs, each time the step seals what it has
written. A blank cell means nothing has counted yet; a source that renders no documents
of its own, like a photo library, counts zero.</p>
<p><b>Bytes on disk</b> is a directory walk over each row’s tree — a group’s is its
whole folder, measured on the same walk — plotted over the last few minutes, with its
change over that time beside the size. Each row’s line is scaled to its own range, so a
jump in a small source shows as plainly as one in a large one: the line is the shape of
the change, and the numbers are its size. Hover for the breakdown.</p>
<p><b>Last update</b> and <b>Last synced</b> are per step, read from the runner’s own
record — so a sync you or an agent start from a terminal shows up here too.
<b>Last success</b> is when the step last ran without failing: when it is older than
Last synced, every run since has failed, and a source's mirror is only known to match
upstream as of then. A step whose sync ended before it finished — the app was quit
mid-run, say — reads as <b>interrupted</b>.</p>
<p>The bar along the bottom of the app is the <b>whole data root</b>, not the sum of
the rows: it includes <code>system/</code> — the stores, the run log, the served
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
// The requests a banner is about, when it is about some. Such a banner
// retires once they have all closed, not on the next action.
const bannerRequests = ref<string[]>([]);

/// Put up a banner, optionally tying it to requests' lifetimes.
function say(ok: boolean, text: string, requestIds: string[] = []) {
  banner.value = { ok, text };
  bannerRequests.value = requestIds;
}

function clearBanner() {
  banner.value = null;
  bannerRequests.value = [];
}

/// Take down a request's banner once its requests have closed.
async function retireBanner() {
  const ids = bannerRequests.value;
  if (ids.length === 0) return;
  try {
    const open = (await fetchRequests()).some((r) => ids.includes(r.id) && r.state === "open");
    if (!open && bannerRequests.value === ids) clearBanner();
  } catch {
    // The banner stays until the next action.
  }
}
const busy = ref(false);
/// The rows, joined server-side from the config, the loop's record and
/// requests, the run store and the usage sampler — see
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
  qmdIndexing: QmdIndexing;
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
  /// The raw store Browse opens instead of a card: a download step's,
  /// in the desktop app, which is the only host that can open one.
  rawStore: string | null;
};

/// The tree the grid shows, as the server assembled it, with the
/// wizard's knowledge added per row.
const rows = computed<Row[]>(() => {
  const all = manage.value?.rows ?? [];
  const groups = new Map(all.filter((r) => r.kind === "group").map((g) => [g.id, g]));
  return all.map((r) => decorate(r, groups));
});

const syncAll = computed(() => syncAllButton(rows.value));

function decorate(r: ManageRow, groups: Map<string, ManageRow>): Row {
  // `system/` is not a config entry: nothing to edit, and Browse is
  // the run log over every run.
  if (r.kind === "system") {
    return {
      ...r,
      editBlocked: "Not a config entry.",
      editGroup: null,
      browseSource: browseAction(r)?.enabled ? "logView()" : null,
      rawStore: null,
    };
  }
  if (r.kind === "group") {
    const editBlocked = groupEditBlocked(r.id);
    return {
      ...r,
      editBlocked,
      editGroup: editBlocked ? null : r.id,
      browseSource: groupBrowse(r),
      rawStore: null,
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
  const group = r.group ? groups.get(r.group) : undefined;
  const ingestLabelled =
    r.group && r.phase === "ingest" ? ingestLabel(r.type?.id ?? null, r.params) : null;
  const rawStore = canReveal ? r.raw_store_path : null;
  return {
    ...r,
    name: ingestLabelled ? { ...r.name, label: ingestLabelled } : r.name,
    actions: rawStore
      ? r.actions.map((a) => (a.id === "browse" ? RAW_STORE_BROWSE : a))
      : r.actions,
    editBlocked,
    editGroup: editBlocked ? null : r.group,
    // A step's rows are its source's: Browse opens the group's view.
    browseSource: r.kind === "step" && group ? groupBrowse(group) : null,
    rawStore,
  };
}

/// Browse on a download step with a raw store: enabled whatever the
/// group's Browse says, since a source that renders nothing still has
/// the tables it downloaded.
const RAW_STORE_BROWSE: Action = {
  id: "browse",
  label: RAW_STORE_BROWSE_LABEL,
  enabled: true,
  hint:
    "Open what this step downloaded, read-only: in DB Browser for SQLite when that " +
    "opens .doltlite_db files here, otherwise in a doltlite shell.",
  disabled_reason: null,
};

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
  args.push(`name: ${JSON.stringify(browseName(g.name.label, type))}`);
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
    if (row.stop_request_ids.length > 0) void stopSyncs(row.stop_request_ids);
  },
  in_syncs: (row) => {
    const on = row.actions.find((a) => a.id === "in_syncs")?.on;
    void setTurnedOff([row], !!on);
  },
};

let gridApi: TableGridApi<Row> | null = null;
function onGridReady(api: TableGridApi<Row>) {
  gridApi = api;
}

/// Commit only the newest answer, whatever order the answers arrive in.
///
/// Not theoretical: rows fetched before a sync was asked for but landing after
/// the ones fetched once it was paint the row backwards.
/// `data-sources-sync`'s monotonicity test catches it.
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

// ── One step's log. A red status says *that* a step failed; the next
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
/// `problems` table, its search bar holding the row's source. A step's
/// problems are its group's — the render store is where a source's
/// live — so a step row opens the same grid as its group. The index
/// group shows every source's.
function openProblems(row: Row) {
  const sourceId = row.kind === "group" ? row.id : (row.group ?? row.id);
  const q = sourceId === "unified_index" ? "" : `source_id:${sourceId}`;
  const source =
    row.kind === "group" ? row : rows.value.find((r) => r.kind === "group" && r.id === sourceId);
  const name =
    sourceId === "unified_index" ? "Problems" : `Problems: ${source?.name.label ?? sourceId}`;
  const opts = {
    url: `${UNIFIED_INDEX}/problems`,
    q,
    name,
    placeholder: "search problems…  (try: severity:error, -stage:fetch, after:2026-01-01)",
  };
  props.ctx.host.openCards(`gridView(${JSON.stringify(opts)})`);
}

/// An in-place edit of the Name cell: a group's rename.
function onCellEdit(row: Row, field: string, value: string) {
  if (field === "name" && row.kind === "group") void renameRow(row, value);
}

/// Rows' commit history, as a card beside this one. On one source it is
/// also where two of its versions are compared; `compare` opens it with
/// the newest two set up.
function openHistory(targets: Row[], compare: boolean) {
  const [first] = targets;
  const source =
    targets.length === 1 && notComparableReason(menuTarget(first)) === null ? first.id : null;
  props.ctx.host.openCards(
    historySource({
      trees: targets.map((r) => r.id),
      title: targets.map((r) => r.name.label).join(", "),
      source,
      compare: compare && source !== null,
    }),
  );
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
    rawStore: row.rawStore !== null,
    stopRequestIds: row.stop_request_ids,
    turnedOffBy: row.turned_off_by,
    statusFrom: row.status_from,
    revealPath: row.reveal_path,
  };
}

function contextMenuItems(anchor: Row, targets: Row[]): MenuEntry[] {
  if (targets.length === 0) return [];
  return rowMenu(targets.map(menuTarget), { canReveal, revealLabel }).map((entry) =>
    entry.separator
      ? { name: "", separator: true }
      : {
          name: entry.name,
          disabled: entry.disabled,
          danger: ["remove", "reset"].includes(entry.action),
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
    case "stop":
      // One stop per request: several rows can be wanted by the same one.
      await stopSyncs([...new Set(targets.flatMap((t) => t.stop_request_ids))]);
      return;
    case "turn_off":
    case "turn_on":
      await setTurnedOff(targets, action === "turn_off");
      return;
    case "edit":
      if (first.editGroup) await openEdit(first.editGroup);
      return;
    case "compare":
      openHistory([first], true);
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
      openHistory(targets, false);
      return;
    case "reveal":
      for (const t of targets) await reveal(t.key);
      return;
    case "reset":
      await resetRows(targets);
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

function reparse() {
  try {
    sources.value = listSteps(configText.value);
    configGroups.value = listGroups(configText.value);
    parseError.value = null;
  } catch (e) {
    parseError.value = (e as Error).message;
  }
}

async function loadConfig() {
  // Cleared on success, not before the fetch: a banner that goes and
  // comes back on every reload moves the table twice.
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
    loadError.value = null;
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

const commitRows = freshest<ManageResponse>((m) => {
  manage.value = m;
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
/// The form reads the config as the server has it: a save from the
/// editor reaches this card by a pushed frame, and an Edit clicked before
/// that lands would open on the text from before it.
async function openEdit(groupId: string) {
  await loadConfig();
  const group = configGroups.value.find((g) => g.id === groupId);
  if (!group) return;
  const steps = sourceStepsOf(group.id, sources.value);
  const entry = groupEntry(group, steps);
  if (!entry) return;
  const qmdIndexing = steps.render ? qmdIndexingOf(sources.value, group.id) : "keyword_and_embed";
  editing.value = { group, entry, steps, qmdIndexing };
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
  qmdIndexing: QmdIndexing;
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
    next = insertEntries(
      configText.value,
      payload.groupBody ? `${payload.groupBody}\n\n${payload.stepsBody}` : payload.stepsBody,
    );
  }

  // The fan-ins name their inputs, so a render step added without this
  // renders happily and is never indexed. Idempotent, so re-saving an
  // edit doesn't duplicate the entry. Free-text search is the one index
  // the wizard asks about: the source's own qmd steps, added or taken out.
  if (payload.renderId) {
    next = wireIntoFanIns(next, payload.renderId);
    next = setQmdSteps(next, payload.id, payload.qmdIndexing);
  } else {
    next = setQmdSteps(next, payload.id, "none");
  }

  // Banners are for a person, so they say the name; the id is what the
  // config and the disk use.
  const shown = payload.name || payload.id;
  const ok = await writeConfig(next, current ? `Saved ${shown}.` : `Added ${shown}.`);
  if (!ok) return;
  closeWizard();
}

// ── Removing. A comparison's tree is computed from two commits its
// source keeps, so unlike a download it is worth nothing kept: the
// question offers to delete it, checked by default. Anything else keeps
// its data, and gets the platform's plain confirm.
const asking = ref<{
  message: string;
  checkLabel: string;
  resolve: (answer: { ok: boolean; checked: boolean }) => void;
} | null>(null);

function comparisonQuestion(name: string): string {
  return `Remove the comparison "${name}" from the config?`;
}

/// The groups whose trees go with the removal, or null for Cancel.
async function confirmRemoval(what: string, leaving: ConfiguredGroup[]): Promise<string[] | null> {
  const comparisons = leaving.filter((g) => g.type === DIFF_TYPE);
  if (comparisons.length === 0) return (await confirmAction(what)) ? [] : null;
  const names = comparisons.map((g) => `"${g.name ?? g.id}"`).join(", ");
  const answer = await new Promise<{ ok: boolean; checked: boolean }>((resolve) => {
    asking.value = {
      message: what,
      checkLabel:
        `Also delete the computed changes of ${names} from disk. ` +
        `Comparing the same two syncs again rebuilds them.`,
      resolve,
    };
  });
  asking.value = null;
  if (!answer.ok) return null;
  return answer.checked ? comparisons.map((g) => g.id) : [];
}

async function removeAndPurge(next: string, what: string, purge: string[]) {
  const ok = await writeConfig(next, what);
  if (!ok || purge.length === 0) return;
  try {
    const done = await purgeGroups(purge);
    banner.value = {
      ok: true,
      text:
        done === "done"
          ? `${what} The computed changes are deleted.`
          : `${what} The computed changes are deleted once the sync in progress is over.`,
    };
  } catch (e) {
    banner.value = {
      ok: false,
      text: `${what} The computed changes were not deleted: ${(e as Error).message}`,
    };
  }
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
  const readers = removedWith([step.id], sources.value).filter((r) => r.id !== sibling?.id);
  const doomed = [step, ...(sibling ? [sibling] : []), ...readers];
  const alsoGone = readers.length
    ? `\n\nThese go with it: ${readers.map((r) => `"${r.name}"`).join(", ")}.`
    : "";

  // A group with nothing left under it goes too: the loader would only
  // warn about it, but a `[[groups]]` entry naming a source that is
  // gone is litter someone has to explain.
  const goneIds = new Set(doomed.map((d) => d.id));
  const emptied = configGroups.value.filter(
    (g) =>
      step.group === g.id && !sources.value.some((s) => s.group === g.id && !goneIds.has(s.id)),
  );

  const what = emptied.some((g) => g.type === DIFF_TYPE)
    ? comparisonQuestion(name)
    : step.kind === "applet"
      ? `Remove the "${name}" applet from the config?\n\n` +
        `The server stops it. Anything in the app that its components or endpoints ` +
        `serve will stop working until you add it back.`
      : step.phase === "index"
        ? `Remove the "${name}" index step from the config?\n\n` +
          `Its output stays on disk but stops being refreshed, so search results go stale.` +
          alsoGone
        : sibling
          ? `Remove "${name}" and the render step that reads it ("${sibling.name}")?\n\n` +
            `Both have to go together: a render step whose input is gone is a config ` +
            `datalib refuses to load.${alsoGone}\n\n` +
            `The data stays on disk. Re-adding later resumes from what's already there.`
          : `Remove "${name}" from the config?${alsoGone}\n\n` +
            `Its data stays on disk — only this step stops running. Re-adding it later ` +
            `resumes from what's already there.`;
  const purge = await confirmRemoval(what, emptied);
  if (!purge) return;

  // Cut first: the entries' offsets are into the text as parsed, and
  // unwiring a fan-in above the source would shift them. Unwiring is a
  // regex over the result, so it needs no offsets.
  let next = removeSteps(configText.value, [...doomed, ...emptied]);
  for (const d of doomed) next = unwireFromFanIns(next, d.id);
  await removeAndPurge(next, `Removed ${name}.`, purge);
}

/// Remove a group with everything filed under it. Its render steps
/// leave the fan-ins too, or the config would name inputs that no
/// longer exist and the loader would refuse the whole file.
async function deleteGroup(id: string) {
  const group = configGroups.value.find((g) => g.id === id);
  if (!group) return;
  const name = group.name ?? group.id;
  const inGroup = sources.value.filter((s) => s.group === id);
  const members = [
    ...inGroup,
    ...removedWith(
      inGroup.map((m) => m.id),
      sources.value,
    ),
  ];
  const steps = members.filter((s) => s.kind === "step").length;
  const applets = members.filter((s) => s.kind === "applet").length;
  const count = (n: number, word: string) => `${n} ${word}${n === 1 ? "" : "s"}`;
  const under = [steps ? count(steps, "step") : "", applets ? count(applets, "applet") : ""]
    .filter(Boolean)
    .join(" and ");
  const what =
    group.type === DIFF_TYPE
      ? comparisonQuestion(name)
      : `Remove "${name}" from the config${under ? `, with the ${under} under it` : ""}?\n\n` +
        `The data stays on disk — these entries just stop running. Adding the source ` +
        `back later resumes from what's already there.`;
  const purge = await confirmRemoval(what, [group]);
  if (!purge) return;

  let next = removeSteps(configText.value, [...members, group]);
  for (const m of members) next = unwireFromFanIns(next, m.id);
  await removeAndPurge(next, `Removed ${name}.`, purge);
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
  for (const r of removedWith([...doomed.keys()], sources.value)) doomed.set(r.id, r);
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
  const entries = [...doomed.values()];
  const leaving = [...doomed.entries()]
    .filter(([key]) => key.startsWith("group:"))
    .map(([, g]) => g as ConfiguredGroup);
  const purge = await confirmRemoval(what, leaving);
  if (!purge) return;
  let next = removeSteps(configText.value, entries);
  for (const d of entries) {
    if ("phase" in d) next = unwireFromFanIns(next, d.id);
  }
  await removeAndPurge(next, `Removed ${targets.length} entries.`, purge);
}

/// Leave the Manage screen for this row's data: one card, the grid,
/// already filtered to the source and carrying its type's columns — or,
/// for a download step in the app, its raw store in another program.
///
/// A card stack IS the URL (see router/columns.ts), so this is an
/// ordinary navigation — the card is bookmarkable, shareable, and the
/// back button returns here.
function openBrowse(row: Row) {
  if (row.rawStore) {
    void browseRawStore(row.rawStore);
    return;
  }
  if (!row.browseSource) return;
  // Beside this card, in whatever layout is showing it.
  props.ctx.host.openCards(row.browseSource);
}

async function browseRawStore(path: string) {
  const res = await openRawStore(path);
  banner.value = res.ok
    ? { ok: true, text: `Opened ${path} read-only in ${res.openedIn}.` }
    : { ok: false, text: `Could not open ${path}: ${res.reason}` };
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
/// are its source steps, all in one request.
function runRow(row: Row) {
  return runRows([row]);
}

/// Several rows at once, so their downstream steps run once. The server
/// opens one request per source among them, each with its own Stop.
async function runRows(targets: Row[]) {
  const seeds = [...new Set(targets.flatMap((r) => r.seeds))];
  if (seeds.length === 0) return;
  const shown = targets
    .map((row) => {
      const step = sources.value.find((s) => s.id === row.id);
      return row.kind === "group" ? row.name.label : (step?.name ?? row.id);
    })
    .join(", ");
  await queueSync(seeds, shown);
}

async function queueSync(seeds: string[], shown: string) {
  busy.value = true;
  clearBanner();
  try {
    const requests = await openRequest(seeds);
    say(
      true,
      `Queued a sync for ${shown}.`,
      requests.map((r) => r.id),
    );
    // The loop's record moving refetches too; this is for a page whose
    // stream is down.
    await loadRows();
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  } finally {
    busy.value = false;
  }
}

/// The steps a reset of these rows empties: a step is itself; a group is
/// its download, what it renders following — or, for a comparison, which
/// downloads nothing, its render. A download's blob store keeps its
/// bytes (`docs/dev/step_protocol.md` § Reset).
function resetTargets(targets: Row[]): string[] {
  const steps = targets.flatMap((t) => {
    if (t.kind !== "group") return [t];
    const under = stepsUnder(t);
    const downloads = under.filter((r) => r.function === "ingest");
    return downloads.length ? downloads : under.filter((r) => r.function === "render_markdown");
  });
  const ids = steps
    .filter((r) => r.function === "ingest" || r.function === "render_markdown")
    .map((r) => r.id);
  return [...new Set(ids)];
}

/// Empty what these rows wrote, keeping the history. A render is rebuilt
/// from what it reads at once; a download is not refilled, but what reads
/// it catches up, so its documents leave the grid
/// (`docs/dev/plans/supervisor.md` §2.10). The server runs it once no sync
/// is running, and refuses it while one is.
async function resetRows(targets: Row[]) {
  const ids = resetTargets(targets);
  const shown = targets.map((t) => t.name.label).join(", ");
  if (ids.length === 0) {
    say(false, `Nothing under ${shown} keeps anything to reset.`);
    return;
  }
  const download = ids.some((id) => rows.value.find((r) => r.id === id)?.function === "ingest");
  const what =
    `Reset ${shown}?\n\n` +
    `Every row goes, and the history keeps them. ` +
    (download
      ? `Its documents leave the grid, and the next Sync downloads it all again from nothing. ` +
        `Attachments already downloaded are kept.`
      : `Its documents are rendered again from what it has downloaded, now.`);
  if (!(await confirmAction(what))) return;
  busy.value = true;
  clearBanner();
  try {
    say(true, `Resetting ${shown}…`);
    await resetSteps(ids);
    say(true, `Reset ${shown}.`);
    await loadRows(true);
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  } finally {
    busy.value = false;
  }
}

/// Sync everything the config declares: one request per source, in one
/// run.
async function runEverything() {
  busy.value = true;
  clearBanner();
  try {
    const requests = await openRequest([]);
    say(
      true,
      "Queued a sync of everything.",
      requests.map((r) => r.id),
    );
    await loadRows();
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  } finally {
    busy.value = false;
  }
}

/// Stop requests. Their steps checkpoint and exit; the rows say Stopping
/// until they have.
async function stopSyncs(requestIds: string[]) {
  if (requestIds.length === 0) return;
  busy.value = true;
  clearBanner();
  try {
    for (const id of requestIds) await stopRequest(id);
    say(
      true,
      `Stopping ${requestIds.length === 1 ? "the sync" : `${requestIds.length} syncs`}. ` +
        "Steps in flight checkpoint what they have and exit.",
      requestIds,
    );
    await loadRows();
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  } finally {
    busy.value = false;
  }
}

/// The steps a group row stands for.
function stepsUnder(group: ManageRow): ManageRow[] {
  return rows.value.filter((r) => r.kind === "step" && r.group === group.id);
}

/// Turn off or on what these rows stand for: a step itself, a group
/// every step under it.
async function setTurnedOff(targets: Row[], off: boolean) {
  const steps = targets.flatMap((t) => (t.kind === "group" ? stepsUnder(t) : [t]));
  busy.value = true;
  clearBanner();
  try {
    for (const s of steps) await (off ? turnOffStep(s.id) : turnOnStep(s.id));
    await loadRows();
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  } finally {
    busy.value = false;
  }
}

let unsubscribe: (() => void) | null = null;
const cardEl = ref<HTMLElement | null>(null);

/// Everything this table shows, refetched together. The rows come from
/// the server with the config and the loop's record already joined.
async function reloadAll(freshStorage = false) {
  await Promise.all([loadConfig(), loadRows(freshStorage)]);
}

onMounted(async () => {
  // Fresh sizes on the first paint. The backend only walks the disk
  // while a run is in flight, so on an idle root — the usual state —
  // this is the walk that produces the numbers on screen.
  await reloadAll(true);

  unsubscribe = subscribeLive(
    {
      root: (e) => {
        if (changed(e, "manage.rows")) {
          // Deliberately *not* a fresh walk: this fires once a second while
          // a run is going. The sampler is already walking on its own
          // cadence; this just reads what it found.
          void loadRows();
        }
        // The loop's record moving is the nearest thing to "a step
        // committed" — nothing watches the stores themselves — and its
        // requests live beside it.
        if (changed(e, "dag")) void retireBanner();
        if (e.kind === "config_changed") {
          // Config and record together, for the "Never run" reason above.
          void reloadAll();
        }
      },
      // A reconnect means we may have slept through a whole run, and the
      // sampler's own last walk with it. Ask for a fresh one.
      resync: () => void reloadAll(true),
    },
    { onScreen: cardEl.value ?? undefined },
  );
});

onUnmounted(() => {
  unsubscribe?.();
  unsubscribe = null;
  gridApi = null;
});
</script>

<template>
  <section ref="cardEl" class="m2 m2-card">
    <header class="m2-head">
      <!-- Here rather than above the table: it comes and goes with each
           sync, and a line appearing above the table moves every row
           under the pointer. -->
      <p
        v-if="banner"
        class="m2-msg m2-head-msg"
        :class="banner.ok ? 'good' : 'bad'"
        :title="banner.text"
        role="status"
      >
        {{ banner.text }}
      </p>
      <div class="m2-head-actions">
        <button
          class="m2-btn m2-runall"
          :class="{ danger: syncAll.glyph === 'stop' }"
          :disabled="busy || !!parseError || !!configError || !!syncAll.blocked"
          :title="syncAll.blocked ?? syncAll.label"
          :aria-label="syncAll.label"
          @click="syncAll.stops.length > 0 ? stopSyncs(syncAll.stops) : runEverything()"
        >
          <svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true">
            <path fill="currentColor" :d="ACTION_ICONS[syncAll.glyph]" />
          </svg>
        </button>
        <button class="m2-add" :disabled="busy || !!parseError || !!configError" @click="openAdd">
          + Data Source
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
         Last update column carries each reason; this says how many and where
         to look, because a dropped row is easy to scroll past. -->
    <div v-else-if="droppedRows.length" class="m2-msg bad m2-invalid">
      <b>
        {{ droppedRows.length }}
        {{ droppedRows.length === 1 ? "entry isn’t" : "entries aren’t" }} in the pipeline.
      </b>
      <span class="m2-invalid-why">
        The rest of this config loaded and still syncs. These are in the file and were not loaded —
        each one’s Last update cell says why. Open the config to fix them, or run
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

    <div class="m2-grid">
      <!-- The typed viewer over the rows the server assembled: a tree,
           one group row with its steps and applets under it. Every row
           is rendered: a config is tens of rows, and a source just
           added lands at the bottom, where a virtualized grid would
           have no row for it until scrolled to. -->
      <TableGrid
        :columns="manage?.columns ?? []"
        :rows="rows"
        :tree="true"
        :virtualizeRows="false"
        :windowSecs="storage?.window_secs ?? 300"
        :actions="rowActions"
        :menu="contextMenuItems"
        :selectable="true"
        :openByDefault="isGroupOpenByDefault"
        :pinnedColumns="1"
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
      <SourceWizard
        v-if="wizardOpen"
        :key="wizardKey"
        :taken-ids="takenIds"
        :editing="editing"
        @close="closeWizard"
        @submit="onWizardSubmit"
      />
      <ConfirmDialog
        v-if="asking"
        title="Remove"
        :message="asking.message"
        :check-label="asking.checkLabel"
        confirm-label="Remove"
        @answer="asking.resolve"
      />
    </Teleport>
  </section>
</template>
