// Thin fetch wrapper for the Datalib HTTP API.

import type { FeedbackContext } from "./feedback/context";
import { pushToast } from "./toasts";

export type SearchRow = {
  uuid: string;
  conversation_uuid: string;
  // FK into the markdowns table — every grid row knows which rendered
  // .md it lives inside. Drives `{UNIFIED_INDEX}/chat/{markdown_uuid}` lookups
  // when the user clicks a row in the preview pane.
  markdown_uuid: string | null;
  message_index: number | null;
  snippet: string;
  sender: string;
  // Null when the row has no source-side timestamp (e.g. contacts
  // without a `REV:` field, or any row whose underlying entity isn't
  // event-shaped). AG Grid renders null as an empty cell.
  when: string | null;
  conversation_name: string;
  project: string;
  account: string;
  // Anthropic-only. Stable owning-org UUID; pair with org_name for display.
  // Empty for non-Anthropic rows.
  org_uuid: string;
  // Human-readable org name (from /api/organizations). Empty when missing.
  org_name: string;
  entire_chat: string;
  // The provider's human label ("Slack") — a property of the source
  // *type*. Two Slack workspaces both say "Slack"; source_id is what
  // separates them.
  source: string;
  // The **id** of the configured source this row came from: the group's
  // directory under the data root (the first segment of its qmd_path).
  // Empty when the row has no rendered document. The name a person gave
  // that group lives in config.toml, not here — the grid joins the two
  // client-side so renaming never needs a re-index.
  source_id: string;
  kind: string;
  author: string;
  channel: string;
  // Legacy Slack deep-link column; new rows carry their public URL in
  // source_url. The "Open source" action prefers source_url, falls back here.
  slack_link: string;
  // Public URL for the row's source artifact (Slack permalink, LinkedIn
  // post, …); empty when none.
  source_url: string;
  // For Notion rows: the page-level UUID the row belongs to. Empty otherwise.
  notion_page_uuid: string;
  // The upstream's own id for this entity (the grid_rows
  // `upstream_id` column); empty when the provider hasn't been
  // ported onto `datalib_id` yet. This is what "Copy source ID(s)"
  // copies, as opposed to `uuid` — ours resolves inside datalib, this
  // one resolves upstream.
  upstream_id: string;
  // What sort of upstream thing the row is, in the provider's own
  // vocabulary (`"pull_request"`, `"pr_review_comment"`, `"page"`).
  // Empty when unset. Disambiguates `upstream_id`, whose numeric
  // ids overlap across a provider's API namespaces.
  upstream_entity_kind: string;
  // Bytes on disk for what this row describes (a measured file or
  // store, a sized artifact). Null for rows with no meaningful size.
  byte_size: number | null;
  // How many things this row counts (rows in a measured table, pages in
  // a PDF). Null for rows that are a single thing.
  item_count: number | null;
  // QMD rank score. Present when the row came from a qmd-routed search;
  // omitted (undefined) for pure structured queries and the LIKE fallback.
  score?: number;
};

// Subset of `query_echo` the UI inspects. The backend ships additional
// keys (free_text, filters, resolved_type, …) that we ignore; typing
// only what we consume keeps the contract narrow.
export type QueryEcho = {
  // Set when the qmd-routed search failed and the backend fell back to
  // the SQL LIKE path. The UI surfaces this as a banner so users see
  // degraded search rather than silently get worse results.
  qmd_error?: string | null;
  [key: string]: unknown;
};

export type SearchResponse = {
  query_echo: QueryEcho;
  rows: SearchRow[];
  total_estimated: number;
  // Backend-side errors that don't fail the response — e.g. the
  // structured-search SQL errored and we returned zero rows rather than
  // surface a 500. `api.ts` raises each as a toast so the user sees
  // them; the field is omitted when empty (serde `skip_serializing_if`).
  errors?: string[];
};

// QMDs are write-only output. The backend ships the body verbatim
// (frontmatter stripped) and the UI runs markdown-it on it. Per-section
// scrolling/highlighting uses the `<div data-section-uuid="…">`
// wrappers the renderer emits (one per message, plus nested ones for
// tool_use / tool_result / thinking blocks). The attribute value is
// the same as the grid row's `uuid` column.
// One row from the `edges` table joined with the destination
// markdown's title. The backend produces this list on every
// `{UNIFIED_INDEX}/chat/{uuid}` response — see `EdgeRowOut` in
// `datalib/backend/core/src/repo.rs`. `src_anchor_uuid`/
// `dst_anchor_uuid` reference values the renderer emits as
// `data-section-uuid` attributes in the body; null means the
// corresponding side is the whole document.
export type EdgeOut = {
  edge_uuid: string;
  src_markdown_uuid: string;
  src_anchor_uuid: string | null;
  dst_markdown_uuid: string;
  dst_anchor_uuid: string | null;
  label: string | null;
  dst_title: string | null;
};

export type ChatResponse = {
  markdown_uuid: string;
  name: string | null;
  account: string | null;
  project: string | null;
  channel: string | null;
  created_at: string | null;
  source_label: string | null;
  source_url: string | null;
  body: string;
  outgoing_edges: EdgeOut[];
};

// One rendered document (a `markdowns` row), as listed by the applet
// for the document-picker card. `markdown_uuid` is the same UUID
// `documentView(...)` / `{UNIFIED_INDEX}/chat/{uuid}` take.
export type DocEntry = {
  markdown_uuid: string;
  title: string | null;
  kind: string;
  provider: string;
  created_at: string | null;
};
// --- The unified_index applet --------------------------------------------
export const UNIFIED_INDEX = "/applet/unified_index";


// Newest-first listing of rendered documents (capped server-side).
export function fetchDocs(signal?: AbortSignal): Promise<DocEntry[]> {
  return getJson<DocEntry[]>(`${UNIFIED_INDEX}/docs`, signal);
}

// --- qmd index state -------------------------------------------------------
export type QmdDocState = {
  indexed: boolean | null;
  embedded: boolean | null;
  // Present when either field is null, or when an otherwise-fine
  // document is not indexed. Shown as the cell's tooltip.
  note?: string;
};

export type QmdStateResponse = {
  // False for a data root that has never synced — there is no
  // index.sqlite, so every document is legitimately un-indexed.
  index_present: boolean;
  summary: { documents: number; embedded: number };
  // markdown_uuid → state. Every requested uuid appears.
  docs: Record<string, QmdDocState>;
  errors?: string[];
};

// Ask which of these rendered documents the qmd index holds. POST
// because the uuid list is as long as the grid's result set; the
// backend dedupes and caps it.
export async function fetchQmdState(
  markdownUuids: string[],
  signal?: AbortSignal,
): Promise<QmdStateResponse> {
  const r = await fetch(`${UNIFIED_INDEX}/qmd_state`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ markdown_uuids: markdownUuids }),
    signal,
  });
  if (!r.ok) {
    throw new Error(`POST ${UNIFIED_INDEX}/qmd_state → ${r.status}`);
  }
  const data = (await r.json()) as QmdStateResponse;
  if (data.errors && data.errors.length > 0) {
    for (const e of data.errors) pushToast(e);
  }
  return data;
}

export type Health = {
  ok: boolean;
  version: string;
  root: string;
  root_exists: boolean;
  // Absolute path of the file the running server published its API token
  // to. Used to tell a coding agent where to read it (see handoff.ts) —
  // the token itself never enters the UI.
  token_file: string;
};

// Last successful /api/health payload. `fetchHealth` fills it; consumers
// that need a fact from it but can't await (the wayfinder builders in
// handoff.ts, which run inside sync click handlers) read it from here.
let lastHealth: Health | null = null;

export function healthSnapshot(): Health | null {
  return lastHealth;
}

export type AccountInfo = {
  provider?: string;
  label?: string;
  email?: string | null;
};

export type AccountsMap = Record<string, AccountInfo>;

export function fetchAccounts(signal?: AbortSignal): Promise<AccountsMap> {
  return getJson<AccountsMap>("/api/accounts", signal);
}

async function getJson<T>(url: string, signal?: AbortSignal): Promise<T> {
  let r: Response;
  try {
    r = await fetch(url, { signal });
  } catch (e) {
    // Network error / aborted before headers. Don't toast on abort
    // (caller-initiated cancellation, e.g. debounced search supersession).
    if ((e as { name?: string }).name !== "AbortError") {
      pushToast(`${url}: ${(e as Error).message}`);
    }
    throw e;
  }
  if (!r.ok) {
    let detail = "";
    try {
      detail = (await r.text()).trim();
    } catch {
      // ignore
    }
    const msg = detail ? `${url} → ${r.status}: ${detail}` : `${url} → ${r.status}`;
    pushToast(msg);
    throw new Error(msg);
  }
  return (await r.json()) as T;
}

export async function fetchHealth(signal?: AbortSignal): Promise<Health> {
  const h = await getJson<Health>("/api/health", signal);
  lastHealth = h;
  return h;
}

export async function fetchSearch(
  q: string,
  limit = 200,
  signal?: AbortSignal,
): Promise<SearchResponse> {
  const params = new URLSearchParams({ q, limit: String(limit) });
  const r = await getJson<SearchResponse>(
    `${UNIFIED_INDEX}/search?${params.toString()}`,
    signal,
  );
  // Backend returned 200 but is telling us something went sideways
  // (schema mismatch, fallback path errored, etc.). Surface each entry
  // as its own toast — the dedupe window in `pushToast` keeps repeated
  // keystroke-driven searches from spamming the tray.
  if (r.errors && r.errors.length > 0) {
    for (const e of r.errors) pushToast(e);
  }
  return r;
}

export function fetchChat(
  markdownUuid: string,
  signal?: AbortSignal,
): Promise<ChatResponse> {
  // One UUID per rendered `.md` file — no disambiguation needed.
  // Provider-specific sharding (beeper's per-period files) is already
  // encoded in the markdown_uuid scheme.
  return getJson<ChatResponse>(
    `${UNIFIED_INDEX}/chat/${encodeURIComponent(markdownUuid)}`,
    signal,
  );
}

// --- Config / setup API ----------------------------------------------------

// One thing wrong with the config, and how much of it that costs.
// Mirrors `datalib_dag::diagnostics` — see that module for why there
// are four severities and not two. In short:
export type Severity = "fatal" | "rejected" | "blocked" | "warning";

export type Diagnostic = {
  severity: Severity;
  // Which `[[groups]]` / `[[steps]]` / `[[applets]]` entry, when it is
  // about one. `id` is null when the id itself is what's broken; `index`
  // is null for problems raised after loading, where the array position
  // has already shifted and the id is the identity.
  entry: {
    kind: "group" | "step" | "applet";
    index: number | null;
    id: string | null;
  } | null;
  message: string;
  // What to do about it — kept separate so it can be rendered as
  // secondary text rather than glued onto the message.
  help: string | null;
  // Byte range in the config text, for selecting the offending key in
  // the editor. Points at the key itself where the loader could find
  // it, else at the entry's `[[steps]]` header.
  span: [number, number] | null;
  line: number | null;
  column: number | null;
};

export type ConfigResponse = {
  // Absolute path of `<root>/config.toml`.
  path: string;
  // Whether that file exists yet (false on a fresh data root).
  exists: boolean;
  // Raw config text ("" when missing).
  text: string;
  // Whether the file is a config at all — i.e. nothing `fatal`.
  // Deliberately not "has no problems": a config with a rejected step
  // still loads and the app still runs on it. Ask `diagnostics`.
  parsed_ok: boolean;
  // The fatal diagnostic's message when parsed_ok is false.
  error: string | null;
  // Everything wrong with the file. Empty for a clean config.
  diagnostics: Diagnostic[];
  // Whether the app can serve its own views at all. False when the
  // file is not a config, or when it loads without a usable
  // `unified_index` applet — that applet serves the grid, search and
  // the document view, so without it every view is a 502. A root with
  // no config is `exists: false` and the first-run screen's business,
  // not this flag's.
  app_ready: boolean;
  source_count: number;
  // How to invoke the latchkey CLI on this install: the app-bundled
  // launcher's absolute path when running from the packaged app, else
  // an `npx -y latchkey@<pin>` fallback. Spliced into the Setup tab's
  // copy-pasteable credential snippets.
  latchkey_cli: string;
};

export type SaveConfigResponse = {
  // Whether the text is acceptable — and, for a save, whether it was
  // written. True only when every entry loads: the PUT door is stricter
  // than the loader on purpose, so that "saved" keeps meaning "saved
  // with nothing dropped". A warning leaves it true.
  ok: boolean;
  // The first diagnostic, for a caller that wants one line.
  error: string | null;
  // All of them, so an editor can show every problem at once instead
  // of one save per typo.
  diagnostics: Diagnostic[];
  source_count: number;
};

// Ask what the server makes of some config text without saving it —
// the editor's linter. Same verdict `saveConfig` would give, and the
// same shape, minus the write.
export async function checkConfig(
  text: string,
  signal?: AbortSignal,
): Promise<SaveConfigResponse> {
  const r = await fetch("/api/config/check", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ text }),
    signal,
  });
  if (!r.ok) throw new Error(`config check failed: ${r.status}`);
  return (await r.json()) as SaveConfigResponse;
}

export function fetchConfig(signal?: AbortSignal): Promise<ConfigResponse> {
  return getJson<ConfigResponse>("/api/config", signal);
}

// Server-generated minimal starter config. Used when the root has no
// config yet; the user fills in sources via the Setup tab's buttons.
export function fetchConfigScaffold(signal?: AbortSignal): Promise<ConfigResponse> {
  return getJson<ConfigResponse>("/api/config/scaffold", signal);
}

// What POST /api/config/init did. `created` is false both when a
// config was already there (`text` is that file, `error` null) and when
// the backend refused, in which case `error` says why.
export type InitConfigResponse = {
  created: boolean;
  path: string;
  text: string;
  error: string | null;
};

// Initialize an empty data library: write the starter config.toml into
// a root that has none. The "only if absent" check lives server-side
// (one `create_new`), so this can't clobber a config that appeared in
// between — a second window, a migration, an agent editing the root.
export async function initConfig(signal?: AbortSignal): Promise<InitConfigResponse> {
  const r = await fetch("/api/config/init", { method: "POST", signal });
  if (!r.ok) {
    let detail = "";
    try {
      detail = await r.text();
    } catch {
      // ignore
    }
    throw new Error(
      detail ? `${r.status}: ${detail}` : `POST /api/config/init → ${r.status}`,
    );
  }
  return (await r.json()) as InitConfigResponse;
}

// One step of the config's DAG (GET /api/dag), in topological order.
// `deps` are the edges — the ids the step names as inputs.
export type DagStep = {
  id: string;
  command: string;
  inputs: string[];
  outputs: string[];
  deps: string[];
  // What this step did the last time a run reached it, from the
  // runner's own state — so a run started from a terminal shows up here
  // exactly like one the app kicked off. Null when never reached.
  last_run: DagStepRun | null;
  // What it is doing in the run currently in flight. Null means the
  // scheduler hasn't reached it, which reads as queued.
  current_state: DagRunState | null;
  // What the step has reported in the current run, from the run store
  // (system/runs.sqlite). Null when it has reported nothing — which is
  // not zero, and should read as a spinner rather than an empty bar.
  progress: DagStepProgress | null;
};

// A step's live numbers and words. `metrics` is the current value per
// series, keyed `name` or `name{labels}`; `done` and `queued` are the
// two the runner derives for a step reporting a plain count, and the
// pair a bar can be drawn from. Empty means the step has only spoken.
export type DagStepProgress = {
  msg: string | null;
  metrics: Record<string, number>;
  updated_at: string;
};

// The fraction a step's `done` / `queued` pair describes, or null when
// the step has not said how much is ahead of it — a bar drawn from an
// invented total claims more than we know.
export function progressFraction(p: DagStepProgress | null | undefined): number | null {
  if (!p) return null;
  const done = p.metrics.done;
  const queued = p.metrics.queued;
  if (done == null || queued == null || done + queued <= 0) return null;
  return Math.max(0, Math.min(1, done / (done + queued)));
}

// What a step is doing, or did, in one run — the runner's own
// vocabulary (`RunState` in datalib/backend/dag/src/run_state.rs).
// Keep the two in step: the backend writes these words into
// system/dag_state.json and the UI switches on them.
export type DagRunState =
  | "running"
  | "succeeded"
  // Checked, and already up to date.
  | "skipped_up_to_date"
  // Outside this run's subgraph, so it was never considered.
  | "not_selected"
  // Something upstream failed, so this was not invoked.
  | "blocked"
  | "failed";

export type DagStepRun = {
  started_at: string;
  finished_at: string | null;
  // Empty while the step is still running.
  status: DagRunState | "";
  attempts: number;
  error: string | null;
};

export type DagRun = {
  run_id: string;
  started_at: string;
  finished_at: string | null;
  // Whether a runner actually holds the root right now. An open record
  // with `live: false` is a run that died — the lock is the truth, not
  // the absence of `finished_at`.
  live: boolean;
};

export type DagResponse = {
  ok: boolean;
  error: string | null;
  steps: DagStep[];
  run: DagRun | null;
};

export function fetchDag(signal?: AbortSignal): Promise<DagResponse> {
  return getJson<DagResponse>("/api/dag", signal);
}

// PUT the edited config text, which always lands in
// `<root>/config.toml`. The backend validates before persisting; a
// validation failure comes back as `{ok:false, error}` (HTTP 200), not
// a thrown error, so the caller can show it inline.
export async function saveConfig(
  text: string,
  signal?: AbortSignal,
): Promise<SaveConfigResponse> {
  const r = await fetch("/api/config", {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ text }),
    signal,
  });
  if (!r.ok) {
    let detail = "";
    try {
      detail = await r.text();
    } catch {
      // ignore
    }
    throw new Error(detail ? `${r.status}: ${detail}` : `PUT /api/config → ${r.status}`);
  }
  return (await r.json()) as SaveConfigResponse;
}

// --- Sync API --------------------------------------------------------------

// A source is any config step with no declared inputs (a fringe
// step — what a sync can target), identified by its step id.
export type SyncSource = {
  id: string;
};

export type SyncJobState = "pending" | "running" | "done" | "failed" | "canceled";
// The only kind enqueued today: one DAG run over the whole config
// (`source_ids` optionally narrows it to selected sources).
export type SyncJobKind = "all";

export type SyncJob = {
  id: string;
  // Free-form, not SyncJobKind: historical rows may carry retired
  // kinds ("download" / "ingest" / "render").
  kind: string;
  // Comma-separated source-step ids, or null for the whole config.
  source_ids: string | null;
  state: SyncJobState;
  progress_pct: number | null;
  progress_msg: string | null;
  error: string | null;
  created_at: string;
  started_at: string | null;
  finished_at: string | null;
  parent_job_id?: string | null;
  pid?: number | null;
};

export type StoragePart = { label: string; bytes: number };

/// Bytes for one declared tree. Keyed on the path: a step's id, or a
/// group's directory — the folder its steps write into, measured as a
/// tree of its own so the group row has a series rather than a sum.
export type OutputStorage = {
  /// The tree, data-root-relative: a step id, a group id, or "." for
  /// the root.
  path: string;
  /// Absolute, for the desktop app's reveal-in-file-manager IPC.
  abs: string;
  /// The directory doesn't exist yet. Distinct from a real zero, so the
  /// UI shows "—" rather than "0 B".
  present: boolean;
  bytes: number;
  parts?: StoragePart[];
  /// Recent measurements, oldest first — the sparkline behind the
  /// number. **Compacted**: the backend records a sample only when the
  /// value moves, and never twice within five seconds, so this is a
  /// step function and not an evenly-spaced series. The first entry may
  /// predate the window; it is the value the window opens at. See
  /// `config/sparkline.ts`, which is the only thing that should be
  /// interpreting it.
  history: UsageSample[];
};

/// One measurement. `at` is ISO-8601 with an explicit offset.
export type UsageSample = { at: string; bytes: number };

/// What the storage endpoint answers: the whole root, every declared
/// tree in it, and how far back the histories reach.
export type PipelineStorage = {
  /// The data root as a whole — including trees no step declares
  /// (`system/`, the stores). Its `path` is ".".
  root: OutputStorage;
  /// One per declared tree — every group's directory and every step's
  /// — in config order.
  outputs: OutputStorage[];
  /// The span each `history` covers, in seconds. Read rather than
  /// assumed, so the plot and the data can't disagree about what
  /// "recent" means.
  window_secs: number;
  /// When the last walk finished, or null when none has yet — the only
  /// case in which a zero doesn't mean an empty disk.
  measured_at: string | null;
};

/// The backend walks the disk on a tick *while a sync is running*, and
/// not at all between runs — so the routine poll is a cheap read of
/// what it last found, and on an idle root that answer can be old.
/// `refresh` asks it to walk first. Pass it at the two moments the
/// stale answer would be wrong on screen rather than merely old: the
/// first paint, and a sync going terminal. Refreshes that arrive
/// together share one walk server-side.
export function fetchPipelineStorage(
  refresh = false,
  signal?: AbortSignal,
): Promise<PipelineStorage> {
  const q = refresh ? "?refresh=1" : "";
  return getJson<PipelineStorage>(`/api/pipeline/storage${q}`, signal);
}

/// One table as it stood after one commit. Mirrors
/// `datalib_history::TableState`.
export type HistoryTable = {
  table: string;
  /// Rows after the commit.
  rows: number;
  added: number;
  deleted: number;
  modified: number;
};

/// One `dolt_commit`. `date` is ISO-8601 with an explicit `+00:00`.
export type HistoryCommit = {
  hash: string;
  /// First parent; null on the root commit.
  parent: string | null;
  committer: string;
  date: string;
  message: string;
  /// Every table, largest first, as of this commit.
  tables: HistoryTable[];
};

/// One `.doltlite_db` file's log, newest first.
export type StoreHistory = {
  /// The file, data-root-relative.
  path: string;
  commits: HistoryCommit[];
  /// The store has more commits than `limit` allowed.
  truncated: boolean;
};

/// What `GET /api/pipeline/history` answers for one declared tree: a
/// step's store(s), or for a group every store under its steps.
export type TreeHistory = {
  tree: string;
  stores: StoreHistory[];
};

export function fetchTreeHistory(
  tree: string,
  signal?: AbortSignal,
): Promise<TreeHistory> {
  return getJson<TreeHistory>(
    `/api/pipeline/history?tree=${encodeURIComponent(tree)}`,
    signal,
  );
}

export function fetchSyncSources(signal?: AbortSignal): Promise<SyncSource[]> {
  return getJson<SyncSource[]>("/api/sync/sources", signal);
}

// One DAG task's state on a job's task board: the runner's
// `DagRunState` with `todo` added for a step the scheduler has not
// reached, and shorter words for two of the outcomes. The translation
// lives in `TaskState::for_run_state`
// (datalib/backend/http/src/worker.rs); keep this union in step with it.
export type SyncTaskState =
  // In the plan, not yet reached.
  | "todo"
  | "running"
  // Ran to completion.
  | "done"
  // Checked, and already up to date.
  | "skipped"
  // Outside this run's subgraph, so it was never considered (a
  // per-source sync leaves most of the graph here).
  | "not_selected"
  | "failed"
  // Something upstream failed, so this was not invoked.
  | "blocked";

export type SyncTask = {
  id: string;
  state: SyncTaskState;
  detail?: string | null;
};

// One push update for a job: an *unnamed* frame on
// `GET /api/sync/stream`. The worker + enqueue/cancel handlers emit
// these the instant they write a job's state, so the UI updates without
// polling. `tasks` is the per-task board (also recoverable from
// `progress_msg`, which carries it as JSON — see src/sync/progress.ts).
export type JobProgressEvent = {
  id: string;
  kind: string;
  source_ids: string | null;
  state: SyncJobState;
  progress_pct: number | null;
  progress_msg: string | null;
  tasks?: SyncTask[] | null;
};

// The stream itself is opened by `@/live`, not here: it multiplexes one
// connection across every subscriber, and owns the reconnect and
// stall-detection policy. This module keeps only the wire types.

export function fetchActiveJobs(signal?: AbortSignal): Promise<SyncJob[]> {
  return getJson<SyncJob[]>("/api/sync/jobs", signal);
}

export function fetchAllJobs(limit = 50, signal?: AbortSignal): Promise<SyncJob[]> {
  const params = new URLSearchParams({ limit: String(limit) });
  return getJson<SyncJob[]>(`/api/sync/jobs/all?${params.toString()}`, signal);
}

export async function enqueueJob(
  req: { kind: SyncJobKind; source_ids?: string | null },
  signal?: AbortSignal,
): Promise<SyncJob> {
  const r = await fetch("/api/sync/jobs", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(req),
    signal,
  });
  if (!r.ok) {
    let detail = "";
    try {
      detail = await r.text();
    } catch {
      // ignore
    }
    throw new Error(detail ? `${r.status}: ${detail}` : `POST /api/sync/jobs → ${r.status}`);
  }
  return (await r.json()) as SyncJob;
}

export async function cancelJob(id: string, signal?: AbortSignal): Promise<void> {
  const r = await fetch(`/api/sync/jobs/${encodeURIComponent(id)}/cancel`, {
    method: "POST",
    signal,
  });
  if (!r.ok) {
    throw new Error(`POST /api/sync/jobs/${id}/cancel → ${r.status}`);
  }
}

export async function fetchJobLog(id: string, signal?: AbortSignal): Promise<string> {
  const r = await fetch(`/api/sync/jobs/${encodeURIComponent(id)}/log`, { signal });
  if (!r.ok) throw new Error(`GET /api/sync/jobs/${id}/log → ${r.status}`);
  return await r.text();
}

// --- Authoring the `user` namespace ----------------------------------------

// What a write to the `user` namespace returns: the name, the content
// hash of the source just stored, and the metadata document as written.
// Everything read back comes from /api/frontend instead — this type is
// only the acknowledgement of a PUT.
export type LibEntry = {
  name: string;
  hash: string;
  meta: Meta;
};

// One `<name>.json` in a namespace directory, as the server read it.
// Either a component or a rename tombstone — the same two shapes the
// file on disk has.
export type Meta =
  | {
      title: string;
      description: string;
      component_hash: string;
      component_args: unknown[];
    }
  | { renamed_to: string };

export type NamespaceView = {
  entries: Record<string, Meta>;
  // Files the namespace could not use, each with why.
  problems?: string[];
};

export type FrontendView = {
  // namespace → its components. `user` is the hand-authored one; the
  // rest are named after applets, but nothing downstream cares which.
  namespaces: Record<string, NamespaceView>;
  // applet id → why its write failed.
  applet_errors?: Record<string, string>;
};

export async function fetchFrontend(signal?: AbortSignal): Promise<FrontendView> {
  const r = await fetch("/api/frontend", { signal });
  if (!r.ok) throw new Error(`GET /api/frontend → ${r.status}`);
  return (await r.json()) as FrontendView;
}

// `description` semantics match the backend: undefined keeps whatever
// description is stored, "" clears it.
export async function putLib(
  name: string,
  source: string,
  description?: string,
  signal?: AbortSignal,
): Promise<LibEntry> {
  const r = await fetch(`/api/lib/${encodeURIComponent(name)}`, {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(
      description === undefined ? { source } : { source, description },
    ),
    signal,
  });
  if (!r.ok) throw new Error(`PUT /api/lib/${name} → ${r.status}`);
  return (await r.json()) as LibEntry;
}

export type FeedbackRequest = {
  sentiment: "up" | "down" | null;
  comment: string;
  context: FeedbackContext;
};

export type FeedbackResponse = {
  feedback_uuid: string;
  created_at: string;
  git_hash: string;
};

// POST /api/feedback. Server stamps the UUID, timestamp, app_version, and
// git_hash; we ship sentiment + comment + the producer-built context.
export async function submitFeedback(
  req: FeedbackRequest,
  signal?: AbortSignal,
): Promise<FeedbackResponse> {
  const r = await fetch("/api/feedback", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(req),
    signal,
  });
  if (!r.ok) {
    // Surface server message when present; fall back to status code so the
    // modal's error line says something more useful than "Failed to fetch".
    let detail = "";
    try {
      detail = await r.text();
    } catch {
      // ignore — body may not be readable on aborted responses
    }
    throw new Error(detail ? `${r.status}: ${detail}` : `POST /api/feedback → ${r.status}`);
  }
  return (await r.json()) as FeedbackResponse;
}

// Credentials and connection testing, for the Add-a-source wizard.

export type StoredAccount = {
  /// latchkey's account key. The empty string is a real value: it is
  /// how latchkey spells its one unnamed account for a service, and it
  /// is addressed by writing no `latchkey_settings.account` at all.
  account: string;
  credential_type: string | null;
  /// `valid`, `invalid`, `missing`, `unknown`.
  credential_status: string | null;
};

export type LatchkeyService = {
  service: string;
  /// `browser`, `set`, … — which ways this service can be authenticated.
  auth_options: string[];
  accounts: StoredAccount[];
  /// Whether latchkey knows this service at all. False means the name
  /// is free — the only state in which the wizard may register it.
  registered: boolean;
  /// How to invoke latchkey on the machine running the backend — a
  /// bundled path, or the `npx` fallback. Any command shown to a person
  /// has to start with this rather than a bare `latchkey`.
  cli: string;
  /// Set when latchkey itself could not be asked. Not fatal: the
  /// account can still be typed.
  error: string | null;
};

/// How to teach latchkey a service it has never heard of, so that a
/// browser login exists for it. Mirrors `ServiceRegistration` in
/// datalib/backend/http/src/connect.rs.
export type ServiceRegistration = {
  base_api_url: string;
  login_url: string;
  login_flow: "cookie-capture" | "token-capture";
  login_flow_params: Record<string, unknown>;
};

/// What one probe item is. Mirrors `ProbeItemKind` in
/// datalib/backend/probe/src/lib.rs, hand-kept in step:
/// `mailbox` (emails are filed here), `keyword` (a Gmail flag —
/// downloadable, but never matched by the render-side filter),
/// `conversation` (one chat thread — a Claude chat, a Slack DM) or
/// `channel` (a Slack channel).
export type ProbeItemKind = "mailbox" | "keyword" | "conversation" | "channel";

/// One row a probe offers a filter field. Mirrors `ProbeItem` in
/// datalib/backend/probe/src/lib.rs.
export type ProbeItem = {
  /// The exact string to put in the filter — a label path, a
  /// conversation uuid.
  path: string;
  kind: ProbeItemKind;
  /// A human name, when `path` is an opaque id.
  title: string | null;
  /// A short tag: a mailbox's role, a channel's `private`, a DM's
  /// `group`. Shown, never matched.
  role: string | null;
  messages: number | null;
  members: number | null;
  updated_at: string | null;
};

export type ProbeReport = {
  mode: string;
  account: {
    id: string;
    address: string | null;
    display_name: string | null;
    message_estimate: number | null;
  };
  items: ProbeItem[];
  notes: string[];
};

/// How one browser-login attempt is going. Mirrors `ConnectState` in
/// datalib/backend/http/src/connect.rs.
export type ConnectState = "running" | "ok" | "failed";

export type ConnectAttempt = {
  id: string;
  status: ConnectState;
  /// Which account latchkey filed the credential under — not the one
  /// asked for. `auth browser` ignores `--account` when storing and
  /// uses the identity the login yields (imbue-ai/latchkey#148), so its
  /// own report is the only reliable answer. Null when the flow had no
  /// identity to derive and used latchkey's unnamed default.
  account: string | null;
  output: string;
};

/// The server's message for a failed request, which for these routes is
/// a JSON `{error}` — the provider's own words ("Gmail
/// users.getProfile: HTTP 401 …"), which is the whole value of showing
/// it. Falls back to the raw body, then to the status code.
async function quietError(url: string, r: Response): Promise<Error> {
  let detail = "";
  try {
    const text = (await r.text()).trim();
    try {
      detail = (JSON.parse(text) as { error?: string }).error ?? text;
    } catch {
      detail = text;
    }
  } catch {
    // ignore — the status line below is still worth reporting
  }
  return new Error(detail || `${url} → ${r.status}`);
}

async function quietJson<T>(url: string, init?: RequestInit): Promise<T> {
  const r = await fetch(url, init);
  if (!r.ok) throw await quietError(url, r);
  return (await r.json()) as T;
}

/// Which accounts latchkey holds for one service, and how another could
/// be added.
export function latchkeyService(service: string): Promise<LatchkeyService> {
  return quietJson<LatchkeyService>(`/api/latchkey/${encodeURIComponent(service)}`);
}

/// Start latchkey's browser login. Returns immediately with an id to
/// poll — the flow waits for a person, so there is nothing to await.
export function startLatchkeyConnect(
  service: string,
  account?: string,
  register?: ServiceRegistration,
  ephemeralBrowser = false,
): Promise<ConnectAttempt> {
  return quietJson<ConnectAttempt>(`/api/latchkey/${encodeURIComponent(service)}/connect`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      account: account ?? "",
      register: register ?? null,
      ephemeral_browser: ephemeralBrowser,
    }),
  });
}

/// Poll one browser login. The server drops a finished attempt once it
/// has been read, so a second poll after `ok`/`failed` is a 404 — read
/// it once and keep the answer.
export function latchkeyConnectStatus(id: string): Promise<ConnectAttempt> {
  return quietJson<ConnectAttempt>(`/api/latchkey/connect/${encodeURIComponent(id)}/status`);
}

/// Ask a provider what these credentials can reach. `params` is the
/// **download** params, even when the caller is configuring a render
/// step: that is where the credentials and the download mode live.
export function probeSource(
  type: string,
  params: Record<string, unknown>,
): Promise<ProbeReport> {
  return quietJson<ProbeReport>("/api/probe", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ type, params }),
  });
}
