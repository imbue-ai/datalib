// The server's API token, as this page holds it. The launch URL carries it
// once as `?token=`; it is moved from there into this origin's
// `localStorage` before anything reads the URL, and every request the page
// makes to its own server sends it. The server sets no cookie
// (backend/http/src/auth.rs), so nothing the browser attaches by itself
// proves anything: a page on another origin, or another port, has no token
// to send.

const STORAGE_KEY = "datalib-api-token";
const QUERY_KEY = "token";

function stored(): string | null {
  try {
    return localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

function store(token: string) {
  try {
    localStorage.setItem(STORAGE_KEY, token);
  } catch {
    // Storage refused (a private window with it blocked): the token
    // still works for this page's life, from `current`.
  }
}

/// The URL with `?token=` taken out, and the token, if it had one.
export function splitLaunchToken(href: string): { href: string; token: string | null } {
  const url = new URL(href);
  const token = url.searchParams.get(QUERY_KEY);
  if (token === null) return { href, token: null };
  url.searchParams.delete(QUERY_KEY);
  return { href: url.href, token };
}

let current: string | null = null;

/// Run once, before the router reads the address bar. A launch URL's
/// token replaces the stored one: each run of the server mints a new
/// one, and the launcher is how a person gets it.
export function adoptLaunchToken() {
  const { href, token } = splitLaunchToken(location.href);
  if (token) {
    store(token);
    history.replaceState(history.state, "", href);
  }
  current = token ?? stored();
}

export function apiToken(): string | null {
  return current;
}

function isOwnOrigin(url: string): boolean {
  try {
    return new URL(url, location.href).origin === location.origin;
  } catch {
    return false;
  }
}

/// `url` with the token in its query, for a load that cannot send a
/// header: an `<img>`, a `<video>`, an `EventSource`. Anything not on
/// this origin (a `data:` URL, a remote one) comes back unchanged, so
/// the token never leaves for another host.
export function withToken(url: string): string {
  const token = current;
  if (!token || !isOwnOrigin(url)) return url;
  const u = new URL(url, location.href);
  u.searchParams.set(QUERY_KEY, token);
  return /^[a-z][a-z0-9+.-]*:/i.test(url) ? u.href : `${u.pathname}${u.search}${u.hash}`;
}

/// Every `fetch` to this origin sends the token, so no call site has to.
/// One that already sets `Authorization` (the Vite dev proxy's caller,
/// a test) keeps its own.
export function installTokenOnFetch() {
  const original = window.fetch.bind(window);
  window.fetch = (input: RequestInfo | URL, init?: RequestInit) => {
    const token = current;
    const url = input instanceof Request ? input.url : String(input);
    if (!token || !isOwnOrigin(url)) return original(input, init);
    const headers = new Headers(
      init?.headers ?? (input instanceof Request ? input.headers : undefined),
    );
    if (!headers.has("authorization")) headers.set("authorization", `Bearer ${token}`);
    return original(input, { ...init, headers });
  };
}

/// What the page shows instead of the app when the server refuses its
/// token: none was given, or the server has restarted since.
export function showTokenRequired(root: HTMLElement) {
  const doc = root.ownerDocument;
  const box = doc.createElement("main");
  box.className = "token-required";
  const add = (tag: string, text: string) => {
    const el = doc.createElement(tag);
    el.textContent = text;
    box.append(el);
  };
  add("h1", "This browser isn't authenticated");
  add(
    "p",
    "Datalib is running, but its API needs a token, the way a Jupyter server does. " +
      "Without one, any web page you have open could reach it.",
  );
  add(
    "p",
    "Each run of the server makes a new token. Open the URL the launcher printed, " +
      "or add the token to this address once:",
  );
  add("pre", `${location.pathname}?token=$(cat <data root>/system/api-token)`);
  root.replaceChildren(box);
}
