// Thin fetch wrapper for the Datalib HTTP API.
//
// In dev (vite), `/api/*` is proxied to the Rust backend via vite.config.ts.
// In Tauri/openhost packaging, the same relative paths are served by the
// embedded backend.
//
// Authentication is deliberately absent from this file. The backend
// requires its per-process API token on every route (see
// datalib/backend/http/src/auth.rs), but the browser gets it as an
// HttpOnly session cookie when it loads the app, and the dev-mode Vite
// proxy stamps it on server-side — so `fetch`, `EventSource`, and
// `<img src="/applet/unified_index/asset/…">` all authenticate without any call site
// here knowing about it. That is the point of carrying it in a cookie:
// there is no per-request token plumbing to forget.

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
  // *type*. Two Slack workspaces both say "Slack"; source_name is what
  // separates them.
  source: string;
  // The configured source this row came from: the stanza directory under
  // the data root (the first segment of its qmd_path). Empty when the row
  // has no rendered document. The friendly `label` a person may have put
  // on that source lives in config.toml, not here — the grid joins the
  // two client-side so relabelling never needs a re-index.
  source_name: string;
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
  //
  // For Perseus it's the locator path — `"1"` (book), `"1.2"`
  // (chapter), `"1.2.3"` (section) — which perseusView parses to build
  // its book→chapter→section tree.
  upstream_id: string;
  // What sort of upstream thing the row is, in the provider's own
  // vocabulary (`"pull_request"`, `"pr_review_comment"`, `"page"`).
  // Empty when unset. Disambiguates `upstream_id`, whose numeric
  // ids overlap across a provider's API namespaces.
  upstream_entity_kind: string;
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
  columns: { field: string; header: string; default_visible: boolean }[];
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
//
// Search, the document list, one document, and the files beside it are
// served by `datalib-applet unified_index`, reached through the
// gateway's applet proxy. `datalib-http` does not know these routes
// exist — it forwards `/applet/<id>/…` to whatever the config declares
// under that id.
//
// Consequence worth knowing: a data root whose `config.toml` does not
// declare this applet has no grid. The scaffold writes it, and the
// gateway answers 502 with the applet named when it is configured but
// not running, so the failure says which file to fix.
export const UNIFIED_INDEX = "/applet/unified_index";


// Newest-first listing of rendered documents (capped server-side).
export function fetchDocs(signal?: AbortSignal): Promise<DocEntry[]> {
  return getJson<DocEntry[]>(`${UNIFIED_INDEX}/docs`, signal);
}

// --- qmd index state -------------------------------------------------------
//
// What the qmd index currently holds for a set of rendered documents,
// behind the grid's `Indexed` / `Embedded` columns. Two separate
// booleans because `qmd update` and `qmd embed` are separate passes: a
// document can be findable by keyword and still invisible to semantic
// search for as long as the embed pass takes.
//
// `null` means "we could not determine this" (no rendered file, file
// unreadable, index unavailable) — distinct from `false`, which is a
// positive claim that the document is absent from the index. The grid
// renders the two differently, because a red ❌ we can't back up is
// worse than an honest blank.
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
//
// The data root is self-contained: its config lives at
// `<root>/config.toml` and is read/written through these endpoints. A
// fresh root has no config (`exists: false`); the Setup view scaffolds
// one, lets the user edit, and PUTs it back.
//
// TOML is the only format these endpoints handle. A data root written
// before the switch is converted once, out of band, by the separate
// `datalib-migrate-config` program; `legacy_yaml_path` below exists
// only so the UI can say so instead of showing an empty setup screen.

export type ConfigResponse = {
  // Absolute path of `<root>/config.toml`.
  path: string;
  // Whether that file exists yet (false on a fresh data root).
  exists: boolean;
  // Raw config text ("" when missing).
  text: string;
  // Whether the current bytes parse + validate.
  parsed_ok: boolean;
  // Loader error when parsed_ok is false.
  error: string | null;
  source_count: number;
  // How to invoke the latchkey CLI on this install: the app-bundled
  // launcher's absolute path when running from the packaged app, else
  // an `npx -y latchkey@<pin>` fallback. Spliced into the Setup tab's
  // copy-pasteable credential snippets.
  latchkey_cli: string;
  // Absolute path of a pre-TOML config.yaml sitting in this root, when
  // there is one and no config.toml yet. Purely a signpost: nothing
  // server-side reads it.
  legacy_yaml_path: string | null;
  // The exact command that converts it, set whenever legacy_yaml_path
  // is. Backend-resolved, because in the packaged desktop app the
  // migrator lives inside the bundle rather than on $PATH.
  legacy_migrate_cmd: string | null;
};

export type SaveConfigResponse = {
  ok: boolean;
  error: string | null;
  source_count: number;
};

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
// the backend refused — today only for a root holding a pre-TOML
// config.yaml, where starting empty would strand the user's sources.
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
  // What it is doing in the run currently in flight: "running",
  // "succeeded", "blocked", … Null means the scheduler hasn't reached
  // it, which reads as queued.
  current_state: string | null;
  // How far into the current run, from the progress bus
  // (system/progress.sqlite). Null when the step has reported nothing —
  // which is not zero, and should read as a spinner rather than an
  // empty bar.
  progress: DagStepProgress | null;
};

// A step's live position. `total: null` means indeterminate: a
// paginated walk that cannot know its length up front.
export type DagStepProgress = {
  done: number | null;
  total: number | null;
  msg: string | null;
  updated_at: string;
};

export type DagStepRun = {
  started_at: string;
  finished_at: string | null;
  // `succeeded` | `skipped_up_to_date` | `blocked` | `failed` |
  // `not_selected`. Empty while running.
  status: string;
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
// (`source_name` optionally narrows it to selected sources).
export type SyncJobKind = "all";

export type SyncJob = {
  id: string;
  // Free-form, not SyncJobKind: historical rows may carry retired
  // kinds ("download" / "ingest" / "render").
  kind: string;
  source_name: string | null;
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

/// Bytes for one declared output path. Keyed on the path rather than on
/// a source, because the grid groups steps into rows and that grouping
/// rule lives in `config/sourceSteps.ts` alone.
export type OutputStorage = {
  /// The tree, data-root-relative: a step id, or "." for the root.
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
  /// One per declared step, in config order.
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

export function fetchSyncSources(signal?: AbortSignal): Promise<SyncSource[]> {
  return getJson<SyncSource[]>("/api/sync/sources", signal);
}

// One DAG task's state on a job's task board. `state` is one of
// todo / running / done / skipped / not_selected / failed / blocked.
// `skipped` = checked, already up to date. `not_selected` = outside
// this run's subgraph, so it was never considered (a per-source sync
// leaves most of the graph there).
export type SyncTask = {
  id: string;
  state: string;
  detail?: string | null;
};

// One push update for a job, streamed from `GET /api/sync/stream` over
// SSE. The worker + enqueue/cancel handlers emit these the instant they
// write a job's state, so the UI updates without polling. `tasks` is
// the per-task board (also recoverable from `progress_msg`, which
// carries it as JSON — see src/sync/progress.ts).
export type JobProgressEvent = {
  id: string;
  kind: string;
  source_name: string | null;
  state: SyncJobState;
  progress_pct: number | null;
  progress_msg: string | null;
  tasks?: SyncTask[] | null;
};

// Open the live job-progress SSE stream. Returns the EventSource so the
// caller can close it on unmount. `onEvent` fires per job update; the
// browser auto-reconnects on transient drops.
export function openJobStream(onEvent: (e: JobProgressEvent) => void): EventSource {
  const es = new EventSource("/api/sync/stream");
  es.onmessage = (m) => {
    try {
      onEvent(JSON.parse(m.data) as JobProgressEvent);
    } catch {
      // ignore malformed frames
    }
  };
  return es;
}

export function fetchActiveJobs(signal?: AbortSignal): Promise<SyncJob[]> {
  return getJson<SyncJob[]>("/api/sync/jobs", signal);
}

export function fetchAllJobs(limit = 50, signal?: AbortSignal): Promise<SyncJob[]> {
  const params = new URLSearchParams({ limit: String(limit) });
  return getJson<SyncJob[]>(`/api/sync/jobs/all?${params.toString()}`, signal);
}

export async function enqueueJob(
  req: { kind: SyncJobKind; source_name?: string | null },
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
//
// GET  /api/lib/{name}          → the component's JS source
// PUT  /api/lib/{name}          → create/overwrite, body {source, …}
// POST /api/lib/{name}/rename   → move to {new_name}, leaving a tombstone
//
// A writer only. Everything read back — the manifest, the gallery, what
// `comp.user.x` resolves to — comes from /api/frontend, which reads the
// filesystem and cannot tell a user-written component from an
// applet-written one.

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
