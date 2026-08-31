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
// "Last synced" / "Last status" are best-effort. There is no per-source
// run record yet — `sync_jobs` is per *run*, and a run routinely spans
// several sources via a comma-joined `source_name` — so a multi-source
// job attributes its outcome to every source it named. The design's
// `step_runs` table is what makes these honest; until then the column
// says so on hover.
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
  fetchPipelineStorage,
  fetchFrontend,
  enqueueJob,
  openJobStream,
  type SyncJob,
  type OutputStorage,
} from "@/api";
import {
  listConfiguredSources,
  type EntryKind,
  appendSource,
  removeSource,
  replaceSource,
  unwireFromFanIns,
  wireIntoFanIns,
  paramsAreRepresentable,
  emptyTableDiagnosis,
  type ConfiguredSource,
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
/// applet id → why it failed to start, from `GET /api/frontend`. An
/// applet that won't come up is otherwise only visible as a 502 from
/// whatever tab needed it.
const appletErrors = ref<Record<string, string>>({});
const sources = ref<ConfiguredSource[]>([]);

// The Advanced disclosure. Closed on load: the point of this tab is
// that a text editor is not the first thing you meet.
const configOpen = ref(false);
const configDirty = ref(false);

// Resolved once — the desktop bridge either exists for this window or
// it doesn't, and the label depends only on the platform.
const canReveal = isDesktopApp();
const revealLabel = revealActionLabel();

const wizardOpen = ref(false);
const editing = ref<{ source: ConfiguredSource; entry: CatalogEntry } | null>(null);

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

const takenIds = computed(
  () => new Set(sources.value.filter((s) => s.kind === "source").map((s) => s.id)),
);

type Row = {
  /// Identity: the stanza directory, the step-id stem, and what every
  /// action here is keyed on.
  id: string;
  kind: EntryKind;
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
  revealBlocked: string | null;
  lastSynced: string | null;
  lastStatus: string;
  approximate: boolean;
  /// Null when nothing is on disk yet — rendered as "—", not "0 B",
  /// which would read as "ran, and produced nothing".
  bytes: number | null;
  /// Storage rows for this entry's declared outputs.
  outputs: OutputStorage[];
  /// Absolute path to reveal: the first output that exists.
  revealPath: string | null;
};

const KIND_LABEL: Record<EntryKind, string> = {
  source: "Source",
  step: "Step",
  applet: "Applet",
};

/// Most recent job naming this source. `source_name` is a comma-joined
/// list of step ids for a multi-source run, and an all-sources run
/// leaves it null — which is why `approximate` exists.
function jobFor(name: string): { job: SyncJob; exact: boolean } | null {
  for (const job of jobs.value) {
    if (!job.source_name) {
      return { job, exact: false };
    }
    const ids = job.source_name.split(",").map((s) => s.trim());
    if (ids.some((id) => id === name || id.startsWith(`${name}.`))) {
      return { job, exact: ids.length === 1 };
    }
  }
  return null;
}

const rows = computed<Row[]>(() =>
  sources.value.map((s) => {
    const entry = s.type ? catalogFor(s.type) : undefined;
    const hit = s.kind === "applet" ? null : jobFor(s.id);
    const outputs = s.outputs
      .map((path) => storage.value.find((x) => x.path === path))
      .filter((x): x is OutputStorage => !!x);
    const onDisk = outputs.filter((o) => o.present);

    // Run: the DAG schedules steps, so a source and a plain step can
    // both be targeted; an applet is spawned by the gateway on demand
    // and has nothing to enqueue.
    const runBlocked =
      s.kind === "applet"
        ? "Applets aren't scheduled — the server starts one when something asks for it."
        : null;

    // Edit: the wizard's forms describe source types. Everything else
    // is hand-written config, and the honest answer is to say so.
    let editBlocked: string | null = null;
    if (s.kind === "applet") {
      editBlocked = "No form for applets — edit this one in Advanced below.";
    } else if (s.kind === "step") {
      editBlocked = "This is a shared pipeline step, not a source; it has no form.";
    } else if (!entry) {
      editBlocked = "This source's step isn't a datalib-step command the catalog knows.";
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

    const revealBlocked =
      s.kind === "applet"
        ? "An applet owns no files — it serves endpoints."
        : onDisk.length === 0
          ? "Nothing on disk yet — this hasn't produced anything."
          : null;

    // An applet's health is its own thing: it isn't scheduled, so the
    // job queue says nothing about it. `GET /api/frontend` does.
    let lastStatus: string;
    if (s.kind === "applet") {
      lastStatus = appletErrors.value[s.id] ? "failed" : "running";
    } else {
      lastStatus = hit ? hit.job.state : "never run";
    }

    return {
      id: s.id,
      kind: s.kind,
      kindLabel: KIND_LABEL[s.kind],
      type: s.type,
      name: s.name,
      typeLabel: entry?.label ?? s.type ?? (s.kind === "source" ? "unknown" : "—"),
      icon: entry?.icon ?? null,
      entry,
      runBlocked,
      editBlocked,
      revealBlocked,
      lastSynced: hit?.job.finished_at ?? hit?.job.started_at ?? null,
      lastStatus,
      approximate: hit ? !hit.exact : false,
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
      return row.approximate
        ? "From a run covering several sources — per-source status needs the step_runs table."
        : undefined;
    },
    cellRenderer: (p: ICellRendererParams<Row>) => {
      const span = document.createElement("span");
      span.className = `m2-status m2-status-${p.data?.lastStatus.replace(/\s+/g, "-")}`;
      span.textContent = p.data?.approximate
        ? `${p.data.lastStatus} ~`
        : (p.data?.lastStatus ?? "");
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
    width: canReveal ? 132 : 102,
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
    sources.value = listConfiguredSources(configText.value);
    parseError.value = null;
  } catch (e) {
    parseError.value = (e as Error).message;
  }
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

function openAdd() {
  editing.value = null;
  wizardOpen.value = true;
}

function openEdit(id: string) {
  const source = sources.value.find((s) => s.id === id);
  if (!source?.type) return;
  const entry = catalogFor(source.type);
  if (!entry) return;
  editing.value = { source, entry };
  wizardOpen.value = true;
}

async function onWizardSubmit(payload: { id: string; name: string; body: string }) {
  const current = editing.value;
  let next = current
    ? replaceSource(configText.value, current.source, payload.body)
    : appendSource(configText.value, payload.body);
  // The fan-ins name their inputs, so a new source has to be wired into
  // them or it renders and is never indexed. Idempotent, so re-saving an
  // edit doesn't duplicate the entry.
  if (payload.body.includes(`id = "${payload.id}/rendered_md"`)) {
    next = wireIntoFanIns(next, `${payload.id}/rendered_md`);
  }
  // Banners are for a person, so they say the name; the id is what the
  // config and the disk use.
  const shown = payload.name || payload.id;
  const ok = await writeConfig(next, current ? `Saved ${shown}.` : `Added ${shown}.`);
  if (ok) {
    wizardOpen.value = false;
    editing.value = null;
  }
}

async function deleteSource(id: string) {
  const source = sources.value.find((s) => s.id === id);
  if (!source) return;
  const name = source.name;
  const ok = window.confirm(
    source.kind === "applet"
      ? `Remove the "${name}" applet from the config?\n\n` +
          `The server stops it. Anything in the app that its components or endpoints ` +
          `serve will stop working until you add it back.`
      : source.kind === "step"
        ? `Remove the "${name}" step from the config?\n\n` +
            `Its outputs stay on disk but stop being refreshed. For a shared index step ` +
            `that means search results go stale.`
        : `Remove "${name}" from the config?\n\n` +
            `Its data stays on disk and stays searchable — only the sync stops. ` +
            `Re-adding it later resumes from what's already downloaded.`,
  );
  if (!ok) return;
  // Unwire before removing: an input naming a step that no longer
  // exists is a config the runner refuses outright.
  let next = configText.value;
  for (const step of source.steps) {
    if (step.phase === "render") next = unwireFromFanIns(next, step.id);
  }
  await writeConfig(removeSource(next, source), `Removed ${name}.`);
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
  const source = sources.value.find((s) => s.id === id);
  // A source is targeted through its download step (the render step
  // follows from the artifact edges); a plain step by its own id.
  const target =
    source?.stepId
    ?? source?.steps.find((s) => s.phase === "download")?.id
    ?? source?.steps[0]?.id
    ?? id;
  busy.value = true;
  banner.value = null;
  try {
    await enqueueJob({ kind: "all", source_name: target });
    banner.value = { ok: true, text: `Queued a sync for ${source?.name ?? id}.` };
    await loadJobs();
  } catch (e) {
    banner.value = { ok: false, text: (e as Error).message };
  } finally {
    busy.value = false;
  }
}

let stream: EventSource | null = null;
let poll: ReturnType<typeof setInterval> | null = null;

onMounted(async () => {
  await Promise.all([loadConfig(), loadJobs(), loadStorage(), loadAppletHealth()]);
  stream = openJobStream(() => void loadJobs());
  // The config can change under us — an agent PUTs it, or the Manage
  // tab saves. Same cadence the Manage tab polls at.
  poll = setInterval(() => {
    void loadConfig();
    void loadJobs();
    void loadStorage();
    void loadAppletHealth();
  }, 5000);
});

onUnmounted(() => {
  if (stream) stream.close();
  if (poll) clearInterval(poll);
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
      breakdown. “Last status” comes from the job queue, which records whole runs rather than
      individual steps, so a <code>~</code> marks a status inferred from a run that covered
      several.
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
      :editing="editing"
      @close="wizardOpen = false; editing = null"
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

.m2-status { text-transform: capitalize; }
.m2-status-running { color: var(--datalib-accent); font-weight: 600; }
.m2-status-failed { color: var(--datalib-log-error); font-weight: 600; }
.m2-status-done { color: var(--datalib-muted); }
.m2-status-running { color: var(--datalib-accent); font-weight: 600; }

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
