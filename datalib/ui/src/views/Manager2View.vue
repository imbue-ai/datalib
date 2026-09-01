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
// writes that record whoever spawned it. The SSE stream only carries
// runs this server started, which is why this polls as well.
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
  fetchPipelineStorage,
  fetchFrontend,
  enqueueJob,
  openJobStream,
  type DagRun,
  type DagStep,
  type DagStepProgress,
  type SyncJob,
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
  lastStatus: string;
  /// Why the status is what it is — a failure message, or how a run
  /// came to be interrupted. Null when the status speaks for itself.
  statusDetail: string | null;
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

/// What the Kind column says. A step is labelled by its phase rather
/// than the word "step", because that is the distinction a reader
/// actually wants: which of these brings data in, which turns it into
/// markdown, which is shared index plumbing.
const PHASE_LABEL: Record<StepPhase, string> = {
  fetch: "Fetch",
  render: "Render",
  index: "Index",
  other: "Step",
};

/// What a step is doing right now, or did last — read off the runner's
/// record rather than inferred from the job queue.
///
/// The queue records whole *runs*, so a run naming several steps could
/// only ever attribute one timestamp and one status to all of them;
/// that is what the old `~` marker was apologizing for. The runner
/// knows per step, and now says so.
///
/// A run whose record has no `finished_at` and whose lock nobody holds
/// is a run that died. Its steps say `interrupted` rather than spinning
/// forever.
function stepStatus(id: string): { label: string; at: string | null; detail?: string } {
  const step = dagSteps.value[id];
  const run = dagRun.value;
  const live = run?.live ?? false;

  const current = step?.current_state;
  if (current && run && !run.finished_at) {
    if (live) return { label: current, at: step?.last_run?.started_at ?? run.started_at };
    return {
      label: "interrupted",
      at: step?.last_run?.started_at ?? run.started_at,
      detail: `Started ${run.started_at} and never finished — no runner holds this root now, so it was killed or crashed.`,
    };
  }

  const last = step?.last_run;
  if (!last) return { label: "never run", at: null };
  return {
    label: last.status || "running",
    at: last.finished_at ?? last.started_at,
    detail: last.error ?? undefined,
  };
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

    // Run: the DAG schedules steps, so a source and a plain step can
    // both be targeted; an applet is spawned by the gateway on demand
    // and has nothing to enqueue.
    const runBlocked =
      s.kind === "applet"
        ? "Applets aren't scheduled — the server starts one when something asks for it."
        : null;

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
    const lastStatus =
      s.kind === "applet"
        ? appletErrors.value[s.id]
          ? "failed"
          : "running"
        : run!.label;

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
      lastSynced: run?.at ?? null,
      lastStatus,
      statusDetail: run?.detail ?? null,
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

/// 24×24 Material-ish glyphs, drawn in `currentColor` so they follow the
/// button's own colour through hover, disabled and the dark theme.
const ICON_PATHS: Record<string, string> = {
  run: "M8 5v14l11-7z",
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
    cellRenderer: (p: ICellRendererParams<Row>) => {
      const url = iconUrl(p.data?.icon);
      const wrap = document.createElement("span");
      wrap.className = "m2-cell-source";
      if (url) {
        const img = document.createElement("img");
        img.src = url;
        img.alt = "";
        wrap.appendChild(img);
      }
      const text = document.createElement("span");
      text.textContent = p.data?.name ?? "";
      wrap.appendChild(text);
      if (p.data && p.data.name !== p.data.id) {
        const dir = document.createElement("span");
        dir.className = "m2-cell-dir";
        dir.textContent = p.data.id;
        dir.title = `Id — stored in ${p.data.id}/ under the data root`;
        wrap.appendChild(dir);
      }
      return wrap;
    },
  },
  {
    headerName: "Kind",
    field: "kindLabel",
    width: 90,
    minWidth: 90,
    cellRenderer: (p: ICellRendererParams<Row>) => {
      const span = document.createElement("span");
      span.className = `m2-kind m2-kind-${p.data?.kind}`;
      span.textContent = p.data?.kindLabel ?? "";
      return span;
    },
  },
  { headerName: "Type", field: "typeLabel", width: 130, minWidth: 130 },
  {
    headerName: "Last synced",
    field: "lastSynced",
    width: 175,
    minWidth: 175,
    valueFormatter: (p) => (p.value ? new Date(p.value as string).toLocaleString() : "—"),
  },
  {
    headerName: "Last status",
    field: "lastStatus",
    width: 140,
    minWidth: 140,
    tooltipValueGetter: (p: { data?: Row }) => {
      const row = p.data;
      if (!row) return undefined;
      if (row.kind === "applet") {
        return appletErrors.value[row.id] ?? "The gateway has this applet up.";
      }
      // While it is running, the step's own words are the most useful
      // thing we have — more than "running" ever is.
      return row.progress?.msg ?? row.statusDetail ?? undefined;
    },
    cellRenderer: (p: ICellRendererParams<Row>) => {
      const span = document.createElement("span");
      const label = p.data?.lastStatus ?? "";
      // `skipped_up_to_date` is the runner's word; "up to date" is what
      // it means to someone looking at a table.
      span.className = `m2-status m2-status-${label.replace(/[\s_]+/g, "-")}`;
      let text = label.replace(/_/g, " ").replace("skipped up to date", "up to date");

      const prog = p.data?.progress;
      if (label === "running" && prog) {
        // A known total gets a count and a bar drawn as the pill's own
        // background. An unknown one gets neither: "6" with no
        // denominator, or a bar at an invented fraction, both claim more
        // than we know. The pill keeps pulsing, which is the honest
        // signal that something is happening.
        if (prog.total != null && prog.total > 0 && prog.done != null) {
          const frac = Math.max(0, Math.min(1, prog.done / prog.total));
          text += ` ${prog.done}/${prog.total}`;
          span.style.background = `linear-gradient(to right, var(--m2-progress-fill) ${
            frac * 100
          }%, transparent ${frac * 100}%)`;
        }
      }
      span.textContent = text;
      return span;
    },
  },
  {
    headerName: "Bytes on disk",
    field: "bytes",
    type: "numericColumn",
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
      // Per declared output, with the entities/attachments split where
      // the backend found one. That split is the answer to "why is this
      // so big" far more often than the total is.
      return present
        .map((o) =>
          o.parts?.length
            ? `${o.path}: ${o.parts.map((x) => `${x.label} ${formatBytes(x.bytes)}`).join(", ")}`
            : `${o.path}: ${formatBytes(o.bytes)}`,
        )
        .join(" · ");
    },
    valueFormatter: (p) => (p.value === null ? "—" : formatBytes(p.value as number)),
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
      wrap.appendChild(
        iconButton("run", "Sync now", row.runBlocked, false, () => runSource(row.id)),
      );
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
    // The poll must never overwrite what someone is typing into the
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

async function loadJobs() {
  try {
    jobs.value = await fetchAllJobs(100);
  } catch {
    // The grid is still useful without status; leave the columns empty.
  }
}

/// The runner's per-step record. Polled rather than pushed: the SSE
/// stream only carries runs *this server* started, and the whole point
/// of reading the runner's own file is that a terminal `datalib-dag`
/// shows up here too.
async function loadDag() {
  try {
    const dag = await fetchDag();
    dagSteps.value = Object.fromEntries(dag.steps.map((st) => [st.id, st]));
    dagRun.value = dag.run;
    // Statuses live in a `cellRenderer`, and AG Grid reuses a cell whose
    // row id hasn't changed — so the painted status would otherwise
    // stay at whatever it was when the row first rendered.
    gridApi?.refreshCells({ columns: ["lastStatus", "lastSynced"], force: true });
  } catch {
    // A missing record reads as "never run", which is what a fresh root
    // looks like anyway.
  }
}

async function loadStorage() {
  try {
    storage.value = await fetchPipelineStorage();
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
  banner.value = null;
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
  wizardOpen.value = true;
}

function openEdit(id: string) {
  const step = sources.value.find((s) => s.id === id);
  if (!step?.type) return;
  const entry = catalogFor(step.type);
  if (!entry) return;
  renderFor.value = null;
  editing.value = { step, entry };
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
  banner.value = null;
}

async function saveConfigEdits() {
  await writeConfig(configText.value, "Saved the config.");
}

async function discardConfigEdits() {
  configDirty.value = false;
  await loadConfig();
  banner.value = null;
}

async function runSource(id: string) {
  const step = sources.value.find((s) => s.id === id);
  // One row, one step, one id — the runner takes it verbatim. There is
  // no longer a pair to choose between.
  const target = id;
  busy.value = true;
  banner.value = null;
  try {
    await enqueueJob({ kind: "all", source_name: target });
    banner.value = { ok: true, text: `Queued a sync for ${step?.name ?? id}.` };
    await loadJobs();
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  } finally {
    busy.value = false;
  }
}

let stream: EventSource | null = null;
let poll: ReturnType<typeof setInterval> | null = null;
let progressPoll: ReturnType<typeof setInterval> | null = null;

onMounted(async () => {
  await Promise.all([loadConfig(), loadJobs(), loadDag(), loadStorage(), loadAppletHealth()]);
  stream = openJobStream(() => {
    void loadJobs();
    // A step finishing is exactly when its record moves.
    void loadDag();
  });
  // Progress moves much faster than anything else here, and only while
  // a run is live — so poll the DAG on its own quick cadence then, and
  // leave the rest on the slow one. Off entirely when nothing is
  // running, so an idle tab is not asking twice a second forever.
  progressPoll = setInterval(() => {
    if (dagRun.value?.live) void loadDag();
  }, 1000);
  // The config can change under us — an agent PUTs it, or the Manage
  // tab saves. Same cadence the Manage tab polls at.
  poll = setInterval(() => {
    void loadConfig();
    void loadJobs();
    void loadDag();
    void loadStorage();
    void loadAppletHealth();
  }, 5000);
});

onUnmounted(() => {
  if (stream) stream.close();
  if (poll) clearInterval(poll);
  if (progressPoll) clearInterval(progressPoll);
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
      <button class="m2-add" :disabled="busy || !!parseError || !!configError" @click="openAdd">
        + Add Data Source
      </button>
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
      “Bytes on disk” is a directory walk over each row’s declared outputs; hover a value for the
      breakdown. “Last synced” and “Last status” are per step, read from the runner’s own
      record — so a sync you start from a terminal shows up here too. A run whose record never
      closed and whose lock nobody holds reads as <b>interrupted</b>: it was killed, not lost.
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

    <SourceWizard
      v-if="wizardOpen"
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
.m2-head h2 { margin: 0 0 4px; font-size: 19px; }
.m2-path { margin: 0; color: var(--datalib-muted); font-size: 12px; }
.m2-add {
  margin-left: auto;
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
</style>

<style>
/* Cell renderers build plain DOM, so their classes can't be scoped. */
.m2-cell-source { display: inline-flex; align-items: center; gap: 8px; }
.m2-cell-source img { width: 16px; height: 16px; }
.m2-cell-dir { color: var(--datalib-muted); font-size: 12px; }

.m2-kind {
  font-size: 11px;
  letter-spacing: 0.03em;
  padding: 1px 7px;
  border-radius: 10px;
  border: 1px solid var(--datalib-border);
  color: var(--datalib-muted);
}
.m2-kind-source { color: var(--datalib-fg); border-color: var(--datalib-fg); }

/* The runner's status vocabulary. Anything unstyled falls through to
   the default colour, which is the right outcome for a status this
   sheet hasn't met. */
.m2-status { text-transform: capitalize; }
.m2-status-running { color: var(--datalib-accent); font-weight: 600; }
/* The progress bar is the running pill's own background, so a step with
   a known total fills left-to-right in place. Kept faint: it has to sit
   under the label without fighting it for legibility. */
.m2-status-running {
  --m2-progress-fill: color-mix(in srgb, var(--datalib-accent) 22%, transparent);
  border-radius: 3px;
  padding: 1px 4px;
  margin: -1px -4px;
}
.m2-status-failed { color: var(--datalib-log-error); font-weight: 600; }
/* A run that died mid-step: not a failure anyone reported, but not a
   success either, so it reads as a warning rather than an error. */
.m2-status-interrupted { color: var(--datalib-log-warn); }
.m2-status-blocked { color: var(--datalib-muted); }
.m2-status-succeeded { color: var(--datalib-muted); }
.m2-status-up-to-date { color: var(--datalib-muted); }
.m2-status-not-selected { color: var(--datalib-muted); }
.m2-status-never-run { color: var(--datalib-muted); }
.m2-status-done { color: var(--datalib-muted); }

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
