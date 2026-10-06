// TOML written by hand, into text a person also edits: a quoted string,
// and an array of strings edited where it stands, keeping how it was
// written — on one line, or one id per line with its indentation, its
// trailing comma and the comments and blank lines between the ids. An
// edit is refused unless it reads back as the ids asked for. The server's
// config upgrade edits an array the same way, in Rust
// (`datalib/backend/dag/src/config_array.rs`, over `config_lex.rs`).

import { parseTOML, getStaticTOMLValue } from "toml-eslint-parser";

/// What `tokens` cuts TOML text into. `str` is a string of any of the four
/// kinds, quotes included; only a multi-line one spans a line break.
/// `punct` is one of `[ ] { } , =`; `bare` is anything else — a key, a
/// number, `true`.
type Kind = "str" | "comment" | "punct" | "newline" | "space" | "bare";

type Token = { kind: Kind; text: string; start: number };

function tokens(text: string, from: number): Token[] {
  const out: Token[] = [];
  const newlineAt = (i: number) => text[i] === "\n" || text.startsWith("\r\n", i);
  for (let i = from; i < text.length;) {
    const start = i;
    let kind: Kind;
    if (newlineAt(i)) {
      kind = "newline";
      i += text[i] === "\r" ? 2 : 1;
    } else if (" \t\r".includes(text[i])) {
      kind = "space";
      while (i < text.length && " \t\r".includes(text[i]) && !newlineAt(i)) i++;
    } else if (text[i] === "#") {
      kind = "comment";
      while (i < text.length && !newlineAt(i)) i++;
    } else if ("[]{},=".includes(text[i])) {
      kind = "punct";
      i++;
    } else if (text[i] === '"' || text[i] === "'") {
      kind = "str";
      i = stringEnd(text, i);
    } else {
      kind = "bare";
      while (i < text.length && !" \t\r\n#[]{},=\"'".includes(text[i])) i++;
    }
    out.push({ kind, text: text.slice(start, i), start });
  }
  return out;
}

/// Past the closing quote of the string opening at `i`. A one-line string
/// left open ends at its line break.
function stringEnd(text: string, i: number): number {
  const q = text[i];
  const triple = text.startsWith(q.repeat(3), i);
  let j = i + (triple ? 3 : 1);
  while (j < text.length) {
    if (q === '"' && text[j] === "\\") {
      j += 2;
    } else if (triple && text.startsWith(q.repeat(3), j)) {
      // Up to two more quotes are the string's own last characters.
      let end = j + 3;
      while (end < text.length && end < j + 5 && text[end] === q) end++;
      return end;
    } else if (!triple && text[j] === q) {
      return j + 1;
    } else if (!triple && text[j] === "\n") {
      return j;
    } else {
      j++;
    }
  }
  return text.length;
}

type Id = {
  kind: "id";
  /// As written, quotes and all.
  token: string;
  /// The string it spells; null for anything that is not a string.
  value: string | null;
  /// The whitespace before it when it starts a line; null when it follows
  /// something else on its line.
  indent: string | null;
  /// A comment after it on its line, with the space before the `#`.
  note: string;
};

type Entry = Id | { kind: "comment"; indent: string; text: string } | { kind: "blank" };

type Scanned = {
  entries: Entry[];
  /// A comment on the `[` line, with the space before it.
  head: string;
  multiline: boolean;
  trailingComma: boolean;
  /// The indentation of a `]` on its own line; null when it closes the
  /// last line of entries.
  close: string | null;
  /// Just past the `]`.
  end: number;
};

/// Rewrite the array whose `[` is at `open` to hold `edit`'s strings: those
/// that stay keep their place, and new ones go at the end. Unchanged text
/// when the strings do not change.
export function editStringArray(
  text: string,
  open: number,
  edit: (values: string[]) => string[],
): string {
  const scanned = scan(text, open);
  const before = scanned.entries.flatMap(stringOf);
  const after = edit(before);
  if (after.length === before.length && after.every((v, i) => v === before[i])) return text;
  const kept = scanned.entries.filter((e) => stringOf(e).every((v) => after.includes(v)));
  const added: Id[] = after
    .filter((v) => !before.includes(v))
    .map((v) => ({ kind: "id", token: quote(v), value: v, indent: null, note: "" }));
  const entries = [...kept, ...added];
  const edited = render(scanned, entries);
  const wanted = entries.flatMap(stringOf);
  const got = stringsIn(edited);
  if (!got || got.length !== wanted.length || got.some((v, i) => v !== wanted[i])) {
    throw new Error(`the edited array would not read back as ${JSON.stringify(wanted)}: ${edited}`);
  }
  return text.slice(0, open) + edited + text.slice(scanned.end);
}

function isId(e: Entry): e is Id {
  return e.kind === "id";
}

/// The entry's string, as a list of none or one.
function stringOf(e: Entry): string[] {
  return isId(e) && e.value !== null ? [e.value] : [];
}

/// The strings of the array `text` holds, as TOML reads them; null when it
/// does not read as an array.
function stringsIn(text: string): string[] | null {
  try {
    const { v } = getStaticTOMLValue(parseTOML(`v = ${text}`)) as { v: unknown };
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : null;
  } catch {
    return null;
  }
}

function scan(text: string, open: number): Scanned {
  const [first, ...rest] = tokens(text, open);
  if (first?.text !== "[") throw new Error(`no array at ${open}`);
  const s: Scanned = {
    entries: [],
    head: "",
    multiline: false,
    trailingComma: false,
    close: null,
    end: 0,
  };
  // Only whitespace so far on a line after the first.
  let lineStart = false;
  let space = "";
  let onThisLine: Id | null = null;
  for (const t of rest) {
    if (t.kind === "space") {
      space += t.text;
      continue;
    }
    if (t.kind === "newline") {
      if (lineStart) s.entries.push({ kind: "blank" });
      s.multiline = true;
      lineStart = true;
      space = "";
      onThisLine = null;
      continue;
    }
    if (t.kind === "punct" && t.text === "]") {
      return { ...s, close: lineStart ? space : null, end: t.start + 1 };
    } else if (t.kind === "punct" && t.text === ",") {
      s.trailingComma = true;
    } else if (t.kind === "punct") {
      throw new Error(`${JSON.stringify(t.text)} in an array of strings`);
    } else if (t.kind === "comment") {
      if (onThisLine) onThisLine.note = space + t.text;
      else if (!s.multiline) s.head = space + t.text;
      else s.entries.push({ kind: "comment", indent: space, text: t.text });
    } else {
      const id: Id = {
        kind: "id",
        token: t.text,
        value: stringsIn(`[${t.text}]`)?.[0] ?? null,
        indent: lineStart ? space : null,
        note: "",
      };
      s.entries.push(id);
      onThisLine = id;
      s.trailingComma = false;
    }
    lineStart = false;
    space = "";
  }
  throw new Error("the array is not closed");
}

function render(s: Scanned, entries: Entry[]): string {
  const ids = entries.filter(isId);
  if (!s.multiline) return `[${ids.map((e) => e.token).join(", ")}]`;
  const hadIds = s.entries.some(isId);
  const trailingComma = hadIds ? s.trailingComma : true;
  const indents = s.entries
    .map((e) => (e.kind === "blank" ? null : e.indent))
    .filter((x): x is string => x !== null);
  const indent = indents[0] ?? `${s.close ?? ""}  `;
  const last = ids[ids.length - 1];
  const lines = entries.map((e) => {
    if (e.kind === "blank") return "";
    if (e.kind === "comment") return e.indent + e.text;
    const comma = e !== last || trailingComma ? "," : "";
    return `${e.indent ?? indent}${e.token}${comma}${e.note}`;
  });
  const open = `[${s.head}\n`;
  const tail = entries[entries.length - 1];
  // A `]` after a comment would be part of it.
  const closeInline = s.close === null && tail !== undefined && isId(tail) && tail.note === "";
  if (closeInline) return `${open}${lines.join("\n")}]`;
  const close = `${s.close ?? ""}]`;
  return lines.length ? `${open}${lines.join("\n")}\n${close}` : `${open}${close}`;
}

/// TOML basic string. Dates are quoted too: a bare `2026-01-01` parses
/// as a TOML date, and the providers validate a *string*.
export function quote(s: string): string {
  const escaped = s
    .replace(/\\/g, "\\\\")
    .replace(/"/g, '\\"')
    .replace(/\n/g, "\\n")
    .replace(/\r/g, "\\r")
    .replace(/\t/g, "\\t")
    // Everything else TOML calls a control char, as \uXXXX.
    .replace(
      /[\u0000-\u001f\u007f]/g,
      (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, "0")}`,
    );
  return `"${escaped}"`;
}
