// What a credential pasted into the wizard looks like for one latchkey
// service: a username and password (an app password, over HTTP Basic),
// or one or more headers with the pasted secret in one of them. Read
// off latchkey's own `auth set` example for the service, unless the
// catalog says the example is wrong — a service registered by hand gets
// latchkey's generic Bearer example whatever it really takes.

import type { PastedCredential } from "@/api";

/// Where the pasted secret goes in a header template.
export const SECRET = "{secret}";

export type CredentialShape =
  | { kind: "basic"; userHint: string; secretLabel: string }
  | { kind: "headers"; headers: string[]; secretLabel: string };

/// The last resort: the person types the whole header line.
const WHOLE_HEADER: CredentialShape = { kind: "headers", headers: [SECRET], secretLabel: "header" };

export function credentialShape(example: string | null, override?: string[]): CredentialShape {
  if (override?.length) return { kind: "headers", headers: override, secretLabel: "token" };
  if (!example) return WHOLE_HEADER;

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
  if (shape.kind === "basic") {
    const u = username.trim();
    return u ? { kind: "basic", username: u, password: s } : null;
  }
  return { kind: "headers", headers: shape.headers.map((h) => h.replace(SECRET, s)) };
}
