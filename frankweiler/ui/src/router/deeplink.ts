// TypeScript companion to frankweiler/backend/core/src/deeplink.rs
// Both implementations parse the same grammar and must agree on the
// shared fixtures in tests/deeplink-fixtures.json.

export type Route =
  | { kind: "search"; params: Record<string, string> }
  | { kind: "chat"; markdownUuid: string; params: Record<string, string> }
  | { kind: "prefs" };

export class ParseError extends Error {}

export function parse(url: string): Route {
  let s = url;
  if (s.startsWith("frankweiler://")) s = s.slice("frankweiler://".length);
  else if (s.startsWith("#")) s = s.slice(1);
  else if (s.startsWith("/")) s = s.slice(1);
  if (!s) throw new ParseError("empty url");

  const qIdx = s.indexOf("?");
  const path = qIdx === -1 ? s : s.slice(0, qIdx);
  const query = qIdx === -1 ? "" : s.slice(qIdx + 1);
  const params = parseQuery(query);

  const slash = path.indexOf("/");
  const head = slash === -1 ? path : path.slice(0, slash);
  const rest = slash === -1 ? "" : path.slice(slash + 1);

  if (head === "search") return { kind: "search", params };
  if (head === "prefs") return { kind: "prefs" };
  if (head === "chat") {
    if (!rest) throw new ParseError("missing chat uuid");
    return { kind: "chat", markdownUuid: rest, params };
  }
  throw new ParseError(`unknown route: ${head}`);
}

export function toHash(route: Route): string {
  switch (route.kind) {
    case "search":
      return withQuery("search", route.params);
    case "chat":
      return withQuery(`chat/${route.markdownUuid}`, route.params);
    case "prefs":
      return "prefs";
  }
}

export function toDeeplink(route: Route): string {
  return `frankweiler://${toHash(route)}`;
}

function parseQuery(q: string): Record<string, string> {
  const out: Record<string, string> = {};
  if (!q) return out;
  for (const pair of q.split("&")) {
    if (!pair) continue;
    const eq = pair.indexOf("=");
    const k = eq === -1 ? pair : pair.slice(0, eq);
    const v = eq === -1 ? "" : pair.slice(eq + 1);
    out[decode(k)] = decode(v);
  }
  return out;
}

function withQuery(path: string, params: Record<string, string>): string {
  const keys = Object.keys(params).sort();
  if (keys.length === 0) return path;
  const q = keys.map((k) => `${encode(k)}=${encode(params[k])}`).join("&");
  return `${path}?${q}`;
}

function encode(s: string): string {
  let out = "";
  for (const ch of s) {
    if (/[A-Za-z0-9\-_.~:,]/.test(ch)) {
      out += ch;
    } else {
      const bytes = new TextEncoder().encode(ch);
      for (const b of bytes) out += "%" + b.toString(16).toUpperCase().padStart(2, "0");
    }
  }
  return out;
}

function decode(s: string): string {
  const bytes: number[] = [];
  let i = 0;
  while (i < s.length) {
    if (s[i] === "%" && i + 2 < s.length) {
      const hex = s.slice(i + 1, i + 3);
      const b = parseInt(hex, 16);
      if (!isNaN(b)) {
        bytes.push(b);
        i += 3;
        continue;
      }
    }
    if (s[i] === "+") {
      bytes.push(0x20);
    } else {
      const cp = s.charCodeAt(i);
      if (cp < 0x80) {
        bytes.push(cp);
      } else {
        for (const b of new TextEncoder().encode(s[i])) bytes.push(b);
      }
    }
    i++;
  }
  return new TextDecoder().decode(new Uint8Array(bytes));
}
