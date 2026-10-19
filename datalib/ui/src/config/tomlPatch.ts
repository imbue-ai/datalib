// Set or remove one key inside one `[[…]]` entry of a TOML file, touching
// nothing else: every other key, comment, blank line and the way each
// table is written (a `[header]`, dotted keys, an inline `{ … }`) stays
// as it was. An edit is refused (throws) unless the file then reads back
// as exactly the entry asked for, with every other entry unchanged.

import { parseTOML, getStaticTOMLValue } from "toml-eslint-parser";

import { quote } from "./tomlText";

type Ast = ReturnType<typeof parseTOML>;
type Table = Extract<Ast["body"][0]["body"][number], { type: "TOMLTable" }>;
type KeyValue = Table["body"][number];
type InlineTable = Extract<KeyValue["value"], { type: "TOMLInlineTable" }>;

/// Which entry: the array it is in and its index there (`["steps", 3]`).
export type EntryRef = { array: string; index: number };

/// A key/value pair inside the entry, by its full path below the entry.
type Pair = {
  path: string[];
  node: KeyValue;
  /// What the pair is written in: a `[header]` table's body, or an
  /// inline table's.
  in: { kind: "table"; table: Table } | { kind: "inline"; inline: InlineTable };
};

/// Where in the entry a path names a table that more keys can join.
type Container =
  | { kind: "header"; table: Table }
  | { kind: "inline"; inline: InlineTable }
  /// A table that exists only because a dotted key (`api.x = 1`) made it,
  /// inside `table`; `prefix` is the dotted part written there.
  | { kind: "dotted"; table: Table; prefix: string[] }
  /// A table that exists only as the parent of a deeper `[header]`.
  | { kind: "implicit" };

type Scan = { tables: Table[]; pairs: Pair[]; containers: Map<string, Container> };

const pathKey = (path: string[]) => JSON.stringify(path);

function keyNames(kv: KeyValue): string[] {
  return kv.key.keys.map((k) => (k.type === "TOMLBare" ? k.name : k.value));
}

function scan(ast: Ast, ref: EntryRef): Scan {
  const tables = ast.body[0].body.filter(
    (n): n is Table =>
      n.type === "TOMLTable" && n.resolvedKey[0] === ref.array && n.resolvedKey[1] === ref.index,
  );
  const pairs: Pair[] = [];
  const containers = new Map<string, Container>();
  const addContainer = (path: string[], c: Container) => {
    const k = pathKey(path);
    // A header or an inline table says more about where to write than
    // an implicit parent does.
    if (!containers.has(k) || containers.get(k)!.kind === "implicit") containers.set(k, c);
  };
  const walkInline = (inline: InlineTable, prefix: string[]) => {
    addContainer(prefix, { kind: "inline", inline });
    for (const kv of inline.body) {
      const path = [...prefix, ...keyNames(kv)];
      pairs.push({ path, node: kv, in: { kind: "inline", inline } });
      if (kv.value.type === "TOMLInlineTable") walkInline(kv.value, path);
    }
  };
  for (const table of tables) {
    const base = table.resolvedKey.slice(2).map(String);
    for (let i = 0; i < base.length; i++) addContainer(base.slice(0, i), { kind: "implicit" });
    addContainer(base, { kind: "header", table });
    for (const kv of table.body) {
      const names = keyNames(kv);
      for (let i = 1; i < names.length; i++) {
        addContainer([...base, ...names.slice(0, i)], {
          kind: "dotted",
          table,
          prefix: names.slice(0, i),
        });
      }
      const path = [...base, ...names];
      pairs.push({ path, node: kv, in: { kind: "table", table } });
      if (kv.value.type === "TOMLInlineTable") walkInline(kv.value, path);
    }
  }
  return { tables, pairs, containers };
}

/// A key as TOML writes it: bare when it can be, quoted otherwise.
function keyText(name: string): string {
  return /^[A-Za-z0-9_-]+$/.test(name) ? name : quote(name);
}

function dotted(path: string[]): string {
  return path.map(keyText).join(".");
}

/// The offset just past the end of the line `at` is on, before its line
/// break — so an insertion there keeps a trailing comment on its own line.
function lineEnd(text: string, at: number): number {
  const nl = text.indexOf("\n", at);
  return nl < 0 ? text.length : nl;
}

function lineStart(text: string, at: number): number {
  return text.lastIndexOf("\n", at - 1) + 1;
}

/// Where a new key/value line goes in a `[header]` table: after the line
/// its last pair ends on, or after the header itself when it has none.
function endOfTable(text: string, table: Table): number {
  const last = table.body.at(-1);
  return lineEnd(text, last ? last.range[1] : table.key.range[1]);
}

function insert(text: string, at: number, s: string): string {
  return text.slice(0, at) + s + text.slice(at);
}

function parse(text: string): Ast {
  try {
    return parseTOML(text);
  } catch (e) {
    throw new Error(`The config does not parse: ${(e as Error).message ?? String(e)}`);
  }
}

/// `text` with `path` (below the entry) set to `value`, written as TOML.
export function setIn(text: string, ref: EntryRef, path: string[], value: string): string {
  const ast = parse(text);
  const { tables, pairs, containers } = scan(ast, ref);
  if (tables.length === 0) throw new Error(`No ${ref.array} entry ${ref.index} to edit.`);
  let next: string;
  const hit = pairs.find((p) => pathKey(p.path) === pathKey(path));
  if (hit) {
    const [start, end] = hit.node.value.range;
    next = text.slice(0, start) + value + text.slice(end);
  } else {
    // The deepest table on the path that already exists takes the key.
    let depth = path.length - 1;
    while (depth > 0 && !containers.has(pathKey(path.slice(0, depth)))) depth--;
    const at = containers.get(pathKey(path.slice(0, depth)))!;
    const rest = path.slice(depth);
    if (at.kind === "inline") {
      const pair = `${dotted(rest)} = ${value}`;
      const last = at.inline.body.at(-1);
      next = last
        ? insert(text, last.range[1], `, ${pair}`)
        : text.slice(0, at.inline.range[0]) + `{ ${pair} }` + text.slice(at.inline.range[1]);
    } else if (at.kind === "dotted") {
      const lastOfPrefix = at.table.body
        .filter((kv) => pathKey(keyNames(kv).slice(0, at.prefix.length)) === pathKey(at.prefix))
        .at(-1)!;
      next = insert(
        text,
        lineEnd(text, lastOfPrefix.range[1]),
        `\n${dotted([...at.prefix, ...rest])} = ${value}`,
      );
    } else if (at.kind === "header" && rest.length === 1) {
      next = insert(text, endOfTable(text, at.table), `\n${keyText(rest[0])} = ${value}`);
    } else {
      // A table of its own, after the entry's last: a header there still
      // belongs to this entry, and `[a.b]` reads better than `a.b = …`
      // under the entry's own header.
      const header = dotted([ref.array, ...path.slice(0, -1)]);
      const last = tables.at(-1)!;
      next = insert(
        text,
        endOfTable(text, last),
        `\n\n[${header}]\n${keyText(path.at(-1)!)} = ${value}`,
      );
    }
  }
  const before = getStaticTOMLValue(ast) as Record<string, unknown>;
  const valueRead = (getStaticTOMLValue(parse(`v = ${value}`)) as { v: unknown }).v;
  checkReadsBack(next, ref, entryWith(before, ref, path, valueRead), path);
  return next;
}

/// `text` without `path` (below the entry). A path the entry does not
/// hold is left alone.
export function deleteIn(text: string, ref: EntryRef, path: string[]): string {
  const ast = parse(text);
  const { pairs } = scan(ast, ref);
  const hit = pairs.find((p) => pathKey(p.path) === pathKey(path));
  if (!hit) return text;
  let next: string;
  if (hit.in.kind === "table") {
    // The whole line, with any comment trailing it.
    const start = lineStart(text, hit.node.range[0]);
    const end = Math.min(text.length, lineEnd(text, hit.node.range[1]) + 1);
    next = text.slice(0, start) + text.slice(end);
  } else {
    const siblings = hit.in.inline.body;
    const i = siblings.indexOf(hit.node);
    if (siblings.length === 1) {
      const [start, end] = hit.in.inline.range;
      next = text.slice(0, start) + "{}" + text.slice(end);
    } else if (i < siblings.length - 1) {
      next = text.slice(0, hit.node.range[0]) + text.slice(siblings[i + 1].range[0]);
    } else {
      next = text.slice(0, siblings[i - 1].range[1]) + text.slice(hit.node.range[1]);
    }
  }
  const before = getStaticTOMLValue(ast) as Record<string, unknown>;
  checkReadsBack(next, ref, entryWith(before, ref, path, undefined), path);
  return next;
}

/// The whole file's value with one path in one entry set (or removed,
/// for `undefined`), and any table emptied by a removal pruned the way a
/// line removal would prune a table only a dotted key made.
function entryWith(
  root: Record<string, unknown>,
  ref: EntryRef,
  path: string[],
  value: unknown,
): unknown {
  const copy = structuredClone(root);
  const entry = (copy[ref.array] as Record<string, unknown>[])[ref.index];
  let cur = entry;
  for (const seg of path.slice(0, -1)) {
    if (typeof cur[seg] !== "object" || cur[seg] === null) cur[seg] = {};
    cur = cur[seg] as Record<string, unknown>;
  }
  if (value === undefined) delete cur[path.at(-1)!];
  else cur[path.at(-1)!] = value;
  return copy;
}

function checkReadsBack(next: string, ref: EntryRef, expected: unknown, path: string[]) {
  let got: unknown;
  try {
    got = getStaticTOMLValue(parseTOML(next));
  } catch (e) {
    throw new Error(
      `Writing ${dotted(path)} would break the config (${(e as Error).message}); it was left as it is.`,
    );
  }
  if (!sameExceptEmptyTables(got, expected, ref, path)) {
    throw new Error(
      `Writing ${dotted(path)} would change more than that key; the config was left as it is.`,
    );
  }
}

/// Equal, except that a removal may also take the tables only the removed
/// key's dotted name made (`api.x = 1` is the whole of `api`): those are
/// gone from `got` and empty in `expected`, up to some depth.
function sameExceptEmptyTables(got: unknown, expected: unknown, ref: EntryRef, path: string[]) {
  const want = canonical(got);
  if (want === canonical(expected)) return true;
  const pruned = structuredClone(expected) as Record<string, unknown>;
  const entry = (pruned[ref.array] as Record<string, unknown>[])[ref.index];
  for (let depth = path.length - 1; depth > 0; depth--) {
    let parent: Record<string, unknown> = entry;
    for (const seg of path.slice(0, depth - 1)) parent = parent[seg] as Record<string, unknown>;
    const child = parent[path[depth - 1]];
    if (!child || typeof child !== "object" || Object.keys(child).length > 0) return false;
    delete parent[path[depth - 1]];
    if (want === canonical(pruned)) return true;
  }
  return false;
}

/// A value as a string that ignores the order of a table's keys, which a
/// file's layout decides and the config's meaning does not.
function canonical(value: unknown): string {
  return JSON.stringify(value, (_, v: unknown) =>
    v && typeof v === "object" && !Array.isArray(v) && !(v instanceof Date)
      ? Object.fromEntries(Object.entries(v).sort(([a], [b]) => a.localeCompare(b)))
      : v,
  );
}
