// What a credential pasted into the wizard looks like for one latchkey
// service: a username and password (an app password, over HTTP Basic),
// one or more headers with the pasted secret in one of them, or a folder
// a plugin reads the credential from (`auth set-nocurl`). Read
// off latchkey's own `auth set` example for the service, unless the
// catalog says the example is wrong — a service registered by hand gets
// latchkey's generic Bearer example whatever it really takes.

import type { PastedCredential } from "@/api";

/// Where the pasted secret goes in a header template.
export const SECRET = "{secret}";

export type CredentialShape =
  | { kind: "basic"; userHint: string; secretLabel: string }
  | { kind: "headers"; headers: string[]; secretLabel: string }
  | { kind: "directory"; placeholder: string; secretLabel: string };

/// The last resort: the person types the whole header line.
const WHOLE_HEADER: CredentialShape = { kind: "headers", headers: [SECRET], secretLabel: "header" };

export function credentialShape(example: string | null, override?: string[]): CredentialShape {
  if (override?.length) return { kind: "headers", headers: override, secretLabel: "token" };
  if (!example) return WHOLE_HEADER;

  const folder = /\sauth\s+set-nocurl\s+\S+\s+(\S+)/.exec(example);
  if (folder)
    return { kind: "directory", placeholder: folder[1] ?? "", secretLabel: "token folder" };

  const basic = /\s-u\s+"([^":]*):([^"]*)"/.exec(example);
  if (basic) {
    return { kind: "basic", userHint: basic[1] ?? "", secretLabel: unbracket(basic[2] ?? "") };
  }

  const headers = [...example.matchAll(/\s-H\s+"([^"]*)"/g)].map((m) => m[1] ?? "");
  const placeholder = /<([^>]+)>/;
  const withSecret = headers.findIndex((h) => placeholder.test(h));
  if (withSecret >= 0) {
    const label = placeholder.exec(headers[withSecret]!)![1]!;
    return {
      kind: "headers",
      headers: headers.map((h, i) => (i === withSecret ? h.replace(placeholder, SECRET) : h)),
      secretLabel: label,
    };
  }
  // One header whose example value is a sample (`Bearer xoxb-your-token`):
  // the secret is its last word.
  if (headers.length === 1 && /\s\S+$/.test(headers[0]!)) {
    return {
      kind: "headers",
      headers: [headers[0]!.replace(/\S+$/, SECRET)],
      secretLabel: "token",
    };
  }
  return WHOLE_HEADER;
}

function unbracket(s: string): string {
  return s.replace(/^<|>$/g, "") || "password";
}

/// What the form sends, or null while a field it needs is empty.
export function pastedCredential(
  shape: CredentialShape,
  username: string,
  secret: string,
): PastedCredential | null {
  const s = secret.trim();
  if (!s) return null;
  if (shape.kind === "directory") return { kind: "directory", path: s };
  if (shape.kind === "basic") {
    const u = username.trim();
    return u ? { kind: "basic", username: u, password: s } : null;
  }
  return { kind: "headers", headers: shape.headers.map((h) => h.replace(SECRET, s)) };
}

/// What storing a pasted credential under `name` does to the credentials
/// latchkey already holds for the service. latchkey keeps one credential
/// per account name and overwrites it without asking, and once a named
/// credential sits beside the unnamed one it refuses to pick for a
/// source that names no account.
export type PasteTarget =
  | { kind: "unnamed" }
  | { kind: "replaces"; account: string }
  | { kind: "new"; besideUnnamed: boolean };

export function pasteTarget(stored: string[], name: string): PasteTarget {
  const account = name.trim();
  if (!account) return { kind: "unnamed" };
  if (stored.includes(account)) return { kind: "replaces", account };
  return { kind: "new", besideUnnamed: stored.includes("") };
}

/// The name a pasted credential is offered under: the username, and the
/// entry's suffix where two entries share one latchkey service.
export function suggestedAccount(username: string, suffix?: string): string {
  const user = username.trim();
  return user && suffix ? `${user} ${suffix}` : user;
}
