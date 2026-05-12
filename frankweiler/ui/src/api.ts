// Thin fetch wrapper for the Frankweiler HTTP API.
//
// In dev (vite), `/api/*` is proxied to the Rust backend via vite.config.ts.
// In Tauri/openhost packaging, the same relative paths are served by the
// embedded backend.

export type SearchRow = {
  uuid: string;
  conversation_uuid: string;
  message_index: number | null;
  snippet: string;
  sender: string;
  when: string;
  conversation_name: string;
  project: string;
  account: string;
  entire_chat: string;
  source: string;
  kind: string;
  author: string;
  channel: string;
  slack_link: string;
  // For Notion rows: the page-level UUID the row belongs to. Empty otherwise.
  notion_page_uuid: string;
};

export type SearchResponse = {
  query_echo: unknown;
  rows: SearchRow[];
  columns: { field: string; header: string; default_visible: boolean }[];
  total_estimated: number;
};

// QMDs are write-only output. The backend ships the body verbatim
// (frontmatter stripped) and the UI runs markdown-it on it. Per-message
// scrolling/highlighting uses the `<div id="m-{uuid}" data-msg-index="…">`
// wrappers the renderer already emits in the body.
export type ChatResponse = {
  conversation_uuid: string;
  name: string | null;
  account: string | null;
  project: string | null;
  channel: string | null;
  created_at: string | null;
  source_label: string | null;
  source_url: string | null;
  body: string;
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

export function fetchChat(uuid: string, signal?: AbortSignal): Promise<ChatResponse> {
  return getJson<ChatResponse>(`/api/chat/${encodeURIComponent(uuid)}`, signal);
}
