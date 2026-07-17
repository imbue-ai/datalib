// Thin fetch wrapper for the Frankweiler HTTP API.
//
// In dev (vite), `/api/*` is proxied to the Rust backend via vite.config.ts.
// In Tauri/openhost packaging, the same relative paths are served by the
// embedded backend.

import type { FeedbackContext } from "./feedback/context";
import { pushToast } from "./toasts";

export type SearchRow = {
  uuid: string;
  conversation_uuid: string;
  // FK into the markdowns table — every grid row knows which rendered
  // .md it lives inside. Drives `/api/chat/{markdown_uuid}` lookups
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
  source: string;
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
  // Provider-assigned stable id (the grid_rows `external_id` column);
  // empty when unset. For Perseus it's the locator path — `"1"` (book),
  // `"1.2"` (chapter), `"1.2.3"` (section) — which perseusView parses to
  // build its book→chapter→section tree.
  external_id: string;
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
// `/api/chat/{uuid}` response — see `EdgeRowOut` in
// `frankweiler/backend/core/src/repo.rs`. `src_anchor_uuid`/
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

export type Health = {
  ok: boolean;
  version: string;
  root: string;
  root_exists: boolean;
};

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

export function fetchHealth(signal?: AbortSignal): Promise<Health> {
  return getJson<Health>("/api/health", signal);
}

export async function fetchSearch(
  q: string,
  limit = 200,
  signal?: AbortSignal,
): Promise<SearchResponse> {
  const params = new URLSearchParams({ q, limit: String(limit) });
  const r = await getJson<SearchResponse>(
    `/api/search?${params.toString()}`,
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
    `/api/chat/${encodeURIComponent(markdownUuid)}`,
    signal,
  );
}

// --- Config / setup API ----------------------------------------------------
//
// The data root is self-contained: its `config.yaml` lives at
// `<root>/config.yaml` and is read/written through these endpoints. A
// fresh root has no config (`exists: false`); the Setup view scaffolds
// one, lets the user edit, and PUTs it back.

export type ConfigResponse = {
  // Absolute path of `<root>/config.yaml`.
  path: string;
  // Whether that file exists yet (false on a fresh data root).
  exists: boolean;
  // Raw YAML text ("" when missing).
  yaml: string;
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

// PUT the edited YAML. The backend validates before persisting; a
// validation failure comes back as `{ok:false, error}` (HTTP 200), not a
// thrown error, so the caller can show it inline.
export async function saveConfig(
  yaml: string,
  signal?: AbortSignal,
): Promise<SaveConfigResponse> {
  const r = await fetch("/api/config", {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ yaml }),
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

export type SyncSource = {
  name: string;
  // Discriminator from the config (e.g. `claude_api`, `notion_api`,
  // `claude_export`). Encodes both provider and provenance.
  type: string;
  // True iff the source has a `sync:` block.
  managed: boolean;
};

export type SyncJobState = "pending" | "running" | "done" | "failed" | "canceled";
export type SyncJobKind = "download" | "ingest" | "render" | "all";

export type SyncJob = {
  id: string;
  kind: SyncJobKind;
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

export function fetchSyncSources(signal?: AbortSignal): Promise<SyncSource[]> {
  return getJson<SyncSource[]>("/api/sync/sources", signal);
}

// One DAG task's state on a job's task board. `state` is one of
// todo / running / done / skipped / failed / blocked.
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

export function fetchJob(id: string, signal?: AbortSignal): Promise<SyncJob> {
  return getJson<SyncJob>(`/api/sync/jobs/${encodeURIComponent(id)}`, signal);
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

// --- Cards (arbitrary JS visualizations) -----------------------------------
//
// POST /api/card stores a JS source string content-addressed by sha256 and
// returns the hash. GET /api/card/{hash} fetches it back. The URL only
// carries the hash, so the JS body never needs to fit in the URL.

export async function createCard(source: string, signal?: AbortSignal): Promise<string> {
  const r = await fetch("/api/card", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ source }),
    signal,
  });
  if (!r.ok) throw new Error(`POST /api/card → ${r.status}`);
  const j = (await r.json()) as { hash: string };
  return j.hash;
}

export async function fetchCard(hash: string, signal?: AbortSignal): Promise<string> {
  const r = await fetch(`/api/card/${encodeURIComponent(hash)}`, { signal });
  if (!r.ok) throw new Error(`GET /api/card/${hash} → ${r.status}`);
  return await r.text();
}

// --- Component library (named, mutable card aliases) -----------------------
//
// GET  /api/lib            → [{name, hash}] manifest of every component
// GET  /api/lib/{name}     → the component's JS source
// PUT  /api/lib/{name}     → create/overwrite, body {source}, returns {name,hash}
//
// `hash` is the sha256 of the source; the UI polls the manifest and
// re-renders a card when an alias it depends on changes hash.

export type LibEntry = { name: string; hash: string };

export async function listLib(signal?: AbortSignal): Promise<LibEntry[]> {
  const r = await fetch("/api/lib", { signal });
  if (!r.ok) throw new Error(`GET /api/lib → ${r.status}`);
  return (await r.json()) as LibEntry[];
}

export async function fetchLib(name: string, signal?: AbortSignal): Promise<string> {
  const r = await fetch(`/api/lib/${encodeURIComponent(name)}`, { signal });
  if (!r.ok) throw new Error(`GET /api/lib/${name} → ${r.status}`);
  return await r.text();
}

export async function putLib(
  name: string,
  source: string,
  signal?: AbortSignal,
): Promise<LibEntry> {
  const r = await fetch(`/api/lib/${encodeURIComponent(name)}`, {
    method: "PUT",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ source }),
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
