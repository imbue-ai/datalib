<script setup lang="ts">
// Manager2 — the Manage tab inverted, per docs/dev/source_wizard.md.
//
// A grid of configured sources is the page; "Add Data Source" sits
// above it; each row carries Run / Edit / Delete, plus Reveal in the
// desktop app. The raw config editor is here but collapsed — demoted,
// not removed, because it stays the source of truth and the wizard's
// "edit as TOML" escape hatch has to lead somewhere.
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
import { computed, nextTick, onMounted, onUnmounted, ref } from "vue";
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
  fetchSourceStorage,
  enqueueJob,
  openJobStream,
  type SyncJob,
  type SourceStorage,
} from "@/api";
import {
  listConfiguredSources,
  appendSource,
  removeSource,
  replaceSource,
  paramsAreRepresentable,
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
const parseError = ref<string | null>(null);
const loadError = ref<string | null>(null);
const banner = ref<{ ok: boolean; text: string } | null>(null);
const busy = ref(false);
const jobs = ref<SyncJob[]>([]);
const storage = ref<SourceStorage[]>([]);
const sources = ref<ConfiguredSource[]>([]);

// The Advanced disclosure. Closed on load: the point of this tab is
// that a text editor is not the first thing you meet.
const configOpen = ref(false);
const configDirty = ref(false);
const editorEl = ref<HTMLTextAreaElement | null>(null);

// Resolved once — the desktop bridge either exists for this window or
// it doesn't, and the label depends only on the platform.
const canReveal = isDesktopApp();
const revealLabel = revealActionLabel();

const wizardOpen = ref(false);
const editing = ref<{ source: ConfiguredSource; entry: CatalogEntry } | null>(null);

const takenNames = computed(() => new Set(sources.value.map((s) => s.name)));

type Row = {
  name: string;
  type: string | null;
  label: string;
  icon: string | null;
  entry: CatalogEntry | undefined;
  /// Null when the wizard can round-trip this source; otherwise why not.
  editBlocked: string | null;
  lastSynced: string | null;
  lastStatus: string;
  approximate: boolean;
  /// Null when the source has no directory yet — rendered as "—", not
  /// "0 B", which would read as "synced and empty".
  bytes: number | null;
  storage: SourceStorage | undefined;
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
    const hit = jobFor(s.name);
    let editBlocked: string | null = null;
    if (!entry) {
      editBlocked = "This source's step isn't a datalib-step command the catalog knows.";
    } else if (!entry.wizard) {
      editBlocked = `No guided form for ${entry.label} yet — edit it in the Manage tab.`;
    } else {
      const rep = paramsAreRepresentable(s, entry);
      if (!rep.ok) {
        editBlocked =
          `The form doesn't model ${rep.unknown.join(", ")}, and saving would drop it. ` +
          `Edit this one in the Manage tab.`;
      }
    }
    const size = storage.value.find((x) => x.name === s.name);
    return {
      bytes: size && size.present ? size.total_bytes : null,
      storage: size,
      name: s.name,
      type: s.type,
      label: entry?.label ?? s.type ?? "unknown",
      icon: entry?.icon ?? null,
      entry,
      editBlocked,
      lastSynced: hit?.job.finished_at ?? hit?.job.started_at ?? null,
      lastStatus: hit ? hit.job.state : "never run",
      approximate: hit ? !hit.exact : false,
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

const columnDefs: ColDef<Row>[] = [
  {
    headerName: "Name",
    field: "name",
    flex: 2,
    minWidth: 180,
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
      return wrap;
    },
  },
  { headerName: "Type", field: "label", flex: 1, minWidth: 110 },
  {
    headerName: "Last synced",
    field: "lastSynced",
    flex: 1,
    minWidth: 150,
    valueFormatter: (p) => (p.value ? new Date(p.value as string).toLocaleString() : "—"),
  },
  {
    headerName: "Last status",
    field: "lastStatus",
    flex: 1,
    minWidth: 130,
    tooltipValueGetter: (p: { data?: Row }) =>
      p.data?.approximate
        ? "From a run covering several sources — per-source status needs the step_runs table."
        : undefined,
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
    flex: 1,
    minWidth: 140,
    // The breakdown is what answers "why is this 40 GB?" — attachments
    // routinely dwarf both the entity store and the rendered markdown.
    tooltipValueGetter: (p: { data?: Row }) => {
      const s = p.data?.storage;
      if (!s || !s.present) return "Nothing on disk yet — this source hasn't synced.";
      const parts = [
        `entities ${formatBytes(s.raw_bytes)}`,
        `attachments ${formatBytes(s.blobs_bytes)}`,
        `markdown ${formatBytes(s.rendered_bytes)}`,
      ];
      if (s.raw_elsewhere) {
        parts.push("plus a raw store held outside the data root, which isn't counted");
      }
      return parts.join(" · ");
    },
    valueFormatter: (p) => (p.value === null ? "—" : formatBytes(p.value as number)),
  },
  {
    headerName: "Actions",
    colId: "actions",
    sortable: false,
    filter: false,
    flex: 1,
    minWidth: canReveal ? 350 : 250,
    valueGetter: (p: ValueGetterParams<Row>) => p.data?.name,
    cellRenderer: (p: ICellRendererParams<Row>) => {
      const wrap = document.createElement("span");
      wrap.className = "m2-actions";
      const mk = (text: string, cls: string, disabledWhy: string | null, fn: () => void) => {
        const b = document.createElement("button");
        b.textContent = text;
        b.className = `m2-btn ${cls}`;
        if (disabledWhy) {
          b.disabled = true;
          b.title = disabledWhy;
        }
        b.addEventListener("click", (e) => {
          e.stopPropagation();
          fn();
        });
        wrap.appendChild(b);
      };
      const row = p.data!;
      mk("Run", "", null, () => runSource(row.name));
      mk("Edit", "", row.editBlocked, () => openEdit(row.name));
      // Absent rather than disabled in a plain browser — the same
      // "a missing menu item, not a broken one" rule desktop.ts states.
      if (canReveal) {
        mk(
          revealLabel,
          "muted",
          row.storage?.present ? null : "Nothing on disk yet — this source hasn't synced.",
          () => reveal(row.name),
        );
      }
      mk("Config", "muted", null, () => showInConfig(row.name));
      mk("Delete", "danger", null, () => deleteSource(row.name));
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
    // The poll must never overwrite what someone is typing into the
    // Advanced editor. Their text wins until they save or discard.
    if (configDirty.value) return;
    configText.value = cfg.text;
    reparse();
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
    storage.value = await fetchSourceStorage();
  } catch {
    // Same: a missing size column beats an error banner over the grid.
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

function openEdit(name: string) {
  const source = sources.value.find((s) => s.name === name);
  if (!source?.type) return;
  const entry = catalogFor(source.type);
  if (!entry) return;
  editing.value = { source, entry };
  wizardOpen.value = true;
}

async function onWizardSubmit(payload: { name: string; body: string }) {
  const current = editing.value;
  const next = current
    ? replaceSource(configText.value, current.source, payload.body)
    : appendSource(configText.value, payload.body);
  const ok = await writeConfig(
    next,
    current ? `Saved ${payload.name}.` : `Added ${payload.name}.`,
  );
  if (ok) {
    wizardOpen.value = false;
    editing.value = null;
  }
}

async function deleteSource(name: string) {
  const source = sources.value.find((s) => s.name === name);
  if (!source) return;
  const ok = window.confirm(
    `Remove "${name}" from the config?\n\n` +
      `Its data stays on disk and stays searchable — only the sync stops. ` +
      `Re-adding it later resumes from what's already downloaded.`,
  );
  if (!ok) return;
  await writeConfig(removeSource(configText.value, source), `Removed ${name}.`);
}

async function reveal(name: string) {
  const path = rows.value.find((r) => r.name === name)?.storage?.path;
  if (!path) return;
  const ok = await revealInFileManager(path);
  if (!ok) {
    banner.value = { ok: false, text: `Could not open ${path} in the file manager.` };
  }
}

/// Open the Advanced editor and select this source's stanza in it.
///
/// A textarea doesn't scroll to its own selection, so estimate the
/// target line's offset from the line count and the computed line
/// height — the same approach the Manage tab's "Locate config" uses.
async function showInConfig(name: string) {
  const source = sources.value.find((s) => s.name === name);
  if (!source) return;
  configOpen.value = true;
  await nextTick();
  const el = editorEl.value;
  if (!el) return;
  el.focus();
  // A zero range means the step has no tables of its own to select —
  // an inline `steps = [{…}]` entry. Leave the cursor alone rather
  // than selecting some unrelated span.
  if (source.end === 0) return;
  el.setSelectionRange(source.start, source.end);
  const lineHeight = Number.parseFloat(getComputedStyle(el).lineHeight) || 16;
  const line = configText.value.slice(0, source.start).split("\n").length - 1;
  el.scrollTop = Math.max(0, line * lineHeight - el.clientHeight / 3);
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

async function runSource(name: string) {
  const source = sources.value.find((s) => s.name === name);
  const target = source?.steps.find((s) => s.phase === "download")?.id
    ?? source?.steps[0]?.id
    ?? name;
  busy.value = true;
  banner.value = null;
  try {
    await enqueueJob({ kind: "all", source_name: target });
    banner.value = { ok: true, text: `Queued a sync for ${name}.` };
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
  await Promise.all([loadConfig(), loadJobs(), loadStorage()]);
  stream = openJobStream(() => void loadJobs());
  // The config can change under us — an agent PUTs it, or the Manage
  // tab saves. Same cadence the Manage tab polls at.
  poll = setInterval(() => {
    void loadConfig();
    void loadJobs();
    void loadStorage();
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
        <h2>Data sources</h2>
        <p class="m2-path">
          <code>{{ configPath }}</code>
        </p>
      </div>
      <button class="m2-add" :disabled="busy || !!parseError" @click="openAdd">
        + Add Data Source
      </button>
    </header>

    <p v-if="loadError" class="m2-msg bad">Could not load the config: {{ loadError }}</p>
    <p v-if="parseError" class="m2-msg bad">
      The config doesn’t parse, so nothing here can be trusted: {{ parseError }}
    </p>
    <p v-if="banner" class="m2-msg" :class="banner.ok ? 'good' : 'bad'">{{ banner.text }}</p>

    <div class="m2-grid">
      <AgGridVue
        :theme="gridTheme"
        :columnDefs="columnDefs"
        :rowData="rows"
        :getRowId="(p: { data: Row }) => p.data.name"
        :domLayout="'autoHeight'"
        :tooltipShowDelay="200"
        @grid-ready="onGridReady"
      />
    </div>

    <p v-if="rows.length === 0 && !parseError" class="m2-empty">
      No data sources configured yet. <b>Add Data Source</b> walks you through one.
    </p>

    <p class="m2-note">
      Account and document-count columns aren’t here yet — each needs a backend endpoint the
      design calls for. “Bytes on disk” is a directory walk; hover a value for the split between
      entities, attachments and rendered markdown. “Last status” comes from the job queue, which
      records whole runs rather than individual sources, so a <code>~</code> marks a status
      inferred from a run that covered several.
    </p>

    <details class="m2-advanced" :open="configOpen" @toggle="configOpen = ($event.target as HTMLDetailsElement).open">
      <summary>Advanced — edit <code>config.toml</code> directly</summary>
      <p class="m2-advanced-note">
        The file is the source of truth; everything above is a view of it. This is where to go
        for anything the forms don’t model — a source type with no wizard yet, or a knob like
        <code>common.download_params</code> that would make a row’s Edit button refuse.
      </p>
      <textarea
        ref="editorEl"
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

    <SourceWizard
      v-if="wizardOpen"
      :taken-names="takenNames"
      :editing="editing"
      @close="wizardOpen = false; editing = null"
      @submit="onWizardSubmit"
    />
  </section>
</template>

<style scoped>
.m2 { padding: 16px 20px 40px; }
.m2-head { display: flex; align-items: flex-start; gap: 16px; }
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

.m2-grid { margin-top: 16px; }
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

.m2-status { text-transform: capitalize; }
.m2-status-failed { color: var(--datalib-log-error); font-weight: 600; }
.m2-status-done { color: var(--datalib-muted); }
.m2-status-running { color: var(--datalib-accent); font-weight: 600; }

.m2-actions { display: inline-flex; gap: 6px; }
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
