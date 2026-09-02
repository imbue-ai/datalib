<script setup lang="ts">
// Manager2 — the Manage tab inverted, per docs/dev/source_wizard.md.
//
// A grid of everything the config declares is the page — sources, the
// shared index steps, and applets — because the thing being managed is
// the pipeline, not only the data. "Add Data Source" sits above it;
// each row carries Run / Edit / Delete, plus Reveal in the desktop app,
// with each action disabled per kind and saying why. The raw config
// editor is here but collapsed — demoted, not removed, because it stays
// the source of truth and the wizard's "edit as TOML" escape hatch has
// to lead somewhere.
//
// Everything is derived from the config text, which stays the single
// source of truth: rows come from parsing it, and add/edit/delete
// splice it and PUT it back through the same endpoint the editor uses.
//
// Columns that need backend work the design calls for, and which are
// therefore absent rather than faked: Account (needs the latchkey
// endpoints) and Documents (must come from the unified_index applet —
// the layout now forbids datalib-http reading that tree).
//
// "Last synced" / "Last status" come from the *runner's* own per-step
// record (`GET /api/dag`), not from the job queue. The queue is per
// *run*, and a run routinely covers several steps, so it could only
// ever attribute one timestamp and one status to all of them — which is
// what the old `~` marker was apologizing for.
//
// Reading the runner's record has a second consequence worth knowing:
// a sync started from a terminal shows up here, because `datalib-dag`
// writes that record whoever spawned it. The job half of the SSE stream
// only carries runs this server started; the `root` half carries the
// record moving whoever wrote it, which is how that terminal run
// reaches this table.
import { computed, onMounted, onUnmounted, ref } from "vue";
import { AgGridVue } from "ag-grid-vue3";
import {
  ModuleRegistry,
  AllCommunityModule,
  themeQuartz,
  colorSchemeVariable,
  type ColDef,
  type GridApi,
  type GridReadyEvent,
  type ICellRendererParams,
  type ValueGetterParams,
} from "ag-grid-community";
import {
  fetchConfig,
  fetchConfigScaffold,
  saveConfig,
  fetchAllJobs,
  fetchDag,
  fetchJobLog,
  fetchPipelineStorage,
  fetchFrontend,
  enqueueJob,
  cancelJob,
  type DagRun,
  type DagStep,
  type DagStepProgress,
  type SyncJob,
  type SyncJobState,
  type SyncTask,
  type JobProgressEvent,
  type OutputStorage,
} from "@/api";
import {
  listSteps,
  type EntryKind,
  appendSource,
  phaseOf,
  removeSteps,
  renderIdFor,
  replaceStep,
  stemOf,
  unwireFromFanIns,
  wireIntoFanIns,
  paramsAreRepresentable,
  emptyTableDiagnosis,
  type ConfiguredStep,
  type StepPhase,
} from "@/config/sourceSteps";
import { catalogFor, type CatalogEntry } from "@/config/catalog";
import { iconUrl } from "@/config/icons";
import { STEP_GLYPHS, STATUS_GLYPHS, glyphSvg } from "@/config/glyphs";
import { stepLogLines, type StepLogLine } from "@/config/stepLog";
import { compareStamps, formatRelative, formatStamp } from "@/config/timeFormat";
import {
  claimedBy as claimedByJob,
  sourcesFeeding as sourcesFeedingIn,
  stepStatus as statusOf,
  waitingOn,
  pushedOverlay,
  boardWentTerminal,
  withOverlay,
  effectiveRun,
  type Overlay,
  type StatusView,
} from "@/config/pipelineStatus";
import { subscribeLive } from "@/live";
import SourceWizard from "@/components/SourceWizard.vue";
import { isDesktopApp, revealActionLabel, revealInFileManager } from "@/desktop";

ModuleRegistry.registerModules([AllCommunityModule]);
const gridTheme = themeQuartz.withPart(colorSchemeVariable);

const configText = ref("");
const configPath = ref("");
// Two independent verdicts on the config, and both matter.
//
// `parseError` is our own TOML parse — it is what stops the grid from
// showing nonsense, and it is all we have while the Advanced editor
// holds unsaved text.
//
// `configError` is the *backend's*, from `GET /api/config`, produced by
// the real loader: it catches everything the runner would reject —
// duplicate step ids, reserved stanza names, bad artifact patterns,
// cycles — none of which is a TOML syntax error, so none of which our
// parse can see. When the file on disk is broken this is the message
// worth showing.
const parseError = ref<string | null>(null);
const configError = ref<string | null>(null);
// What the backend's own loader made of the same file. Held so the
// empty state can cross-check itself against it — see
// `emptyTableDiagnosis`.
const serverSourceCount = ref(0);
const configExists = ref(false);
const loadError = ref<string | null>(null);
const banner = ref<{ ok: boolean; text: string } | null>(null);
/// The job a banner is *about*, when it is about one.
///
/// "Queued a sync for Signal (Work)." is true for a few seconds and
/// then isn't, and nothing was taking it down: the banner only ever
/// cleared on the next action, so a finished sync left the page
/// insisting one was still queued. Holding the job id lets the banner
/// retire itself the moment that job stops running — the grid's Status
/// column is what says how it went, and it says so per step, which the
/// banner never could.
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

/// Take down a job-scoped banner once its job has stopped running.
function retireBanner(jobId: string, state: SyncJobState) {
  if (bannerJob.value !== jobId) return;
  if (state === "pending" || state === "running") return;
  clearBanner();
}
const busy = ref(false);
const jobs = ref<SyncJob[]>([]);
const storage = ref<OutputStorage[]>([]);
/// The runner's own per-step record, from `GET /api/dag`. What makes
/// "last synced" and "last status" exact per step — and what makes a
/// run started from a terminal visible here at all, since the runner
/// writes it whoever spawned it.
const dagSteps = ref<Record<string, DagStep>>({});
const dagRun = ref<DagRun | null>(null);
/// applet id → why it failed to start, from `GET /api/frontend`. An
/// applet that won't come up is otherwise only visible as a 502 from
/// whatever tab needed it.
const appletErrors = ref<Record<string, string>>({});
const sources = ref<ConfiguredStep[]>([]);

// The Advanced disclosure. Closed on load: the point of this tab is
// that a text editor is not the first thing you meet.
const configOpen = ref(false);
const configDirty = ref(false);

// Resolved once — the desktop bridge either exists for this window or
// it doesn't, and the label depends only on the platform.
const canReveal = isDesktopApp();
const revealLabel = revealActionLabel();

const wizardOpen = ref(false);
/// Bumped on every opening, and bound to the dialog's `key`.
///
/// Without it the chained "also render this?" flow writes the wrong
/// step. `onWizardSubmit` closes the dialog and reopens it for the
/// render step in one synchronous stretch — `window.confirm` blocks
/// the event loop, so `wizardOpen` goes false and back to true inside
/// a single tick. Vue never flushes the false, so `v-if` never
/// unmounts, the component instance is *reused*, and every `ref` the
/// wizard initialises in `setup()` keeps the value it had while
/// creating the fetch step. The id is the one that matters: the render
/// step was written as `signal-work` — the create-mode stem — instead
/// of `signal-work/rendered_md`, which the runner then rejects with
/// "a step writes only the tree its id names", and which `phaseOf`
/// reads as `other`, so it was never wired into the fan-ins either.
/// A key that changes forces the remount the flow was assuming.
const wizardKey = ref(0);
const editing = ref<{ step: ConfiguredStep; entry: CatalogEntry } | null>(null);
/// Set while the wizard is being used to add the render step for a
/// fetch step that was just written (or picked from a row action).
const renderFor = ref<{ fetchId: string; fetchName: string; entry: CatalogEntry } | null>(
  null,
);

// Only source names gate the wizard: an applet id and a stanza name
// live in different namespaces and may safely coincide.
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

/// Stems already spoken for. A new step reserves a whole tree
/// (`work-slack/` covers both `work-slack/raw` and its render sibling),
/// so collisions are checked on the stem rather than the full id.
const takenIds = computed(
  () => new Set(sources.value.filter((s) => s.kind === "step").map((s) => stemOf(s.id))),
);

/// The render step that reads a given fetch step, if the config has
/// one. What decides whether a fetch row offers "render to markdown",
/// and what delete has to take with it.
function renderSiblingOf(fetchId: string): ConfiguredStep | undefined {
  return sources.value.find(
    (s) => s.kind === "step" && s.inputs.includes(fetchId) && s.phase === "render",
  );
}

type Row = {
  /// Identity: the tree this entry writes, and what every action here
  /// is keyed on.
  id: string;
  kind: EntryKind;
  phase: StepPhase;
  /// The word behind the step-role glyph that follows the name —
  /// its `title`, and its accessible name. The only place the word
  /// survives now that the mark has no column of its own.
  kindLabel: string;
  type: string | null;
  /// What to show in the Name column. Equal to `id` until someone sets
  /// a `name =` on one of this entry's steps.
  name: string;
  /// The catalog's name for the provider ("Slack"), shown under Type —
  /// a property of the entry's type, not of this entry.
  typeLabel: string;
  icon: string | null;
  entry: CatalogEntry | undefined;
  /// Null when the action applies to this row; otherwise the reason it
  /// doesn't, which becomes the disabled button's tooltip.
  runBlocked: string | null;
  editBlocked: string | null;
  renderBlocked: string | null;
  revealBlocked: string | null;
  lastSynced: string | null;
  status: StatusView;
  /// The active job that has claimed this step, when one has. Non-null
  /// is exactly the condition that turns Run into Stop: work is already
  /// queued or in flight for this row, so the useful button is the one
  /// that calls it off.
  stopJobId: string | null;
  /// Live position in the run currently in flight, from the progress
  /// bus. Null when the step isn't running or hasn't reported anything.
  progress: DagStepProgress | null;
  /// Null when nothing is on disk yet — rendered as "—", not "0 B",
  /// which would read as "ran, and produced nothing".
  bytes: number | null;
  /// Storage rows for this entry's declared outputs.
  outputs: OutputStorage[];
  /// Absolute path to reveal: the first output that exists.
  revealPath: string | null;
};

/// The word behind a row's step-role glyph. A step is labelled by its
/// phase rather than the word "step", because that is the distinction a
/// reader actually wants: which of these brings data in, which turns it
/// into markdown, which is shared index plumbing.
const PHASE_LABEL: Record<StepPhase, string> = {
  fetch: "Fetch",
  render: "Render",
  index: "Index",
  other: "Step",
};

/// The DAG edges and the claimed-step map, recomputed whenever the
/// config or the queue moves. The logic itself is in
/// `config/pipelineStatus.ts`, where it is testable without a grid.
const claimedBy = computed(() => claimedByJob(sources.value, jobs.value));

/// Per-step state from the newest pushed task board, overlaid on what
/// the last `/api/dag` fetch knew.
///
/// This is what makes the grid react rather than wait. The worker
/// publishes the board over SSE within 400 ms of any change — and the
/// enqueue and cancel handlers publish the instant they write a job
/// row — so a step going running reaches this ref without a round trip.
/// Cleared when a run ends, since a board outlives nothing.
const liveTasks = ref<Record<string, Overlay>>({});

/// The job the pushed board belongs to. Only one job runs at a time
/// (the worker claims them one by one), so this is unambiguous.
const liveJob = computed(() => jobs.value.find((j) => j.state === "running"));

/// Has this step reached a terminal state in the run now in flight?
///
/// A step with no state has not been reached; one that is `running` is
/// still going. Everything else the runner reports — including
/// `not_selected` — means it will not move again this run, which is
/// what "no longer blocking anything downstream" means.
function finishedThisRun(id: string): boolean {
  const state = withOverlay(dagSteps.value[id], id, liveTasks.value[id])?.current_state;
  return !!state && state !== "running";
}

function stepStatus(id: string): StatusView {
  return statusOf({
    id,
    step: withOverlay(dagSteps.value[id], id, liveTasks.value[id]),
    run: effectiveRun(dagRun.value, liveJob.value),
    claim: claimedBy.value.get(id),
    waitingOn: waitingOn(sources.value, id, finishedThisRun),
  });
}

const rows = computed<Row[]>(() =>
  sources.value.map((s) => {
    const entry = s.type ? catalogFor(s.type) : undefined;
    const run = s.kind === "applet" ? null : stepStatus(s.id);
    // A step writes exactly one tree, and it is the step's id.
    const outputs =
      s.kind === "applet"
        ? []
        : [storage.value.find((x) => x.path === s.id)].filter(
            (x): x is OutputStorage => !!x,
          );
    const onDisk = outputs.filter((o) => o.present);

    // Run: a sync is started at a *source* step — one with no declared
    // inputs — and everything downstream follows change propagation.
    // `datalib-dag` rejects a `--sync` naming anything else outright
    // ("not a source step: …"), so offering the button on a render or
    // index row would only ever queue a job that fails on startup.
    // Naming the sources that would carry it is the useful half of
    // saying no.
    const seeds = s.kind === "step" ? sourcesFeedingIn(sources.value, s.id) : [];
    const runBlocked =
      s.kind === "applet"
        ? "Applets aren't scheduled — the server starts one when something asks for it."
        : s.inputs.length === 0
          ? null
          : seeds.length === 1
            ? `A sync starts at a source step. Run ${seeds[0]} — this runs with it.`
            : `A sync starts at a source step. This one runs whenever any of its ` +
              `sources does: ${seeds.join(", ") || "none it can reach"}.`;

    // Edit: the wizard's forms describe provider steps. Everything else
    // is hand-written config, and the honest answer is to say so.
    let editBlocked: string | null = null;
    if (s.kind === "applet") {
      editBlocked = "No form for applets — edit this one in Advanced below.";
    } else if (s.phase === "index") {
      editBlocked = "A shared index step has no options — its inputs are its whole config.";
    } else if (!entry) {
      editBlocked = "This step isn't a datalib-step command the catalog knows.";
    } else if (!entry.wizard) {
      editBlocked = `No guided form for ${entry.label} yet — edit it in Advanced below.`;
    } else {
      const rep = paramsAreRepresentable(s, entry);
      if (!rep.ok) {
        editBlocked =
          `The form doesn't model ${rep.unknown.join(", ")}, and saving would drop it. ` +
          `Edit this one in Advanced below.`;
      }
    }

    // "Render to markdown": offered on a fetch step that has no render
    // step reading it yet, for a provider that renders at all.
    let renderBlocked: string | null = null;
    if (s.kind !== "step" || s.phase !== "fetch") {
      renderBlocked = "Only a fetch step can have a render step added to it.";
    } else if (!entry?.wizard) {
      renderBlocked = "No guided form for this type — add the render step in Advanced below.";
    } else if (entry.renderStep === false) {
      renderBlocked = `${entry.label} produces no markdown to render.`;
    } else if (renderSiblingOf(s.id)) {
      renderBlocked = "This already has a render step.";
    }

    const revealBlocked =
      s.kind === "applet"
        ? "An applet owns no files — it serves endpoints."
        : onDisk.length === 0
          ? "Nothing on disk yet — this hasn't produced anything."
          : null;

    // An applet's health is its own thing: it isn't scheduled, so the
    // runner's record says nothing about it. `GET /api/frontend` does.
    // It is up or it is not — there is no history to show, which is why
    // the timestamp stays null on these rows rather than borrowing one.
    const appletErr = appletErrors.value[s.id];
    const status: StatusView =
      s.kind === "applet"
        ? appletErr
          ? { key: "failed", label: "Failed to start", at: null, detail: appletErr }
          : { key: "succeeded", label: "Up", at: null, detail: "The gateway has this applet up." }
        : run!;

    return {
      id: s.id,
      kind: s.kind,
      phase: s.phase,
      kindLabel: s.kind === "applet" ? "Applet" : PHASE_LABEL[s.phase],
      type: s.type,
      name: s.name,
      typeLabel: entry?.label ?? s.type ?? "—",
      icon: entry?.icon ?? null,
      entry,
      runBlocked,
      editBlocked,
      renderBlocked,
      revealBlocked,
      lastSynced: status.at,
      status,
      stopJobId: claimedBy.value.get(s.id)?.id ?? null,
      progress: s.kind === "applet" ? null : (dagSteps.value[s.id]?.progress ?? null),
      bytes: onDisk.length ? outputs.reduce((n, o) => n + o.bytes, 0) : null,
      outputs,
      revealPath: onDisk[0]?.abs ?? null,
    };
  }),
);

/// Base-10 units, matching what a file manager shows — the question
/// behind this column is "how much of my disk is this", not a precise
/// block count.
function formatBytes(n: number): string {
  if (n < 1000) return `${n} B`;
  const units = ["kB", "MB", "GB", "TB"];
  let v = n / 1000;
  let i = 0;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i++;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

/// The largest value in the Bytes column, which is what every bar is
/// drawn against. Zero when nothing is on disk, and the renderer draws
/// no bar at all rather than dividing by it.
const maxBytes = computed(() =>
  rows.value.reduce((m, r) => Math.max(m, r.bytes ?? 0), 0),
);

/// 24×24 Material-ish glyphs, drawn in `currentColor` so they follow the
/// button's own colour through hover, disabled and the dark theme.
const ICON_PATHS: Record<string, string> = {
  run: "M8 5v14l11-7z",
  // The play button's other face. A row whose work is already queued or
  // in flight can't usefully be started again, so the button becomes
  // the one thing left to do with it.
  stop: "M6 6h12v12H6z",
  edit: "M3 17.25V21h3.75L17.81 9.94l-3.75-3.75L3 17.25zM20.71 7.04a1 1 0 0 0 0-1.41l-2.34-2.34a1 1 0 0 0-1.41 0l-1.83 1.83 3.75 3.75 1.83-1.83z",
  reveal: "M10 4H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-8l-2-2z",
  trash: "M6 19a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2V7H6v12zM19 4h-3.5l-1-1h-5l-1 1H5v2h14V4z",
  // "add a render step": a document with a plus.
  render:
    "M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8l-6-6zm-1 9h3v2h-3v3h-2v-3H8v-2h3V8h2v3z",
};

/// An icon button for the Actions cell.
///
/// The label is the native `title` tooltip *and* the accessible name —
/// an icon with neither is a guess, and this row has four of them. When
/// disabled the tooltip becomes the reason, which is the thing worth
/// reading.
function iconButton(
  icon: keyof typeof ICON_PATHS,
  label: string,
  disabledWhy: string | null,
  danger: boolean,
  onClick: () => void,
): HTMLButtonElement {
  const b = document.createElement("button");
  b.className = `m2-icon-btn${danger ? " danger" : ""}`;
  b.title = disabledWhy ?? label;
  b.setAttribute("aria-label", label);
  if (disabledWhy) b.disabled = true;
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("width", "15");
  svg.setAttribute("height", "15");
  svg.setAttribute("aria-hidden", "true");
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("d", ICON_PATHS[icon]);
  path.setAttribute("fill", "currentColor");
  svg.appendChild(path);
  b.appendChild(svg);
  b.addEventListener("click", (e) => {
    e.stopPropagation();
    onClick();
  });
  return b;
}

const columnDefs: ColDef<Row>[] = [
  {
    headerName: "Name",
    field: "name",
    // The only flexing column: it absorbs slack on a wide window, and
    // stops shrinking at a width a stanza name still fits in.
    flex: 1,
    minWidth: 200,
    // The label leads and the directory name follows it, muted,
    // whenever they differ. Showing only the label would hide which
    // folder this is — the whole reason the name stays fixed is that
    // the on-disk layout is meant to be legible, and a grid that
    // stopped naming it would give that away for a prettier row.
    //
    // The step's role rides along as a glyph after the name, where a
    // column of its own used to be. It belongs here: a fetch step and
    // the render step reading it routinely carry the *same* name, so
    // the mark is what tells two adjacent rows apart — and a 64px
    // column holding one icon read as a column about nothing.
    cellRenderer: (p: ICellRendererParams<Row>) => {
      const wrap = document.createElement("span");
      wrap.className = "m2-cell-source";
      const row = p.data;
      const text = document.createElement("span");
      text.textContent = row?.name ?? "";
      wrap.appendChild(text);
      if (row) {
        const glyph = row.kind === "applet" ? STEP_GLYPHS.applet : STEP_GLYPHS[row.phase];
        const mark = document.createElement("span");
        mark.className = "m2-name-step";
        mark.title = row.kindLabel;
        mark.appendChild(glyphSvg(glyph, row.kindLabel, 14));
        wrap.appendChild(mark);
      }
      if (row && row.name !== row.id) {
        const dir = document.createElement("span");
        dir.className = "m2-cell-dir";
        dir.textContent = row.id;
        dir.title = `Id — stored in ${row.id}/ under the data root`;
        wrap.appendChild(dir);
      }
      return wrap;
    },
  },
  {
    // The service, as its own mark. No text: a brand mark is more
    // legible at a glance than its name is, and the name is one hover
    // away — which is the trade the whole row makes.
    headerName: "Type",
    field: "typeLabel",
    width: 70,
    minWidth: 70,
    cellRenderer: (p: ICellRendererParams<Row>) => {
      const row = p.data;
      const wrap = document.createElement("span");
      wrap.className = "m2-cell-type";
      if (!row) return wrap;
      const url = iconUrl(row.icon);
      if (url) {
        const img = document.createElement("img");
        img.src = url;
        img.alt = row.typeLabel;
        img.title = row.typeLabel;
        wrap.appendChild(img);
      } else {
        // No brand mark for this type. The word is what we have, and a
        // blank cell would read as "no type" rather than "no logo".
        const abbr = document.createElement("span");
        abbr.className = "m2-type-word";
        abbr.textContent = row.typeLabel;
        abbr.title = row.typeLabel;
        wrap.appendChild(abbr);
      }
      return wrap;
    },
  },
  {
    headerName: "Status",
    field: "status",
    colId: "status",
    width: 96,
    minWidth: 96,
    // Sort and filter on the word, not on the object — otherwise both
    // operate on "[object Object]" and quietly do nothing useful.
    valueGetter: (p: ValueGetterParams<Row>) => p.data?.status.label ?? "",
    // No `tooltipValueGetter` here: the renderer sets a native `title`,
    // and AG Grid's own tooltip on top of it means two tooltips racing
    // on one cell. One mechanism, carrying the whole sentence.
    cellRenderer: (p: ICellRendererParams<Row>) => {
      const row = p.data;
      const wrap = document.createElement("span");
      if (!row) return wrap;
      const { key, label } = row.status;
      wrap.className = `m2-status m2-status-${key.replace(/[\s_]+/g, "-")}`;
      // The word, and then why it is that word: a failure message, how
      // a run died, or which steps a queued row is behind. The column
      // is icons, so this is the only place either appears.
      //
      // While it is running the step's own words beat everything — more
      // than "Running" ever is.
      const why = row.progress?.msg ?? row.status.detail;
      wrap.title = why ? `${label} — ${why}` : label;

      if (key === "running") {
        // A still frame can't say "still going", so running is the one
        // state drawn rather than glyphed.
        const spin = document.createElement("span");
        spin.className = "m2-spinner";
        spin.setAttribute("role", "img");
        spin.setAttribute("aria-label", label);
        wrap.appendChild(spin);

        const prog = row.progress;
        // A known total gets a bar. An unknown one gets none: a bar at
        // an invented fraction claims more than we know, and the
        // spinner is already the honest signal that something is
        // happening.
        if (prog && prog.total != null && prog.total > 0 && prog.done != null) {
          const frac = Math.max(0, Math.min(1, prog.done / prog.total));
          const bar = document.createElement("span");
          bar.className = "m2-progress";
          const fill = document.createElement("span");
          fill.style.width = `${frac * 100}%`;
          bar.appendChild(fill);
          wrap.appendChild(bar);
        }
        return wrap;
      }

      const glyph = STATUS_GLYPHS[key];
      if (glyph) {
        wrap.appendChild(glyphSvg(glyph, label));
      } else {
        // A status this sheet hasn't met. Say the word rather than
        // drawing nothing — an unknown state is exactly when a reader
        // most needs to know what it was.
        wrap.textContent = label;
      }
      return wrap;
    },
  },
  {
    headerName: "Last synced",
    field: "lastSynced",
    width: 150,
    minWidth: 150,
    // Sort on the instant. The cell paints "5 minutes ago", and AG Grid
    // sorts the row's *value* rather than what a renderer drew — but
    // the value is an ISO string carrying its source's own UTC offset,
    // which does not compare correctly as text either. A row that never
    // ran sorts as "forever ago", so reversing the column reverses all
    // of it and one click groups the never-run rows. See
    // `compareStamps`.
    comparator: compareStamps,
    cellRenderer: (p: ICellRendererParams<Row>) => {
      const iso = p.data?.lastSynced ?? null;
      const span = document.createElement("span");
      span.textContent = formatRelative(iso, Date.now());
      // The exact stamp, for when "7 days ago" isn't the answer you
      // needed. Only when there is one — "—" has nothing to reveal.
      if (iso) span.title = formatStamp(iso);
      else span.className = "m2-none";
      return span;
    },
  },
  {
    headerName: "Bytes on disk",
    field: "bytes",
    width: 140,
    minWidth: 140,
    // The breakdown is what answers "why is this 40 GB?" — attachments
    // routinely dwarf both the entity store and the rendered markdown.
    tooltipValueGetter: (p: { data?: Row }) => {
      const row = p.data;
      if (!row) return undefined;
      if (row.kind === "applet") return "An applet owns no artifacts.";
      const present = row.outputs.filter((o) => o.present);
      if (present.length === 0) return "Nothing on disk yet — this hasn't produced anything.";
      // The total leads, because it is the number the bar stands for
      // and the bar alone can't say it. The per-output breakdown —
      // entities vs attachments, where the backend found a split —
      // follows, and is the answer to "why is this so big" far more
      // often than the total is.
      const detail = present
        .map((o) =>
          o.parts?.length
            ? `${o.path}: ${o.parts.map((x) => `${x.label} ${formatBytes(x.bytes)}`).join(", ")}`
            : `${o.path}: ${formatBytes(o.bytes)}`,
        )
        .join(" · ");
      const total = formatBytes(row.bytes ?? 0);
      return present.length === 1 && !present[0].parts?.length ? total : `${total} — ${detail}`;
    },
    // A bar rather than a number, on a linear scale against the largest
    // row. Linear is the point: the question this column answers is
    // "how much of my disk is this", and on a real root one source
    // routinely dwarfs every other — which a log scale would flatter
    // away. Small rows get a sliver rather than nothing, so "tiny" and
    // "absent" stay distinguishable.
    cellRenderer: (p: ICellRendererParams<Row>) => {
      const row = p.data;
      const wrap = document.createElement("span");
      wrap.className = "m2-bytes";
      if (!row || row.bytes === null) {
        // Null is "nothing on disk yet", which is not a zero-length
        // bar — it's the absence of a bar.
        wrap.textContent = "—";
        wrap.classList.add("m2-none");
        return wrap;
      }
      const max = maxBytes.value;
      const frac = max > 0 ? row.bytes / max : 0;
      const track = document.createElement("span");
      track.className = "m2-bar";
      const fill = document.createElement("span");
      fill.className = "m2-bar-fill";
      fill.style.width = `${Math.max(frac * 100, row.bytes > 0 ? 1.5 : 0)}%`;
      track.appendChild(fill);
      // The size, centred over the bar. The bar answers "how does this
      // compare to the others" at a glance and the number answers "how
      // much is it" exactly; neither replaces the other, and stacking
      // them costs no width in a column that has little to spare.
      const label = document.createElement("span");
      label.className = "m2-bar-label";
      label.textContent = formatBytes(row.bytes);
      track.appendChild(label);
      wrap.appendChild(track);
      return wrap;
    },
  },
  {
    headerName: "Actions",
    colId: "actions",
    sortable: false,
    filter: false,
    flex: 1,
    width: canReveal ? 162 : 132,
    minWidth: canReveal ? 132 : 102,
    resizable: false,
    valueGetter: (p: ValueGetterParams<Row>) => p.data?.id,
    cellRenderer: (p: ICellRendererParams<Row>) => {
      const wrap = document.createElement("span");
      wrap.className = "m2-actions";
      const row = p.data!;
      // One button, two faces. While a job has this row claimed the
      // only useful thing to do with it is call it off — starting a
      // second sync of work already queued is never what was meant.
      if (row.stopJobId) {
        const claim = claimedBy.value.get(row.id);
        wrap.appendChild(
          iconButton(
            "stop",
            claim?.source_name
              ? `Stop the sync of ${claim.source_name}`
              : "Stop the sync in progress",
            null,
            true,
            () => stopSource(row.id),
          ),
        );
      } else {
        wrap.appendChild(
          iconButton("run", "Sync now", row.runBlocked, false, () => runSource(row.id)),
        );
      }
      wrap.appendChild(
        iconButton("edit", "Edit settings", row.editBlocked, false, () => openEdit(row.id)),
      );
      // Only shown where it applies: a fetch step with no render step
      // reading it yet. Absent rather than disabled everywhere else,
      // which would put a dead button on every index and applet row.
      if (row.phase === "fetch") {
        wrap.appendChild(
          iconButton(
            "render",
            "Render to markdown",
            row.renderBlocked,
            false,
            () => openRenderFor(row.id),
          ),
        );
      }
      // Absent rather than disabled in a plain browser — the same
      // "a missing menu item, not a broken one" rule desktop.ts states.
      if (canReveal) {
        wrap.appendChild(
          iconButton("reveal", revealLabel, row.revealBlocked, false, () => reveal(row.id)),
        );
      }
      wrap.appendChild(
        iconButton("trash", "Remove from config", null, true, () => deleteSource(row.id)),
      );
      return wrap;
    },
  },
];

let gridApi: GridApi<Row> | null = null;
function onGridReady(e: GridReadyEvent<Row>) {
  gridApi = e.api;
}

/// Commit only the newest answer, whatever order the answers arrive in.
///
/// Every loader below can be in flight more than once at a time: a 2s
/// progress poll, a 5s full poll, and the explicit calls the enqueue,
/// cancel and SSE handlers make. Nothing orders those, so an older
/// request can answer *after* a newer one — and each of these assigns
/// its ref wholesale, so the older snapshot wins and the screen walks
/// backwards.
///
/// The two timers that used to be the loudest source of overlap are
/// gone (`backend/http/src/watch.rs` replaced them with pushes), and
/// the hazard is not: a `dag_changed` burst, a resync issuing every
/// loader at once, and an enqueue racing the list read all still put
/// several requests in flight at the same time.
///
/// That is not theoretical. A list of jobs fetched before the sync was
/// enqueued, landing after it, drops the job we just created; the step
/// loses its claim, and `stepStatus` falls past the queued branch to
/// "never run" — a row that reads as *never synced* one frame after
/// being queued, and then as succeeded. `manager2-sync`'s monotonicity
/// test catches it as `["queued","never_run","succeeded"]`.
///
/// Sequenced on when each request *started*, and a late answer is
/// dropped rather than applied. An answer still commits if nothing
/// newer has committed yet, so a failed newer request doesn't strand
/// the older one's data.
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
  ///
  /// For when something *other* than a fetch becomes the newest truth —
  /// `adoptJob` writing the row `POST /api/sync/jobs` just returned.
  /// Sequencing the fetches against each other is not enough on its
  /// own: a list read issued before the click still carries a list from
  /// before the job existed, and committing it erases the job, drops
  /// the step's claim, and the row falls back to whatever it said last
  /// run. That is a stale "Succeeded" one frame after a sync was
  /// queued, which is exactly the going-backwards `manager2-sync`
  /// exists to catch.
  run.invalidate = () => {
    committed = issued;
  };
  return run as typeof run & { invalidate: () => void };
}

// ── One step's log ───────────────────────────────────────────────────
//
// A red Status says *that* a step failed and, on hover, the runner's
// one-line reason. The next question is always the same — what was it
// doing — and until now the only answer was the whole job log, every
// step's events interleaved and each `log` event's sentence buried
// inside an escaped `tracing` envelope. Double-clicking the cell
// narrows that to the one step, unwrapped: see `config/stepLog.ts`.
//
// It reads the job logs the worker already writes, so there is no new
// endpoint behind this. The cost is that a run started from a terminal
// has no job row and so no log to find — which the empty state says,
// rather than leaving a blank panel implying the step said nothing.

/// The row whose log is open, or null when the panel is closed.
const logFor = ref<Row | null>(null);
const logLines = ref<StepLogLine[]>([]);
const logBusy = ref(false);
/// Which job's log is on screen. Shown, because "this step's last run"
/// is only meaningful if you can tell *which* run.
const logJob = ref<SyncJob | null>(null);
const logError = ref<string | null>(null);

/// How far back to look for a job whose log mentions this step.
///
/// A step that is skipped as up-to-date still appears in its run's log,
/// so the newest job is very nearly always the answer. The walk exists
/// for the case that isn't: a step added since, or one whose last real
/// work was several syncs ago. Bounded because each miss is a fetch of
/// a whole log file.
const LOG_SEARCH_DEPTH = 8;

async function openStepLog(row: Row) {
  logFor.value = row;
  logLines.value = [];
  logJob.value = null;
  logError.value = null;
  logBusy.value = true;
  try {
    // Newest first. `fetchAllJobs` returns them that way, but sorting
    // here means this doesn't quietly depend on that.
    const recent = [...jobs.value]
      .sort((a, b) => compareStamps(b.created_at, a.created_at))
      .slice(0, LOG_SEARCH_DEPTH);
    for (const job of recent) {
      let text: string;
      try {
        text = await fetchJobLog(job.id);
      } catch {
        // 404 while the worker has claimed the job but not yet opened
        // the file, or a log already cleaned up. Neither is this
        // step's problem; keep looking.
        continue;
      }
      const lines = stepLogLines(text, row.id);
      if (lines.length === 0) continue;
      logLines.value = lines;
      logJob.value = job;
      return;
    }
  } catch (e) {
    logError.value = (e as Error).message;
  } finally {
    logBusy.value = false;
  }
}

/// Double-click on Status opens that row's log. Only that column: the
/// rest of the row has its own meanings for a double-click, and
/// overloading all of them would make the gesture unguessable.
function onCellDoubleClicked(e: { column?: { getColId: () => string }; data?: Row }) {
  if (e.column?.getColId() !== "status" || !e.data) return;
  void openStepLog(e.data);
}

/// Escape closes the log panel, which is what a modal owes its reader.
function onWindowKeydown(e: KeyboardEvent) {
  if (e.key === "Escape" && logFor.value) logFor.value = null;
}

function reparse() {
  try {
    sources.value = listSteps(configText.value);
    parseError.value = null;
  } catch (e) {
    parseError.value = (e as Error).message;
  }
  // The Actions cell is a `cellRenderer`, and AG Grid reuses a cell
  // whose row id is unchanged — so a button's disabled state is baked
  // in at first render and does not follow the row. That matters here:
  // adding a render step must disable "Render to markdown" on the fetch
  // row beside it, and the row id didn't change. Same repaint the qmd
  // columns do in GridCard for the same reason.
  gridApi?.refreshCells({ columns: ["actions"], force: true });
}

async function loadConfig() {
  loadError.value = null;
  try {
    let cfg = await fetchConfig();
    if (!cfg.exists) cfg = await fetchConfigScaffold();
    configPath.value = cfg.path;
    configError.value = cfg.parsed_ok ? null : (cfg.error ?? "The config was rejected.");
    serverSourceCount.value = cfg.source_count;
    configExists.value = cfg.exists;
    // A reload must never overwrite what someone is typing into the
    // Advanced editor. Their text wins until they save or discard.
    if (configDirty.value) return;
    configText.value = cfg.text;
    reparse();
    if (sources.value.length === 0 && cfg.source_count > 0) {
      // The inspector is the only channel when someone hits this in the
      // desktop app and can't copy text out of a banner.
      console.warn(
        "manager2: parsed 0 entries from a config the server reads",
        cfg.source_count,
        "sources from —",
        { path: cfg.path, textLength: cfg.text.length, parsedOk: cfg.parsed_ok },
      );
    }
  } catch (e) {
    loadError.value = (e as Error).message;
  }
}

/// Repaint the columns whose content is a `cellRenderer` over state
/// that lives outside the row's identity.
///
/// AG Grid reuses a cell whose row id hasn't changed, so everything
/// these renderers read — the runner's record, the job queue, the
/// column-wide byte maximum — would otherwise stay frozen at whatever
/// it was when the row first rendered. Same repaint the qmd columns do
/// in GridCard for the same reason.
///
/// `actions` is in the list because the Run/Stop face and every
/// button's disabled state are baked in at render time.
function repaint() {
  gridApi?.refreshCells({
    columns: ["status", "lastSynced", "bytes", "actions"],
    force: true,
  });
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
    if (j) retireBanner(j.id, j.state);
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
const jobActive = computed(() =>
  jobs.value.some((j) => j.state === "pending" || j.state === "running"),
);

/// The runner's per-step record.
///
/// Fetched when the server says it moved (`dag_changed`), not on a
/// timer. A separate channel from the job stream on purpose: that
/// stream only carries runs *this server* started, and the whole point
/// of reading the runner's own file is that a `datalib-dag` run from a
/// terminal shows up here too. The watcher behind `dag_changed`
/// (`backend/http/src/watch.rs`) sees that run's writes exactly as it
/// sees ours.
///
/// Through `freshest` because these refetches still overlap — several
/// land together on a resync, and one can answer after a newer one.
const commitDag = freshest<Awaited<ReturnType<typeof fetchDag>>>((dag) => {
  dagSteps.value = Object.fromEntries(dag.steps.map((st) => [st.id, st]));
  dagRun.value = dag.run;
  repaint();
});

async function loadDag() {
  try {
    await commitDag(() => fetchDag());
  } catch {
    // A missing record reads as "never run", which is what a fresh root
    // looks like anyway.
  }
}

const commitStorage = freshest<OutputStorage[]>((s) => {
  storage.value = s;
});

async function loadStorage() {
  try {
    await commitStorage(() => fetchPipelineStorage());
  } catch {
    // Same: a missing size column beats an error banner over the grid.
  }
}

async function loadAppletHealth() {
  try {
    const view = await fetchFrontend();
    appletErrors.value = view.applet_errors ?? {};
  } catch {
    // Leave the last known state; an applet row without a status beats
    // claiming it failed because one fetch did.
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
    configDirty.value = false;
    reparse();
    banner.value = { ok: true, text: what };
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
  renderFor.value = null;
}

function openAdd() {
  editing.value = null;
  renderFor.value = null;
  wizardKey.value++;
  wizardOpen.value = true;
}

function openEdit(id: string) {
  const step = sources.value.find((s) => s.id === id);
  if (!step?.type) return;
  const entry = catalogFor(step.type);
  if (!entry) return;
  renderFor.value = null;
  editing.value = { step, entry };
  wizardKey.value++;
  wizardOpen.value = true;
}

/// Add the render step that reads an existing fetch step. The row
/// action; the same dialog the chained "also render this?" opens.
function openRenderFor(fetchId: string) {
  const step = sources.value.find((s) => s.id === fetchId);
  if (!step?.type) return;
  const entry = catalogFor(step.type);
  if (!entry) return;
  editing.value = null;
  renderFor.value = { fetchId, fetchName: step.name, entry };
  wizardKey.value++;
  wizardOpen.value = true;
}

async function onWizardSubmit(payload: {
  id: string;
  name: string;
  body: string;
  entry: CatalogEntry;
  inputs: string[];
  offerRenderFor: { fetchId: string; fetchName: string } | null;
  alsoRender: { id: string; body: string } | null;
}) {
  const current = editing.value;
  let next = current
    ? replaceStep(configText.value, current.step, payload.body)
    : appendSource(configText.value, payload.body);

  // The render step written alongside a fetch step, when the wizard's
  // checkbox was ticked. One save, so a failure leaves neither.
  if (payload.alsoRender) next = appendSource(next, payload.alsoRender.body);

  // The fan-ins name their inputs, so a render step added without this
  // renders happily and is never indexed. Idempotent, so re-saving an
  // edit doesn't duplicate the entry.
  for (const id of [payload.id, payload.alsoRender?.id]) {
    if (id && phaseOf(id) === "render") next = wireIntoFanIns(next, id);
  }

  // Banners are for a person, so they say the name; the id is what the
  // config and the disk use.
  const shown = payload.name || payload.id;
  const what = current ? `Saved ${shown}.` : `Added ${shown}.`;
  const ok = await writeConfig(
    next,
    payload.alsoRender ? `${what.slice(0, -1)}, with a step to render it.` : what,
  );
  if (!ok) return;

  // Providers whose render step *does* have options get a second dialog
  // instead of the checkbox. Declining is a real answer — the fetch
  // step stands on its own, and the row action adds one later.
  const offer = payload.offerRenderFor;
  closeWizard();
  if (
    offer &&
    window.confirm(
      `Added ${offer.fetchName}.\n\n` +
        `Also render it to markdown? That is the step that makes it searchable — ` +
        `you can add it later from the row's actions instead.`,
    )
  ) {
    renderFor.value = { ...offer, entry: payload.entry };
    wizardKey.value++;
    wizardOpen.value = true;
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
  const sibling = step.phase === "fetch" ? renderSiblingOf(step.id) : undefined;
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

  // Unwire before removing, for the same reason.
  let next = configText.value;
  for (const d of doomed) {
    if (d.phase === "render") next = unwireFromFanIns(next, d.id);
  }
  await writeConfig(removeSteps(next, doomed), `Removed ${name}.`);
}

async function reveal(id: string) {
  const path = rows.value.find((r) => r.id === id)?.revealPath;
  if (!path) return;
  const ok = await revealInFileManager(path);
  if (!ok) {
    banner.value = { ok: false, text: `Could not open ${path} in the file manager.` };
  }
}

function onConfigEdit() {
  configDirty.value = true;
  clearBanner();
}

async function saveConfigEdits() {
  await writeConfig(configText.value, "Saved the config.");
}

async function discardConfigEdits() {
  configDirty.value = false;
  await loadConfig();
  clearBanner();
}

async function runSource(id: string) {
  const step = sources.value.find((s) => s.id === id);
  // One row, one step, one id — the runner takes it verbatim. There is
  // no longer a pair to choose between.
  const target = id;
  busy.value = true;
  clearBanner();
  try {
    const job = await enqueueJob({ kind: "all", source_name: target });
    adoptJob(job);
    say(true, `Queued a sync for ${step?.name ?? id}.`, job.id);
    // Before returning: the queue is what puts this row and everything
    // downstream of it into "Queued" and flips the button to Stop, and
    // the whole complaint this answers is that pressing play looked
    // like nothing happened.
    await loadJobs();
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  } finally {
    busy.value = false;
  }
}

/// Fold a job we have in hand into the queue we hold.
///
/// `POST /api/sync/jobs` answers with the row it created, which is the
/// authoritative fact that the job exists — more so than the list read
/// that follows it, which is a separate query against a store the write
/// may not be visible in yet. Seeding it here means the row is claimed,
/// and reads as Queued, on the same tick as the click rather than a
/// round trip later.
function adoptJob(job: SyncJob) {
  // Newest truth wins: anything already in flight predates this job.
  commitJobs.invalidate();
  const at = jobs.value.findIndex((j) => j.id === job.id);
  jobs.value =
    at >= 0
      ? [...jobs.value.slice(0, at), job, ...jobs.value.slice(at + 1)]
      : [job, ...jobs.value];
  repaint();
}

/// Sync everything the config declares, in one run.
///
/// `source_name: null` is the runner's own "all sources" — the same job
/// kind a row's Sync enqueues, minus the narrowing. Worth its own
/// button because the per-row one deliberately refuses on anything that
/// isn't a source step: with several sources configured, "bring
/// everything up to date" otherwise meant finding each source row and
/// pressing them one at a time, and getting one run per source instead
/// of one run over the whole graph.
async function runEverything() {
  busy.value = true;
  clearBanner();
  try {
    const job = await enqueueJob({ kind: "all" });
    adoptJob(job);
    say(true, "Queued a sync of everything.", job.id);
    await loadJobs();
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  } finally {
    busy.value = false;
  }
}

/// Call off the job that has this row claimed.
///
/// The unit of cancellation is the job, not the row: one job is one
/// `datalib-dag` process covering a whole subgraph, and there is no way
/// to drop a single step out of a run in flight. The button says which
/// sync it stops for exactly that reason.
async function stopSource(id: string) {
  const job = claimedBy.value.get(id);
  if (!job) return;
  busy.value = true;
  clearBanner();
  try {
    await cancelJob(job.id);
    say(
      true,
      `Stopping the sync of ${job.source_name || "everything"}. Steps in flight ` +
        `checkpoint what they have and exit.`,
      job.id,
    );
    await loadJobs();
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  } finally {
    busy.value = false;
  }
}

/// One pushed job update, applied without a round trip.
///
/// The stream carries everything the grid needs to change state: the
/// job row (which decides Queued and the Run/Stop face) and the task
/// board (which decides Running and the progress message). Both used to
/// be discarded — the handler here simply refetched — so every update
/// cost two HTTP requests and arrived a fetch late. Pressing Sync
/// looked like nothing had happened partly for that reason.
///
/// What still needs a fetch, and why: a step reaching a *terminal*
/// state. The board says "done"; it does not say when, or with what
/// error, and those are the two things the finished row shows. Only the
/// runner's record has them. So terminal transitions ask, and
/// everything else is painted from the push.
///
/// This handler sees only runs *this server* started. A `datalib-dag`
/// run from a terminal is invisible to it, which is what the `root`
/// channel in `onMounted` is for — not a fallback, a second mechanism
/// for a case the first structurally cannot reach.
function onJobEvent(e: JobProgressEvent) {
  mergeJob(e);
  retireBanner(e.id, e.state);
  const tasks: SyncTask[] = e.tasks ?? [];
  const active = e.state === "pending" || e.state === "running";
  liveTasks.value = active ? pushedOverlay(tasks, new Date().toISOString()) : {};
  repaint();
  // A step finishing is exactly when its record moves — and the record
  // is the only place its finish time and error live.
  if (!active || boardWentTerminal(tasks)) {
    void loadDag();
    void loadStorage();
  }
}

/// Fold a pushed job update into the queue we hold, so the Run/Stop
/// face and every Queued row move on the push rather than on the next
/// `GET /api/sync/jobs/all`.
///
/// The event is a subset of `SyncJob` — it carries no timestamps — so
/// an update patches the row we already have and an unseen job is
/// stubbed with the arrival time. That stub matters: `stepStatus`
/// compares a step's last run against the claiming job's `started_at`
/// to decide whether the job has already been past it, and a missing
/// value there reads as "not yet", which is the safe answer.
function mergeJob(e: JobProgressEvent) {
  const now = new Date().toISOString();
  const at = jobs.value.findIndex((j) => j.id === e.id);
  if (at >= 0) {
    const prev = jobs.value[at];
    const next: SyncJob = {
      ...prev,
      state: e.state,
      progress_pct: e.progress_pct,
      progress_msg: e.progress_msg,
      started_at: prev.started_at ?? (e.state === "running" ? now : null),
      finished_at:
        prev.finished_at ??
        (e.state === "done" || e.state === "failed" || e.state === "canceled" ? now : null),
    };
    jobs.value = [...jobs.value.slice(0, at), next, ...jobs.value.slice(at + 1)];
    return;
  }
  jobs.value = [
    {
      id: e.id,
      kind: e.kind,
      source_name: e.source_name,
      state: e.state,
      progress_pct: e.progress_pct,
      progress_msg: e.progress_msg,
      error: null,
      created_at: now,
      started_at: e.state === "running" ? now : null,
      finished_at: null,
    },
    ...jobs.value,
  ];
}

let unsubscribe: (() => void) | null = null;
let relativePoll: ReturnType<typeof setInterval> | null = null;

/// The Last synced column reads "5 minutes ago", which goes stale on
/// its own: nothing about the page has changed a second later, but the
/// cell is now wrong. Every other repaint here is triggered by data
/// moving, so this is the one clock the column needs.
///
/// It ticks every second but repaints only when at least one row would
/// actually read differently — on a table whose newest row is hours
/// old that is a handful of short string builds per second and no DOM
/// work at all, and in the seconds after a sync it is the per-second
/// update that makes "2 seconds ago" mean it.
let lastRelativePaint = "";
function tickRelative() {
  const now = Date.now();
  const next = rows.value.map((r) => formatRelative(r.lastSynced, now)).join("\u0000");
  if (next === lastRelativePaint) return;
  lastRelativePaint = next;
  gridApi?.refreshCells({ columns: ["lastSynced"], force: true });
}

/// Everything this table shows, refetched together.
///
/// Together is the point, and it is why this is one function rather
/// than five calls at five cadences. The rows are derived from the
/// config; the Status and Last synced columns come from the runner's
/// record. Fetch the first without the second and a row that has run
/// paints as "Never run" — it exists because the config declares it,
/// and nothing has yet said what it did. That intermediate state is
/// real enough that `manager2-sync.spec.ts` had to reload the page to
/// avoid observing it (see `writeConfig` there).
async function reloadAll() {
  await Promise.all([
    loadConfig(),
    loadJobs(),
    loadDag(),
    loadStorage(),
    loadAppletHealth(),
  ]);
}

onMounted(async () => {
  await reloadAll();
  window.addEventListener("keydown", onWindowKeydown);

  // Two push channels, and the split matters.
  //
  // `job` carries syncs *this server* started — it is what makes the
  // Run/Stop face and the Queued rows move on the click rather than on
  // the next fetch.
  //
  // `root` carries what the job stream structurally cannot: a
  // `datalib-dag` run started from a terminal, which has no job row to
  // report on, and an agent (or a hand) editing config.toml. Those two
  // cases are the entire reason this view used to poll — five endpoints
  // every 5 s forever, plus two more every 2 s during a run. On an idle
  // tab that was 60 requests a minute, each `/api/dag` among them
  // reloading the config, rebuilding the graph and taking the runner's
  // lock, to answer "nothing has changed" every time.
  unsubscribe = subscribeLive({
    job: onJobEvent,
    root: (e) => {
      if (e.kind === "dag_changed") {
        // The record moved: a step finished, or a run wrote progress.
        void loadDag();
        void loadStorage();
      } else if (e.kind === "config_changed") {
        // Config and record together, for the "Never run" reason above.
        void reloadAll();
      }
    },
    resync: () => void reloadAll(),
  });

  // The one timer left, and the only one that was ever right: "5
  // minutes ago" goes stale with nothing happening, so it needs a
  // clock rather than an event. See `tickRelative` — it repaints only
  // when a row would actually read differently.
  relativePoll = setInterval(tickRelative, 1000);
});

onUnmounted(() => {
  window.removeEventListener("keydown", onWindowKeydown);
  unsubscribe?.();
  unsubscribe = null;
  if (relativePoll) clearInterval(relativePoll);
  gridApi = null;
});
</script>

<template>
  <section class="m2">
    <header class="m2-head">
      <div>
        <h2>Pipeline</h2>
        <p class="m2-path">
          <code>{{ configPath }}</code>
        </p>
      </div>
      <div class="m2-head-actions">
        <button
          class="m2-btn m2-runall"
          :disabled="busy || !!parseError || !!configError || jobActive || rows.length === 0"
          :title="
            jobActive
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
          + Add Data Source
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
        It parses as TOML, so the table below still reflects it — but nothing will sync, and
        applets won’t start, until this is fixed. Open <b>Advanced</b> below to edit it.
      </span>
      <button class="m2-btn" @click="configOpen = true">Show the config</button>
    </div>
    <p v-if="banner" class="m2-msg" :class="banner.ok ? 'good' : 'bad'">{{ banner.text }}</p>

    <div class="m2-grid">
      <AgGridVue
        class="m2-ag"
        :theme="gridTheme"
        :columnDefs="columnDefs"
        :rowData="rows"
        :getRowId="(p: { data: Row }) => p.data.id"
        :tooltipShowDelay="200"
        @grid-ready="onGridReady"
        @cell-double-clicked="onCellDoubleClicked"
      />
    </div>

    <div class="m2-foot">
    <div v-if="emptyDiagnosis && !parseError" class="m2-msg bad m2-invalid">
      <b>This table is empty, and it shouldn’t be.</b>
      <span>{{ emptyDiagnosis }}</span>
      <button class="m2-btn" @click="configOpen = true">Show the config</button>
    </div>
    <p v-else-if="rows.length === 0 && !parseError" class="m2-empty">
      Nothing configured yet. <b>Add Data Source</b> walks you through one.
    </p>

    <p class="m2-note">
      Every row is something <code>config.toml</code> declares: your <b>sources</b>, the shared
      index <b>steps</b> that make them searchable, and the <b>applets</b> the app spawns to serve
      them. Actions that don’t apply to a kind are disabled and say why. Account and
      document-count columns aren’t here yet — each needs a backend endpoint the design calls for.
      <b>Type</b> and <b>Status</b> are icons, and the mark after a name says what that step
      does — hover any of them for the word. <b>Double-click a Status</b> to read just that
      step's log from the run it last took part in.
      “Bytes on disk” is a directory walk over each row’s declared outputs, drawn against the
      largest row — hover for the size and the breakdown. “Last synced” and “Status” are per
      step, read from the runner’s own record — so a sync you start from a terminal shows up
      here too. A run whose record never closed and whose lock nobody holds reads as
      <b>interrupted</b>: it was killed, not lost. A step a queued sync will reach reads as
      <b>queued</b>, and its Sync button becomes a Stop — one job is one runner process over a
      whole subgraph, so stopping is per sync, not per row.
    </p>

    <details class="m2-advanced" :open="configOpen" @toggle="configOpen = ($event.target as HTMLDetailsElement).open">
      <summary>Advanced — edit <code>config.toml</code> directly</summary>
      <p class="m2-advanced-note">
        The file is the source of truth; everything above is a view of it. This is where to go
        for anything the forms don’t model — a source type with no wizard yet, or a knob like
        <code>common.download_params</code> that would make a row’s Edit button refuse.
      </p>
      <textarea
        v-model="configText"
        class="m2-editor"
        spellcheck="false"
        @input="onConfigEdit"
      />
      <div class="m2-advanced-actions">
        <button class="m2-btn" :disabled="!configDirty || busy" @click="saveConfigEdits">
          Save
        </button>
        <button class="m2-btn muted" :disabled="!configDirty || busy" @click="discardConfigEdits">
          Discard changes
        </button>
        <span v-if="configDirty" class="m2-advanced-dirty">
          Unsaved — the grid above still shows the last saved version.
        </span>
      </div>
    </details>
    </div>

    <div v-if="logFor" class="m2-logs-backdrop" @click.self="logFor = null">
      <div class="m2-logs" role="dialog" aria-modal="true" aria-label="Step log">
        <header class="m2-logs-head">
          <div>
            <h3>{{ logFor.name }}</h3>
            <p>
              <code>{{ logFor.id }}</code>
              <span v-if="logJob">
                · from the sync of
                <b>{{ logJob.source_name || "everything" }}</b>
                <span :title="formatStamp(logJob.created_at)">
                  {{ formatRelative(logJob.created_at, Date.now()) }}</span>
              </span>
            </p>
          </div>
          <button class="m2-btn" @click="logFor = null">Close</button>
        </header>

        <p v-if="logBusy" class="m2-logs-note">Reading the job logs…</p>
        <p v-else-if="logError" class="m2-logs-note bad">{{ logError }}</p>
        <p v-else-if="logLines.length === 0" class="m2-logs-note">
          Nothing found for this step in the last {{ LOG_SEARCH_DEPTH }} syncs this app ran.
          A run started from a terminal writes no job log here, and a step that has never
          run has nothing to say yet.
        </p>
        <ol v-else class="m2-logs-body">
          <li v-for="(l, i) in logLines" :key="i" :class="`m2-log-${l.level}`">
            <span v-if="l.ts" class="m2-log-ts" :title="formatStamp(l.ts)">{{
              l.ts.slice(11, 23)
            }}</span>
            <span class="m2-log-text">{{ l.text }}</span>
          </li>
        </ol>
      </div>
    </div>

    <SourceWizard
      v-if="wizardOpen"
      :key="wizardKey"
      :taken-ids="takenIds"
      :render-for="renderFor"
      :editing="editing"
      @close="closeWizard"
      @submit="onWizardSubmit"
    />
  </section>
</template>

<style scoped>
/* The shell is a viewport-height flex column, so this view can claim
   the leftover and bound itself — which is what lets the grid scroll on
   its own instead of growing the page. `min-height: 0` is the part that
   makes a flex child actually shrink rather than overflow. */
.m2 {
  flex: 1 1 0;
  min-height: 0;
  display: flex;
  flex-direction: column;
  padding: 16px 20px 20px;
  box-sizing: border-box;
}
.m2-head { display: flex; align-items: flex-start; gap: 16px; flex: 0 0 auto; }
/* The two header actions travel together, pinned right. */
.m2-head-actions { margin-left: auto; display: flex; align-items: center; gap: 10px; }
/* Sized to sit level with "Add Data Source", but outlined rather than
   filled: running what is already configured is the routine act, adding
   a source the deliberate one, and only one of them should read as the
   primary thing to do on this screen. */
.m2-runall {
  padding: 8px 14px;
  font-size: inherit;
  font-weight: 600;
  white-space: nowrap;
}
.m2-head h2 { margin: 0 0 4px; font-size: 19px; }
.m2-path { margin: 0; color: var(--datalib-muted); font-size: 12px; }
.m2-add {
  padding: 8px 14px;
  border: 1px solid var(--datalib-accent);
  border-radius: 5px;
  background: var(--datalib-accent);
  color: #fff;
  font: inherit;
  font-weight: 600;
  cursor: pointer;
}
.m2-add:disabled { opacity: 0.5; cursor: not-allowed; }

.m2-msg { margin: 12px 0 0; font-size: 13px; }
.m2-msg.bad { color: var(--datalib-log-error); }
.m2-msg.good { color: var(--datalib-muted); }
.m2-invalid {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 6px;
  margin-top: 12px;
  padding: 10px 12px;
  border: 1px solid var(--datalib-log-error);
  border-radius: 5px;
  max-width: 90ch;
}
.m2-invalid > span { color: var(--datalib-fg); }
.m2-invalid-why { color: var(--datalib-muted) !important; font-size: 12.5px; line-height: 1.55; }

.m2-grid {
  margin-top: 16px;
  /* A definite height is what makes AG Grid scroll internally — both
     ways. `min-height` keeps it usable when the Advanced editor is
     open and competing for the same space. */
  flex: 1 1 auto;
  min-height: 180px;
  position: relative;
}
/* Without `domLayout: autoHeight` the grid sizes to its container, so
   its own element has to fill the box we just gave it — otherwise it
   collapses to nothing and renders neither headers nor rows.
   Fill the positioned .m2-grid absolutely rather than with
   `height: 100%`. WebKit (Safari + the Tauri WKWebView the desktop app
   runs in) resolves a percentage height against a flex-sized parent
   that has no explicit `height` as `auto`, which collapsed this grid to
   its 2px of border — headers and rows present in the DOM, nothing
   painted — while Chromium gives it the full flexed height. Same bug,
   same fix as `.grid` in cards/GridCard.ce.vue. Pinned by
   tests/e2e/manager2-grid.spec.ts under the suite's `webkit` project. */
.m2-ag {
  position: absolute;
  inset: 0;
}
/* Everything under the grid scrolls as one block, so opening the
   Advanced editor never pushes the table off screen. */
.m2-foot {
  flex: 0 0 auto;
  max-height: 52vh;
  overflow-y: auto;
}
.m2-empty { color: var(--datalib-muted); font-size: 14px; margin-top: 16px; }
.m2-advanced {
  margin-top: 24px;
  border-top: 1px solid var(--datalib-border);
  padding-top: 14px;
}
.m2-advanced > summary {
  cursor: pointer;
  font-size: 13px;
  color: var(--datalib-muted);
  user-select: none;
}
.m2-advanced > summary:hover { color: var(--datalib-fg); }
.m2-advanced-note {
  margin: 12px 0 8px;
  font-size: 12.5px;
  color: var(--datalib-muted);
  line-height: 1.6;
  max-width: 76ch;
}
.m2-editor {
  width: 100%;
  min-height: 340px;
  padding: 10px 12px;
  border: 1px solid var(--datalib-border);
  border-radius: 5px;
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12.5px;
  line-height: 1.55;
  resize: vertical;
}
.m2-advanced-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 8px;
}
.m2-advanced-dirty { font-size: 12px; color: var(--datalib-muted); }

.m2-note {
  margin-top: 20px;
  color: var(--datalib-muted);
  font-size: 12px;
  line-height: 1.6;
  max-width: 76ch;
}

/* The per-step log panel. Modal, because it is a full answer to a
   question the grid can only gesture at, and because the grid behind it
   repaints whenever a step moves — a panel docked inside it would be
   fighting that. */
.m2-logs-backdrop {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.45);
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 24px;
  z-index: 50;
}
.m2-logs {
  background: var(--datalib-bg);
  border: 1px solid var(--datalib-border);
  border-radius: 8px;
  width: min(920px, 100%);
  max-height: 100%;
  display: flex;
  flex-direction: column;
  box-shadow: 0 10px 40px rgba(0, 0, 0, 0.35);
}
.m2-logs-head {
  display: flex;
  align-items: flex-start;
  gap: 16px;
  padding: 14px 16px;
  border-bottom: 1px solid var(--datalib-border);
  flex: 0 0 auto;
}
.m2-logs-head h3 { margin: 0 0 3px; font-size: 15px; }
.m2-logs-head p { margin: 0; font-size: 12px; color: var(--datalib-muted); }
.m2-logs-head button { margin-left: auto; }
.m2-logs-note { margin: 0; padding: 16px; font-size: 13px; color: var(--datalib-muted); max-width: 70ch; }
.m2-logs-note.bad { color: var(--datalib-log-error); }
/* Monospace and scrolling on its own: a log line is not prose, and the
   long ones (a path, a serialized error) must not reflow the panel. */
.m2-logs-body {
  margin: 0;
  padding: 10px 16px 16px;
  list-style: none;
  overflow: auto;
  flex: 1 1 auto;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12px;
  line-height: 1.6;
}
.m2-logs-body li {
  display: flex;
  gap: 10px;
  align-items: baseline;
  white-space: pre-wrap;
  word-break: break-word;
}
.m2-log-ts { color: var(--datalib-muted); flex: 0 0 auto; }
.m2-log-text { flex: 1 1 auto; }
.m2-log-warn .m2-log-text { color: var(--datalib-log-warn); }
.m2-log-error .m2-log-text { color: var(--datalib-log-error); }
</style>

<style>
/* Cell renderers build plain DOM, so their classes can't be scoped. */
.m2-cell-source { display: inline-flex; align-items: center; gap: 8px; }
.m2-cell-dir { color: var(--datalib-muted); font-size: 12px; }
/* The step-role mark, riding after the name. Muted and a size down
   from the Type mark beside it: the name is what the eye should land
   on, and this answers the follow-up question rather than competing
   with it. `gap` on the parent already spaces it. */
.m2-name-step {
  display: inline-flex;
  align-items: center;
  color: var(--datalib-muted);
  flex: 0 0 auto;
}

/* Type: one mark, centred in a narrow column, with the word on
   `title`. The cell fills the row height so the glyph sits on the text
   baseline's optical centre rather than at the top. */
.m2-cell-type {
  display: inline-flex;
  align-items: center;
  height: 100%;
}
.m2-cell-type img { width: 18px; height: 18px; object-fit: contain; }
/* The fallback when a type has no brand mark. Clipped rather than
   wrapped: the column is sized for an icon, and the full name is on
   `title` like every other cell here. */
.m2-type-word {
  font-size: 11px;
  color: var(--datalib-muted);
  max-width: 100%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
/* The status vocabulary. Anything unstyled falls through to the default
   colour, which is the right outcome for a status this sheet hasn't
   met — and that case renders the word rather than a glyph. */
.m2-status {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 100%;
  color: var(--datalib-muted);
}
.m2-status-running { color: var(--datalib-accent); }
.m2-status-queued { color: var(--datalib-muted); }
.m2-status-failed { color: var(--datalib-log-error); }
/* A run that died mid-step: not a failure anyone reported, but not a
   success either, so it reads as a warning rather than an error. */
.m2-status-interrupted { color: var(--datalib-log-warn); }
/* Both tick glyphs are green, and for the same reason the failure "!"
   is red: the column is icons now, so colour is doing the work the
   words used to. A grey tick beside a red exclamation reads as "no
   answer yet" rather than "fine".
   `succeeded` is one tick, `skipped_up_to_date` ("Up to date") is two —
   different facts, both good news, so they share the colour and are
   told apart by the glyph. */
.m2-status-succeeded,
.m2-status-skipped-up-to-date { color: var(--datalib-log-ok); }
/* Never run is the emptiest state in the column, and reads as such. */
.m2-status-never-run { opacity: 0.55; }

/* Running is drawn, not glyphed — a still frame can't say "still
   going". A ring with one lit quarter, turning once a second. */
.m2-spinner {
  width: 14px;
  height: 14px;
  border-radius: 50%;
  border: 2px solid color-mix(in srgb, currentColor 25%, transparent);
  border-top-color: currentColor;
  animation: m2-spin 1s linear infinite;
}
@keyframes m2-spin {
  to { transform: rotate(360deg); }
}
/* Motion is the signal here, not decoration — but a static ring still
   reads as "running" beside a column of finished ticks, so honour the
   preference rather than exempting ourselves from it. */
@media (prefers-reduced-motion: reduce) {
  .m2-spinner { animation: none; }
}
/* Shown only when the step reported a total to be a fraction of. */
.m2-progress {
  flex: 1 1 auto;
  min-width: 20px;
  height: 4px;
  border-radius: 2px;
  background: color-mix(in srgb, currentColor 18%, transparent);
  overflow: hidden;
}
.m2-progress > span {
  display: block;
  height: 100%;
  background: currentColor;
}

/* Bytes on disk: a bar against the largest row, with the size centred
   over it and the per-output breakdown on `title`. */
.m2-bytes {
  display: flex;
  align-items: center;
  height: 100%;
  width: 100%;
}
/* "Nothing to show here" — an em dash, in both the Bytes column
   (no artifacts on disk) and Last synced (never run). Shared,
   because it is one meaning. */
.m2-none { color: var(--datalib-muted); opacity: 0.55; }
.m2-bar {
  position: relative;
  flex: 1 1 auto;
  /* Tall enough to hold the label: the bar is the label's background
     now, not a rule beside it. */
  height: 18px;
  border-radius: 3px;
  background: color-mix(in srgb, var(--datalib-fg) 10%, transparent);
  overflow: hidden;
}
/* Absolute so the label can sit over it rather than after it. */
.m2-bar-fill {
  position: absolute;
  inset: 0 auto 0 0;
  border-radius: 3px;
  background: color-mix(in srgb, var(--datalib-accent) 55%, transparent);
}
/* The number reads across both halves of the bar — over the fill on the
   left and the empty track on the right — so it can't take its contrast
   from either. `--datalib-fg` against a fill kept at 55% opacity holds
   up in both themes; the shadow is what keeps the glyph edges legible
   where the two meet. */
.m2-bar-label {
  position: relative;
  display: block;
  text-align: center;
  line-height: 18px;
  font-size: 11px;
  font-variant-numeric: tabular-nums;
  color: var(--datalib-fg);
  text-shadow: 0 0 3px var(--datalib-bg);
}

.m2-actions {
  display: inline-flex;
  gap: 2px;
  align-items: center;
  height: 100%;
}
.m2-icon-btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 26px;
  height: 24px;
  padding: 0;
  border: 1px solid transparent;
  border-radius: 4px;
  background: none;
  color: var(--datalib-muted);
  cursor: pointer;
}
.m2-icon-btn:hover:not(:disabled) {
  background: var(--datalib-hover);
  border-color: var(--datalib-border);
  color: var(--datalib-fg);
}
.m2-icon-btn:disabled { opacity: 0.35; cursor: not-allowed; }
.m2-icon-btn.danger:hover:not(:disabled) {
  color: var(--datalib-log-error);
  border-color: var(--datalib-log-error);
}
.m2-btn {
  padding: 2px 9px;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
  background: var(--datalib-card-bg);
  color: inherit;
  font: inherit;
  font-size: 12px;
  cursor: pointer;
}
.m2-btn:hover:not(:disabled) { background: var(--datalib-hover); }
.m2-btn:disabled { opacity: 0.45; cursor: not-allowed; }
.m2-btn.danger:hover:not(:disabled) { border-color: var(--datalib-log-error); color: var(--datalib-log-error); }
.m2-btn.muted { color: var(--datalib-muted); }
.m2-btn.muted:hover:not(:disabled) { color: var(--datalib-fg); }
</style>
