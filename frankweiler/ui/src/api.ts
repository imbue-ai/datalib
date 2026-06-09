// Thin fetch wrapper for the Frankweiler HTTP API.
//
// In dev (vite), `/api/*` is proxied to the Rust backend via vite.config.ts.
// In Tauri/openhost packaging, the same relative paths are served by the
// embedded backend.

import type { FeedbackContext } from "./feedback/context";

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
  when: string;
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
  slack_link: string;
  // For Notion rows: the page-level UUID the row belongs to. Empty otherwise.
  notion_page_uuid: string;
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
  const r = await fetch(url, { signal });
  if (!r.ok) throw new Error(`${url} → ${r.status}`);
  return (await r.json()) as T;
}

export function fetchHealth(signal?: AbortSignal): Promise<Health> {
  return getJson<Health>("/api/health", signal);
}

export function fetchSearch(
  q: string,
  limit = 200,
  signal?: AbortSignal,
): Promise<SearchResponse> {
  const params = new URLSearchParams({ q, limit: String(limit) });
  return getJson<SearchResponse>(`/api/search?${params.toString()}`, signal);
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
