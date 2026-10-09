// The config as the Manage screen reads and writes it: groups, the steps
// and applets filed under them, and the TOML the wizard produces.
//
// A `[[groups]]` entry is a source: an `id` that is the directory its
// steps write into, a `name` that is free text, and a `type`. A step
// under it is `group` + `function`, and its id is composed as
// `<group>/<function>` — never written, and never split. What a step is
// (ingest, render, index) is read off its `function`; which group it
// belongs to is read off `group`. Nothing here takes an id apart to
// learn either. A step outside any group carries a verbatim `id` and is
// a custom executable the wizard knows nothing about.
//
// The wizard writes a source as one unit — the group, its `ingest`
// step and its `render_markdown` step — from one form, and edits it the
// same way. Writes are whole-text: add/delete splice the text the
// editor holds. Field-level editing that preserves comments needs a
// format-preserving TOML writer, so until then `paramsAreRepresentable`
// gates the Edit button and the wizard never silently drops something
// it can't model.
//
// An applet is never scheduled and owns no artifacts, so most row
// actions don't apply to it — but it is configured, it can fail to
// start, and that should be visible here rather than as a 502 in
// another tab.

import { parseTOML, getStaticTOMLValue } from "toml-eslint-parser";

import { NAME_IT_HELP } from "./accountNaming";
import { formatBytes, parseByteSize } from "./byteSize";
import { catalogForStep } from "./catalog";
import type { Answer, CatalogEntry, Field, FieldPhase, Preset } from "./catalog";
import { editStringArray, quote } from "./tomlText";

/// Which wave a step belongs to, for display and for picking the right
/// half of a catalog entry's fields. Read off the step's `function`.
export type StepPhase = "ingest" | "render" | "index" | "other";

export type EntryKind = "step" | "applet";

export type ConfiguredStep = {
  /// Identity, and the tree this step writes: `<group>/<function>` for
  /// a grouped step, the written `id` for one outside any group.
  /// Path-safe, unique, and what the directory structure is formed
  /// from — so changing it moves data on disk and strands the paths the
  /// index recorded, which is why the wizard holds it fixed after
  /// creation.
  id: string;
  kind: EntryKind;
  /// The `[[groups]]` entry this step is filed under, and what it does
  /// there. Both null for a custom step with a verbatim id, and for an
  /// applet.
  group: string | null;
  function: string | null;
  /// What to show: the step's own `name =`; else, for a grouped step,
  /// the group's name (suffixed for its render step); else `id`.
  name: string;
  phase: StepPhase;
  /// The group's `type` for a grouped step; the word after
  /// `datalib-applet` for an applet; null for anything else, which is a
  /// legitimate config with no catalog entry.
  type: string | null;
  /// The ids this step declares as inputs.
  inputs: string[];
  params: Record<string, unknown>;
  /// [start, end) character offsets covering this entry's TOML tables,
  /// for splice-based edit and delete.
  start: number;
  end: number;
};

/// One `[[groups]]` entry as written.
export type ConfiguredGroup = {
  id: string;
  name: string | null;
  type: string | null;
  /// What this source is to its owner, in a sentence. Edited in the
  /// wizard and kept in the config; nothing reads it yet. Meant to help
  /// search one day — imbue-ai/datalib#409 is why it does not today.
  description: string | null;
  /// [start, end) character offsets covering the `[[groups]]` table.
  start: number;
  end: number;
};

/// The label for a grouped step that wrote no `name` of its own.
function groupedName(
  group: ConfiguredGroup,
  id: string,
  fn: string | null,
  phase: StepPhase,
): string {
  if (!group.name) return defaultName(id);
  if (phase === "ingest") return group.name;
  if (phase === "render") return `${group.name} (render markdown)`;
  if (fn === "keyword_index") return `${group.name} (keyword index)`;
  if (fn === "embed") return `${group.name} (embeddings)`;
  return defaultName(id);
}

/// The built-in functions, each the directory it writes. Mirrors
/// `datalib_step::function::Function`; hand-kept in step with it. Any
/// other function is a custom executable's.
const PHASE_BY_FUNCTION: Record<string, StepPhase> = {
  ingest: "ingest",
  render_markdown: "render",
  grid_index: "index",
  qmd_aggregator: "index",
  keyword_index: "index",
  embed: "index",
  embedding_map: "index",
};

/// A step's phase, from its function. A step outside any group has no
/// function and is a custom executable: `other`.
function phaseOfFunction(fn: string | null): StepPhase {
  return fn === null ? "other" : (PHASE_BY_FUNCTION[fn] ?? "other");
}

/// What to call the shared entries when nobody has named them — a default
/// that lives here rather than in anyone's config file. A `name =` someone did
/// set still wins, and the id stays visible beside the name in the grid.
const DEFAULT_NAMES: Record<string, string> = {
  "unified_index/grid_index": "Unified Index (table)",
  "unified_index/qmd_aggregator": "Unified Index (QMD)",
  "unified_index/embedding_map": "Unified Index (map)",
  unified_index: "Unified Index (Applet)",
};

/// The label to show for an entry that declares no `name`. Falls back
/// to the id, which is what an unnamed step has always shown.
export function defaultName(id: string): string {
  return DEFAULT_NAMES[id] ?? id;
}

type ParsedConfig = {
  ast: ReturnType<typeof parseTOML>;
  root: { groups?: unknown; steps?: unknown; applets?: unknown };
};

/// Parse the config text, throwing with the parser's message (and line,
/// when it has one) on malformed TOML.
function parseConfig(text: string): ParsedConfig {
  try {
    const ast = parseTOML(text);
    return { ast, root: getStaticTOMLValue(ast) as ParsedConfig["root"] };
  } catch (e) {
    const err = e as { message?: string; lineNumber?: number };
    const at = err.lineNumber !== undefined ? ` (line ${err.lineNumber})` : "";
    throw new Error(`${err.message ?? String(e)}${at}`);
  }
}

/// Every entry's character range in one `[[…]]` array. `[steps.params]`
/// is a sibling node in the AST rather than a child of the step's own
/// table, so the span has to be widened to cover it.
function ranges(ast: ParsedConfig["ast"], key: string): Map<number, [number, number]> {
  const out = new Map<number, [number, number]>();
  for (const node of ast.body[0].body) {
    if (node.type !== "TOMLTable") continue;
    const [k, index] = node.resolvedKey;
    if (k !== key || typeof index !== "number") continue;
    const prev = out.get(index);
    out.set(
      index,
      prev
        ? [Math.min(prev[0], node.range[0]), Math.max(prev[1], node.range[1])]
        : [node.range[0], node.range[1]],
    );
  }
  return out;
}

function groupsOf({ ast, root }: ParsedConfig): ConfiguredGroup[] {
  if (!Array.isArray(root.groups)) return [];
  const groupRanges = ranges(ast, "groups");
  return root.groups.map((raw, i) => {
    const g = raw as { id?: unknown; name?: unknown; type?: unknown; description?: unknown } | null;
    const [start, end] = groupRanges.get(i) ?? [0, 0];
    return {
      id: typeof g?.id === "string" ? g.id : "",
      name: nonBlank(g?.name),
      type: typeof g?.type === "string" ? g.type : null,
      description: nonBlank(g?.description),
      start,
      end,
    };
  });
}

/// A string with something in it, trimmed; anything else is "not set".
function nonBlank(v: unknown): string | null {
  return typeof v === "string" && v.trim() !== "" ? v.trim() : null;
}

/// The `[[groups]]` entries a config declares, in file order.
export function listGroups(text: string): ConfiguredGroup[] {
  return groupsOf(parseConfig(text));
}

/// Parse the config text and list every step and applet it declares,
/// one row per entry. Throws with the parser's message (and line, when
/// it has one) on malformed TOML.
export function listSteps(text: string): ConfiguredStep[] {
  const parsed = parseConfig(text);
  const { ast, root } = parsed;
  const groupsById = new Map(groupsOf(parsed).map((g) => [g.id, g]));

  const steps: ConfiguredStep[] = [];

  if (Array.isArray(root.steps)) {
    const stepRanges = ranges(ast, "steps");
    root.steps.forEach((raw, i) => {
      const step = raw as {
        id?: unknown;
        group?: unknown;
        function?: unknown;
        name?: unknown;
        command?: unknown;
        inputs?: unknown;
        params?: unknown;
      } | null;
      const group = typeof step?.group === "string" ? step.group : null;
      const fn = typeof step?.function === "string" ? step.function : null;
      // The id the loader composes — `<group>/<function>` — or the one
      // written. A half-declared step is malformed and the loader will
      // say so; it just needs to be addressable here.
      const id =
        group !== null && fn !== null
          ? `${group}/${fn}`
          : typeof step?.id === "string"
            ? step.id
            : "";
      const groupEntry = group !== null ? groupsById.get(group) : undefined;
      const phase = phaseOfFunction(fn);
      // Blank is the same as absent: the row falls back to the derived
      // label in both cases, so a whitespace name never blanks a row.
      const name =
        typeof step?.name === "string" && step.name.trim() !== "" ? step.name.trim() : null;
      const [start, end] = stepRanges.get(i) ?? [0, 0];
      steps.push({
        id: id || `step ${i + 1}`,
        kind: "step",
        group,
        function: fn,
        name:
          name ??
          (groupEntry
            ? groupedName(groupEntry, id, fn, phase)
            : id
              ? defaultName(id)
              : `step ${i + 1}`),
        phase,
        // The group's type is the only place a step's provider is
        // written; a step outside any group has none.
        type: groupEntry?.type ?? null,
        inputs: (Array.isArray(step?.inputs) ? (step!.inputs as unknown[]) : []).filter(
          (v): v is string => typeof v === "string",
        ),
        params:
          step?.params && typeof step.params === "object"
            ? (step.params as Record<string, unknown>)
            : {},
        start,
        end,
      });
    });
  }

  const applets: ConfiguredStep[] = [];
  if (Array.isArray(root.applets)) {
    const appletRanges = ranges(ast, "applets");
    root.applets.forEach((raw, i) => {
      const applet = raw as {
        id?: unknown;
        group?: unknown;
        command?: unknown;
      } | null;
      const id = typeof applet?.id === "string" ? applet.id : `applet ${i + 1}`;
      const [start, end] = appletRanges.get(i) ?? [0, 0];
      applets.push({
        id,
        kind: "applet",
        group: typeof applet?.group === "string" ? applet.group : null,
        function: null,
        // `AppletEntry` has no `name` key — an applet takes its display
        // label through its own `params` — so it is shown by its id,
        // or by the default label when it is one of the shared entries
        // the app ships (see `DEFAULT_NAMES`).
        name: defaultName(id),
        phase: "other",
        // The word after `datalib-applet`, when it is one — the same
        // shape as a step's provider word, and what names the applet.
        type: appletType(typeof applet?.command === "string" ? applet.command : ""),
        inputs: [],
        params: {},
        start,
        end,
      });
    });
  }

  // Config order for steps, then applets. Steps in the order written is
  // what a person editing the file expects to see; sibling fetch/render
  // pairs land adjacent because that is how they are written.
  return [...steps, ...applets];
}

/// `datalib-applet unified_index` → `unified_index`. Null for anything
/// else, which is legitimate — an applet may be any executable.
function appletType(command: string): string | null {
  const words = command.trim().split(/\s+/);
  if (words.length < 2) return null;
  if (!/(^|\/)datalib-applet$/.test(words[0])) return null;
  return words[1];
}

/// Read a dotted path (`api.channels`) out of a params tree.
export function getParam(params: Record<string, unknown>, target: string): unknown {
  let cur: unknown = params;
  for (const seg of target.split(".")) {
    if (cur === null || typeof cur !== "object") return undefined;
    cur = (cur as Record<string, unknown>)[seg];
  }
  return cur;
}

/// Every dotted leaf path in a params tree, so we can tell whether a
/// descriptor covers all of them.
function leafPaths(value: unknown, prefix = ""): string[] {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return prefix ? [prefix] : [];
  }
  const out: string[] = [];
  for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
    out.push(...leafPaths(v, prefix ? `${prefix}.${k}` : k));
  }
  return out;
}

/// Can the wizard round-trip this step without losing anything?
export function paramsAreRepresentable(
  step: ConfiguredStep,
  entry: CatalogEntry,
): { ok: true } | { ok: false; unknown: string[] } {
  const phase = fieldPhaseOf(step);
  // Presets count as known. They are values this descriptor *writes*,
  // just without a box to type them in, so a Gmail step's
  // `gmail.user_id` is modeled even though no field names it —
  // and without this, every source with a preset would be permanently
  // un-editable.
  const known = new Set([
    ...fieldsFor(entry, phase).map((f) => f.target),
    ...presetsFor(entry, phase).map((p) => p.target),
  ]);
  const unknown = leafPaths(step.params).filter((path) => !known.has(path) && !INERT.has(path));
  return unknown.length === 0 ? { ok: true } : { ok: false, unknown };
}

/// Keys a hand-written config may still carry that no longer do anything
/// (`datalib-dag --check` warns at each). Saving the form drops them,
/// which is what the warning asks for, so they do not block an edit.
const INERT = new Set(["common.always_clear_before_ingest"]);

/// A descriptor's presets for one phase. Same default as a field's:
/// absent means `download`.
export function presetsFor(entry: CatalogEntry, phase: FieldPhase): Preset[] {
  return (entry.preset ?? []).filter((p) => (p.phase ?? "download") === phase);
}

/// The step whose output this one reads — its producer: the first input
/// that names a step, else the ingest step filed under the same group,
/// which is what `datalib-step` itself falls back to when a render step
/// declares no inputs.
export function producerOf(
  step: ConfiguredStep,
  all: ConfiguredStep[],
): ConfiguredStep | undefined {
  for (const id of step.inputs) {
    const hit = all.find((s) => s.id === id);
    if (hit) return hit;
  }
  if (step.group === null) return undefined;
  return all.find((s) => s.group === step.group && s.phase === "ingest");
}

/// The two steps a source is made of, as the config has them.
export type SourceSteps = { ingest?: ConfiguredStep; render?: ConfiguredStep };

/// A group's ingest and render steps, by phase.
export function sourceStepsOf(groupId: string, all: ConfiguredStep[]): SourceSteps {
  const under = all.filter((s) => s.kind === "step" && s.group === groupId);
  return {
    ingest: under.find((s) => s.phase === "ingest"),
    render: under.find((s) => s.phase === "render"),
  };
}

/// The catalog entry describing a step, in the context of the config it
/// sits in.
export function entryForStep(
  step: ConfiguredStep,
  all: ConfiguredStep[],
): CatalogEntry | undefined {
  const params =
    step.phase === "render" ? (producerOf(step, all)?.params ?? step.params) : step.params;
  return catalogForStep(step.type, params);
}

/// Which half of a catalog entry's fields this step takes. A catalog
/// `Field` is tagged `download` or `render`; a step that is neither
/// (an index step, a custom executable) has no form, and `download` is
/// the harmless default that yields nothing to show.
export function fieldPhaseOf(step: ConfiguredStep): FieldPhase {
  return step.phase === "render" ? "render" : "download";
}

/// A descriptor's fields for one phase. `phase` is optional on a field
/// and defaults to `download`, which is where all but one sit — only
/// `signal` declares a render knob today, so a render step's form is
/// usually a name and nothing else.
///
/// Every source that signs in through latchkey gets an account field,
/// whether or not its descriptor declares one: picking a stored login,
/// or naming a new one, is the same for every service. A descriptor
/// declares its own only to word it (Gmail's is "Google account").
export function fieldsFor(entry: CatalogEntry, phase: FieldPhase): Field[] {
  const fields = (entry.fields ?? []).filter((f) => (f.phase ?? "download") === phase);
  const declaresAccount = fields.some((f) => f.kind === "text" && f.latchkey);
  if (phase !== "download" || !entry.credentialService || declaresAccount) return fields;
  return [accountFieldFor(entry), ...fields];
}

/// The account field a latchkey source gets when it declares none.
export function accountFieldFor(entry: CatalogEntry): Field {
  return {
    kind: "text",
    latchkey: true,
    target: "latchkey_settings.account",
    label: `${entry.label} account`,
    help: NAME_IT_HELP,
  };
}

export type FieldValues = Record<string, unknown>;

/// A `bytes` value is the string a person would write — "5 MB" — and
/// is written as one. A number reaching here (the catalog's default, a
/// config that says `5_000_000`) is put into that form first.
function bytesText(value: unknown): string {
  return typeof value === "number" ? formatBytes(value) : String(value ?? "");
}

/// The option a stored value corresponds to, or the value unchanged.
function matchOption(options: { value: string }[], value: unknown): unknown {
  if (typeof value !== "string") return value;
  const hit = options.find((o) => o.value === value.toLowerCase());
  return hit ? hit.value : value;
}

/// The form's starting values for one descriptor: what the config already
/// says, else the descriptor's default, else empty. A download field
/// reads the ingest step's params and a render field the render step's.
///
/// `steps` present means *editing*, absent means *creating*, and the
/// difference is load-bearing for numeric fields: an `int` or `bytes`
/// default is a policy this wizard imposes where the backend has none,
/// so applying it on edit would cap a deliberately-uncapped source.
/// `bool` and `select` defaults mirror the backend's own and seed
/// either way.
export function seedFieldValues(entry: CatalogEntry, steps?: SourceSteps): FieldValues {
  const next: FieldValues = {};
  for (const field of entry.fields ?? []) {
    const step = (field.phase ?? "download") === "render" ? steps?.render : steps?.ingest;
    const existing = step ? getParam(step.params, field.target) : undefined;
    if (existing !== undefined) {
      next[field.target] =
        field.kind === "string_list"
          ? ((existing as string[]) ?? [])
          : field.kind === "select"
            ? matchOption(field.options, existing)
            : field.kind === "bytes"
              ? bytesText(existing)
              : existing;
    } else if (field.kind === "int" && field.default !== undefined && !steps) {
      next[field.target] = field.default;
    } else if (field.kind === "bytes" && field.default !== undefined && !steps) {
      next[field.target] = formatBytes(field.default);
    } else if (field.kind === "bool") {
      next[field.target] = field.default ?? false;
    } else if (field.kind === "select") {
      next[field.target] = field.default;
    } else if (field.kind === "string_list") {
      next[field.target] = [];
    } else {
      next[field.target] = "";
    }
  }
  return next;
}

// Writing

/// Every `(dotted target, value)` pair this descriptor writes for one
/// phase, in the order they should appear: presets first (they are what
/// the step *is*), then the fields that are active and set.
function paramEntries(
  entry: CatalogEntry,
  values: FieldValues,
  phase: FieldPhase,
): { target: string; value: unknown; field?: Field }[] {
  const out: { target: string; value: unknown; field?: Field }[] = presetsFor(entry, phase).map(
    (p) => ({ target: p.target, value: p.value }),
  );
  for (const field of fieldsFor(entry, phase)) {
    if (!fieldIsActive(field, values)) continue;
    const value = values[field.target];
    if (!isSet(field, value)) continue;
    out.push({ target: field.target, value, field });
  }
  return out;
}

/// The same params as a nested object, for anything that has to *send*
/// a step's config rather than write it — today the wizard's "Test
/// connection", which POSTs it to `/api/probe`.
export function paramsObject(
  entry: CatalogEntry,
  values: FieldValues,
  phase: FieldPhase,
): Record<string, unknown> {
  const root: Record<string, unknown> = {};
  for (const { target, value, field } of paramEntries(entry, values, phase)) {
    const segs = target.split(".");
    let cur = root;
    for (const seg of segs.slice(0, -1)) {
      if (typeof cur[seg] !== "object" || cur[seg] === null) cur[seg] = {};
      cur = cur[seg] as Record<string, unknown>;
    }
    cur[segs[segs.length - 1]] = jsonValue(field, value);
  }
  // A mode-selecting table with no keys of its own still has to exist —
  // `gmail = {}` is how a config says "this is a Gmail source" — and so
  // does the params object the probe receives.
  for (const preset of presetsFor(entry, phase)) {
    const head = preset.target.split(".")[0];
    if (!(head in root)) root[head] = {};
  }
  if (phase === "download" && entry.method && !(entry.method in root)) root[entry.method] = {};
  return root;
}

/// A form value as JSON. Mirrors [`tomlValue`]'s coercions: an `int`
/// field holds the string an `<input type=number>` produced, and the
/// backend's `Option<usize>` will not take `"5000"`.
function jsonValue(field: Field | undefined, value: unknown): unknown {
  if (!field) return value;
  switch (field.kind) {
    case "bool":
      return !!value;
    case "int":
      return Number(value);
    case "bytes":
      return bytesText(value);
    case "string_list":
      return value as string[];
    default:
      return String(value);
  }
}

/// Render the download step's `[steps.params.…]` body from form values.
/// Emitted as sub-table headers, so it must come last within its step —
/// in TOML every key after a table header belongs to that table.
function paramsToml(entry: CatalogEntry, values: FieldValues, phase: FieldPhase): string {
  // Group by the table each target sits in (`api.channels` → `api`).
  const tables = new Map<string, string[]>();
  for (const { target, value, field } of paramEntries(entry, values, phase)) {
    const dot = target.lastIndexOf(".");
    const table = dot < 0 ? "" : target.slice(0, dot);
    const key = dot < 0 ? target : target.slice(dot + 1);
    const lines = tables.get(table) ?? [];
    lines.push(`${key} = ${tomlValue(field, value)}`);
    tables.set(table, lines);
  }
  // The method table has to exist even with none of its knobs set: its
  // *presence* is what names the ingest method (`api = {}` is a complete
  // selection), and `datalib-step` refuses a step naming none.
  if (
    phase === "download" &&
    entry.method &&
    ![...tables.keys()].some((t) => t === entry.method || t.startsWith(`${entry.method}.`))
  ) {
    tables.set("", [...(tables.get("") ?? []), `${entry.method} = {}`]);
  }
  if (tables.size === 0) return "";
  // Shallowest table first, so `[steps.params]` precedes
  // `[steps.params.common]`. TOML permits defining a super-table after
  // a sub-table, but a generated file people are meant to read and
  // hand-edit shouldn't make them work that out.
  return [...tables.entries()]
    .sort(([a], [b]) => a.split(".").length - b.split(".").length || a.localeCompare(b))
    .map(([table, lines]) => `[steps.params${table ? `.${table}` : ""}]\n${lines.join("\n")}`)
    .join("\n\n");
}

// Laying the form out

/// One heading of the form and what is drawn under it: a question's
/// answers, each with the fields it shows while chosen, or fields as
/// they are. `solo` is an advanced field no section names, whose own
/// label is the heading.
export type Row = {
  heading: string;
  help?: string;
  answers?: { answer: Answer; fields: Field[] }[];
  fields: Field[];
  solo?: boolean;
};

export type Layout = { basic: Row[]; advanced: Row[] };

/// The form for one source: its sections in order, split into the
/// basic part and Advanced options, with every field no section names
/// appended to the advanced part. The latchkey account of a source that
/// signs in is drawn with the sign-in, so it is left out here.
export function layoutOf(entry: CatalogEntry, renders: boolean): Layout {
  const fields = [
    ...fieldsFor(entry, "download").filter(
      (f) => !(entry.credentialService && f.kind === "text" && f.latchkey),
    ),
    ...(renders ? fieldsFor(entry, "render") : []),
  ];
  const at = (targets: string[] | undefined) =>
    (targets ?? []).flatMap((t) => fields.filter((f) => f.target === t));
  const placed = new Set<string>();
  const layout: Layout = { basic: [], advanced: [] };
  for (const section of entry.sections ?? []) {
    const row: Row = {
      heading: section.heading,
      help: section.help,
      answers: section.answers?.map((answer) => ({ answer, fields: at(answer.fields) })),
      fields: at(section.fields),
    };
    for (const f of row.fields) placed.add(f.target);
    for (const a of section.answers ?? []) {
      for (const t of [...(a.fields ?? []), ...Object.keys(a.sets ?? {})]) placed.add(t);
    }
    if (row.fields.length || row.answers) layout[section.advanced ? "advanced" : "basic"].push(row);
  }
  for (const f of fields) {
    if (!placed.has(f.target)) layout.advanced.push({ heading: f.label, fields: [f], solo: true });
  }
  return layout;
}

/// Which answer the values amount to. An answer that shows fields is
/// the one only while one of them holds something; otherwise the first
/// plain answer whose `sets` agree, and the first answer when none do.
export function chosenAnswer(
  answers: { answer: Answer; fields: Field[] }[],
  values: FieldValues,
): number {
  const agrees = (a: Answer) => Object.entries(a.sets ?? {}).every(([t, v]) => !!values[t] === v);
  const filled = answers.findIndex(
    ({ answer, fields }) =>
      fields.length > 0 && agrees(answer) && fields.some((f) => isSet(f, values[f.target])),
  );
  if (filled >= 0) return filled;
  const plain = answers.findIndex(({ answer, fields }) => fields.length === 0 && agrees(answer));
  return Math.max(plain, 0);
}

/// The values after choosing one answer: what it sets, and the fields
/// of every other answer emptied, so no setting outlives the answer
/// that showed it.
export function applyAnswer(
  answers: { answer: Answer; fields: Field[] }[],
  index: number,
  values: FieldValues,
): FieldValues {
  const next = { ...values };
  answers.forEach(({ fields }, i) => {
    if (i === index) return;
    for (const f of fields) next[f.target] = f.kind === "string_list" ? [] : "";
  });
  Object.assign(next, answers[index]?.answer.sets ?? {});
  return next;
}

/// Is every field of this answer still empty? A chosen answer that
/// shows fields is not answered until one of them is filled.
export function answerIsEmpty(fields: Field[], values: FieldValues): boolean {
  return fields.length > 0 && !fields.some((f) => isSet(f, values[f.target]));
}

/// Is this field's gate open? A field with no `requires` always is.
export function fieldIsActive(field: Field, values: FieldValues): boolean {
  return field.requires === undefined || !!values[field.requires];
}

function isSet(field: Field, value: unknown): boolean {
  if (value === undefined || value === null) return false;
  if (field.kind === "string_list") return Array.isArray(value) && value.length > 0;
  if (field.kind === "text" || field.kind === "date" || field.kind === "path") {
    return String(value).trim() !== "";
  }
  if (field.kind === "int") return value !== "" && Number.isFinite(Number(value));
  if (field.kind === "bytes") return parseByteSize(bytesText(value)) !== null;
  // A select normally holds one of its options, so it is always written. The
  // membership test is deliberately *not* here: a hand-edited config can hold
  // a value the dropdown doesn't know, and dropping it on save would silently
  // rewrite someone's config.
  if (field.kind === "select") return String(value).trim() !== "";
  // A boolean is always meaningful — false is a real setting, and for
  // `media` (which defaults true) omitting it would change behavior.
  return true;
}

/// A value as TOML. `field` is absent for a preset, whose value is a
/// literal in the catalog rather than something a form produced — so
/// its own JavaScript type is the right thing to read.
function tomlValue(field: Field | undefined, value: unknown): string {
  if (!field) {
    if (typeof value === "boolean") return value ? "true" : "false";
    if (typeof value === "number") return String(value);
    return quote(String(value));
  }
  switch (field.kind) {
    case "bool":
      return value ? "true" : "false";
    case "int":
      return String(Number(value));
    case "bytes":
      return quote(bytesText(value));
    case "string_list":
      return `[${(value as string[]).map(quote).join(", ")}]`;
    default:
      return quote(String(value));
  }
}

/// The function a step of this phase performs within its group, which
/// is also the directory it writes under the group's.
export function functionOf(phase: FieldPhase): string {
  return phase === "render" ? "render_markdown" : "ingest";
}

/// The id the loader composes for a group's step of this phase — the
/// one place this side of the app puts a group and a function together.
export function stepIdFor(group: string, phase: FieldPhase): string {
  return `${group}/${functionOf(phase)}`;
}

/// One source's `[[groups]]` block, with a divider above it. The name
/// is written only when there is one and it says more than the id; the
/// description only when there is one.
export function buildGroup(opts: {
  id: string;
  name: string;
  type: string;
  description?: string;
}): string {
  const { id, type } = opts;
  const divider = `# ── ${id} ${"─".repeat(Math.max(4, 66 - id.length))}`;
  const lines = [
    `id = ${quote(id)}`,
    nameLine(id, opts.name),
    `type = ${quote(type)}`,
    descriptionLine(opts.description ?? ""),
  ].filter((l): l is string => l !== null);
  return `${divider}\n[[groups]]\n${lines.join("\n")}`;
}

/// The `name = …` line for a group, or none: a blank name, or one that
/// only repeats the id, is not worth a line.
function nameLine(groupId: string, name: string): string | null {
  const next = name.trim();
  return next && next !== groupId ? `name = ${quote(next)}` : null;
}

/// The `description = …` line for a group, or none when it is blank.
function descriptionLine(description: string): string | null {
  const next = description.trim();
  return next ? `description = ${quote(next)}` : null;
}

/// One step, as a `[[steps]]` block. No name: a grouped step's label
/// comes from its group and its function. No command either: a built-in
/// step is `datalib-step`, which reads the function and the group's type
/// from the environment.
export function buildStep(opts: {
  entry: CatalogEntry;
  group: string;
  phase: FieldPhase;
  inputs?: string[];
  values: FieldValues;
}): string {
  const { entry, group, phase, values } = opts;
  return stepToml({
    group,
    phase,
    inputs: opts.inputs,
    params: paramsToml(entry, values, phase),
  });
}

/// The `[[steps]]` block itself: group, function, inputs, then a
/// params body already rendered as TOML. What `buildStep` writes once
/// it has turned form values into that body — the one place the shape of
/// a step is spelled out.
export function stepToml(opts: {
  group: string;
  phase: FieldPhase;
  inputs?: string[];
  params?: string;
}): string {
  const inputs = opts.inputs ?? [];
  const inputsLine = inputs.length ? `\ninputs = [${inputs.map(quote).join(", ")}]` : "";
  const params = opts.params ?? "";
  const block = `[[steps]]
group = ${quote(opts.group)}
function = ${quote(functionOf(opts.phase))}${inputsLine}${params ? `\n${params}` : ""}`;
  return block.trimEnd();
}

/// Everything the wizard writes for one source, in the order it goes
/// into the file: the group (when creating), the ingest step, and the
/// render step for a provider that renders. A provider that renders
/// nothing (`renderStep: false`) gets no render step and no
/// `renderId`.
export function buildSource(opts: {
  entry: CatalogEntry;
  group: string;
  name: string;
  description?: string;
  values: FieldValues;
  /// Write the `[[groups]]` block too. Off when editing: the group
  /// already exists and is renamed in place.
  withGroup: boolean;
  /// Whether this source wants its render step. Defaults to whatever
  /// the provider can do; the wizard passes the answer the person gave,
  /// which is the one that decides.
  renders?: boolean;
}): { groupBody: string | null; stepsBody: string; renderId: string | null } {
  const { entry, group, values } = opts;
  const ingestId = stepIdFor(group, "download");
  const ingest = buildStep({ entry, group, phase: "download", values });
  const renders = entry.renderStep !== false && opts.renders !== false;
  const render = renders
    ? buildStep({ entry, group, phase: "render", inputs: [ingestId], values })
    : null;
  return {
    groupBody: opts.withGroup
      ? buildGroup({
          id: group,
          name: opts.name,
          type: entry.type,
          description: opts.description,
        })
      : null,
    stepsBody: render ? `${ingest}\n\n${render}` : ingest,
    renderId: renders ? stepIdFor(group, "render") : null,
  };
}

/// The group `type` of a comparison between two commits of a source's
/// raw store — `docs/dev/plans/completed/diff_renderer.md`. Not a source type
/// the catalog offers: one is made from a source, by "Compare…".
export const DIFF_TYPE = "diff";

/// Everything "Compare…" writes for one diff group: the group, with the
/// source it compares, and its one render step, reading the source's
/// ingest tree with the two commits under `params.diff`. The step is
/// wired into the fan-ins like any render step (`renderId`).
export function buildDiffSource(opts: {
  id: string;
  name: string;
  source: string;
  from: string;
  to: string;
  maxDocuments: number;
}): { groupBody: string; stepsBody: string; renderId: string } {
  const { id, source } = opts;
  const divider = `# ── ${id} ${"─".repeat(Math.max(4, 66 - id.length))}`;
  const lines = [
    `id = ${quote(id)}`,
    nameLine(id, opts.name),
    `type = ${quote(DIFF_TYPE)}`,
    `source = ${quote(source)}`,
  ].filter((l): l is string => l !== null);
  const params = [
    "[steps.params.diff]",
    `from = ${quote(opts.from)}`,
    `to = ${quote(opts.to)}`,
    `max_documents = ${Math.max(1, Math.floor(opts.maxDocuments))}`,
  ].join("\n");
  return {
    groupBody: `${divider}\n[[groups]]\n${lines.join("\n")}`,
    stepsBody: stepToml({
      group: id,
      phase: "render",
      inputs: [stepIdFor(source, "download")],
      params,
    }),
    renderId: stepIdFor(id, "render"),
  };
}

/// Set, replace or (with an empty name) remove the `name` of one
/// `[[groups]]` entry, leaving everything else in the text alone.
export function renameGroup(text: string, groupId: string, name: string): string {
  return setGroupLine(text, groupId, "name", nameLine(groupId, name));
}

/// Set, replace or (with a blank description) remove the `description`
/// of one `[[groups]]` entry, leaving everything else in the text alone.
export function describeGroup(text: string, groupId: string, description: string): string {
  return setGroupLine(text, groupId, "description", descriptionLine(description));
}

/// Replace the `<key> = …` line of one `[[groups]]` entry with `line`,
/// remove it when `line` is null, or add it under the `id` line when the
/// entry has none. The rest of the text is untouched.
function setGroupLine(text: string, groupId: string, key: string, line: string | null): string {
  const group = listGroups(text).find((g) => g.id === groupId);
  if (!group || group.end === 0) return text;
  const body = text.slice(group.start, group.end);
  const keyRe = new RegExp(`^[ \\t]*${key}[ \\t]*=.*$`, "m");
  let edited: string;
  // Function replacers: the value is user text, and as a replacement
  // *string* `$1`, `$&` and `$$` in it would be expanded.
  if (keyRe.test(body)) {
    edited = body.replace(keyRe, () => line ?? "").replace(/\n\n(?=\S)/, "\n");
  } else if (line) {
    edited = body.replace(/^([ \t]*id[ \t]*=.*)$/m, (_m, idLine: string) => `${idLine}\n${line}`);
  } else {
    edited = body;
  }
  return text.slice(0, group.start) + edited + text.slice(group.end);
}

/// The fan-in steps, by function: the SQL index the grid reads, which
/// consumes rendered markdown; the qmd aggregator, which runs after each
/// source's own qmd steps; and the map, which reads the aggregator. A
/// source can be in one and not another.
export type FanInFunction = "grid_index" | "qmd_aggregator" | "embedding_map";

/// The fan-ins a render step feeds.
const RENDER_FAN_INS: FanInFunction[] = ["grid_index"];

/// One `[[steps]]` table and its body: up to the next line opening a
/// table — its own `[steps.params…]` sub-table, or the next entry.
/// Stopping on a *line* that starts with `[` rather than on any `[` is
/// what lets the body hold an array. Keys inside a table can be written
/// in any order, so the body is *tested* rather than pattern-matched.
const STEP_TABLE = /(\[\[steps\]\])([\s\S]*?)(?=\n[ \t]*\[|$)/g;

const INPUTS_OPEN = /^[ \t]*inputs[ \t]*=[ \t]*\[/m;

/// Is this step body a fan-in — filed under the `unified_index` group,
/// or, for a custom step, writing an `unified_index/…` id — and, when
/// `only` names some, one of those?
function isFanIn(body: string, only?: FanInFunction[]): boolean {
  const verbatim = /id\s*=\s*"unified_index\/([^"]*)"/.exec(body);
  if (!verbatim && !/group\s*=\s*"unified_index"/.test(body)) return false;
  if (!only) return true;
  const fn = /function\s*=\s*"([^"]*)"/.exec(body)?.[1] ?? verbatim?.[1];
  return only.includes(fn as FanInFunction);
}

/// Rewrite the `inputs` of the fan-ins `only` selects — all of them
/// when it is absent — leaving every other table, and every other key
/// in theirs, exactly as written, and the array laid out as it was.
function editFanInInputs(
  text: string,
  only: FanInFunction[] | undefined,
  edit: (ids: string[]) => string[],
): string {
  // A function replacer: an id is user text, and as a replacement
  // *string* `$1`, `$&` and `$$` in it would be expanded.
  return text.replace(STEP_TABLE, (whole, head: string, body: string) => {
    if (!isFanIn(body, only)) return whole;
    const found = INPUTS_OPEN.exec(body);
    if (!found) return whole;
    return `${head}${editStringArray(body, found.index + found[0].length - 1, edit)}`;
  });
}

/// Wire a step into fan-ins: a render step into the two that consume
/// rendered markdown, or into just the one `only` names — which is how
/// an embed step reaches the map.
///
/// The fan-ins name their inputs by id, so a source added without this renders
/// happily and is never indexed — invisible in search, with nothing on screen
/// to say why.
export function wireIntoFanIns(text: string, stepId: string, only?: FanInFunction): string {
  return editFanInInputs(text, only ? [only] : RENDER_FAN_INS, (ids) =>
    ids.includes(stepId) ? ids : [...ids, stepId],
  );
}

/// Drop a step from the fan-ins' inputs — every fan-in's, or the one
/// `only` names. The mirror of [`wireIntoFanIns`]: an input naming a
/// step that no longer exists costs the step that names it, so deleting
/// a source has to take its edges with it.
export function unwireFromFanIns(text: string, stepId: string, only?: FanInFunction): string {
  return editFanInInputs(text, only ? [only] : undefined, (ids) => ids.filter((t) => t !== stepId));
}

/// Which fan-in a step is, or null for a step that is not one. A
/// grouped step says so with `group` + `function`; a custom step filed
/// outside any group says it in the id it writes.
export function fanInFunctionOf(step: ConfiguredStep): string | null {
  if (step.group === "unified_index") return step.function;
  const [group, fn] = step.id.split("/");
  return step.group === null && group === "unified_index" ? (fn ?? null) : null;
}

/// A source's own qmd steps, as `[[steps]]` blocks: its keyword index,
/// which reads its render, and its embeddings, which read the keyword
/// index.
export function buildQmdSteps(group: string): { id: string; body: string }[] {
  const keywordId = `${group}/keyword_index`;
  const block = (fn: string, inputs: string[]) => ({
    id: `${group}/${fn}`,
    body: `[[steps]]\ngroup = ${quote(group)}\nfunction = ${quote(fn)}\ninputs = [${inputs.map(quote).join(", ")}]`,
  });
  return [block("keyword_index", [stepIdFor(group, "render")]), block("embed", [keywordId])];
}

/// How far into qmd a source's markdown goes: not at all, a keyword
/// index, or a keyword index and the embeddings that read it. There is
/// no embeddings-only: the embed step reads the keyword index.
export type QmdIndexing = "none" | "keyword" | "keyword_and_embed";

/// Which of a source's qmd steps the config has, as a `QmdIndexing`.
export function qmdIndexingOf(steps: ConfiguredStep[], group: string): QmdIndexing {
  const has = (fn: string) => steps.some((s) => s.id === `${group}/${fn}`);
  if (!has("keyword_index")) return "none";
  return has("embed") ? "keyword_and_embed" : "keyword";
}

/// Give a source the qmd steps `indexing` asks for — those it lacks — and
/// a place in the aggregator's inputs, and take away the ones it does not
/// ask for, with every step that reads them. Only where the config has the
/// aggregator: search is off without one, and the aggregator is what
/// retires a source's collection once it goes.
export function setQmdSteps(text: string, group: string, indexing: QmdIndexing): string {
  const [keyword, embed] = buildQmdSteps(group);
  const all = listSteps(text);
  const aggregated = all.some((s) => s.kind === "step" && fanInFunctionOf(s) === "qmd_aggregator");
  const wanted =
    !aggregated || indexing === "none" ? [] : indexing === "keyword" ? [keyword] : [keyword, embed];
  const unwanted = [keyword, embed].filter((b) => !wanted.includes(b)).map((b) => b.id);
  const gone = [...all.filter((s) => unwanted.includes(s.id)), ...readersOf(unwanted, all)];
  let next = gone.length ? removeSteps(text, gone) : text;
  const missing = wanted.filter((b) => !all.some((s) => s.id === b.id));
  if (missing.length) next = insertEntries(next, missing.map((b) => b.body).join("\n\n"));
  for (const b of [keyword, embed]) {
    next = wanted.includes(b)
      ? wireIntoFanIns(next, b.id, "qmd_aggregator")
      : unwireFromFanIns(next, b.id, "qmd_aggregator");
  }
  return next;
}

/// What has to leave the config with `ids`: every step that reads one of
/// them ([`readersOf`]), and, when the qmd aggregator is among them, every
/// source's own qmd steps — search is off without it, and nothing would
/// retire what they index.
export function removedWith(ids: string[], all: ConfiguredStep[]): ConfiguredStep[] {
  const aggregatorGoes = all.some(
    (s) => ids.includes(s.id) && s.kind === "step" && fanInFunctionOf(s) === "qmd_aggregator",
  );
  const qmdSteps = aggregatorGoes
    ? all.filter(
        (s) =>
          s.kind === "step" &&
          !ids.includes(s.id) &&
          (s.function === "keyword_index" || s.function === "embed"),
      )
    : [];
  const readers = readersOf([...ids, ...qmdSteps.map((s) => s.id)], all);
  return [...qmdSteps, ...readers];
}

/// Every step that reads one of `ids`, directly or through another, other
/// than a fan-in: a fan-in loses the input instead (`unwireFromFanIns`).
/// Removing a step without these leaves each naming an input that is
/// gone, which the loader drops it for.
export function readersOf(ids: string[], all: ConfiguredStep[]): ConfiguredStep[] {
  const gone = new Set(ids);
  const out: ConfiguredStep[] = [];
  for (let grew = true; grew;) {
    grew = false;
    for (const s of all) {
      if (s.kind !== "step" || gone.has(s.id) || fanInFunctionOf(s) !== null) continue;
      if (s.inputs.some((i) => gone.has(i))) {
        gone.add(s.id);
        out.push(s);
        grew = true;
      }
    }
  }
  return out;
}

/// The group every source feeds.
const INDEX_GROUP = "unified_index";

/// Add entries — a source, a comparison, a source's qmd steps — where the
/// file keeps reading in the order data flows: each step below the steps
/// it reads, a source's entries together. That is beside its own group's
/// entries when the group is already in the file, and otherwise after the
/// last source, just above the index every source feeds. A file already
/// out of that order may have no such place; then the entries go at the
/// end, which loads the same — the runner follows `inputs`, not the file.
export function insertEntries(text: string, body: string): string {
  const at = placeFor(text, body);
  return at === null ? `${text.replace(/\s*$/, "")}\n\n${body}\n` : splice(text, at, at, body);
}

/// Where [`insertEntries`] puts `body`, or null for the end. Always
/// between two entries, so what follows starts with a `[[…]]` header: a
/// splice anywhere else would reparent the keys after it to the table
/// spliced in.
function placeFor(text: string, body: string): number | null {
  const steps = listSteps(text).filter((s) => s.end > 0);
  const groups = listGroups(text).filter((g) => g.end > 0);
  const added = listSteps(body);
  const addedIds = new Set(added.map((s) => s.id));
  const reads = new Set(added.flatMap((s) => s.inputs).filter((id) => !addedIds.has(id)));
  const earliest = Math.max(0, ...steps.filter((s) => reads.has(s.id)).map((s) => s.end));
  const readers = [
    ...steps.filter((s) => s.group === INDEX_GROUP || fanInFunctionOf(s) !== null),
    ...steps.filter((s) => s.inputs.some((id) => addedIds.has(id))),
    ...groups.filter((g) => g.id === INDEX_GROUP),
  ];
  const latest = Math.min(text.length, ...readers.map((r) => extendOverComments(text, r.start)));
  const own = new Set(added.map((s) => s.group).filter((g) => g !== null));
  const beside = listGroups(body).length
    ? []
    : [
        ...steps.filter((s) => s.group !== null && own.has(s.group)),
        ...groups.filter((g) => own.has(g.id)),
      ];
  const at = beside.length ? Math.max(...beside.map((e) => e.end)) : latest;
  return earliest <= at && at <= latest ? at : null;
}

/// `text` with [start, end) replaced by `body`, a blank line either side.
function splice(text: string, start: number, end: number, body: string): string {
  const before = text.slice(0, start).replace(/\s*$/, "");
  const after = text.slice(end).replace(/^\s*/, "");
  return `${[before, body, after].filter(Boolean).join("\n\n").replace(/\s*$/, "")}\n`;
}

/// Each entry's span, its banner included (`extendOverComments`).
function spans(text: string, steps: Pick<ConfiguredStep, "start" | "end">[]): [number, number][] {
  return steps
    .filter((s) => s.end > 0)
    .map((s) => [extendOverComments(text, s.start), s.end] as [number, number])
    .sort((a, b) => a[0] - b[0]);
}

/// `text` without the spans. The last is cut first, so each cut's
/// offsets still hold when its turn comes.
function cut(text: string, ranges: [number, number][]): string {
  let out = text;
  for (const [start, end] of [...ranges].reverse()) {
    out = out.slice(0, start) + out.slice(end);
  }
  return out;
}

function tidy(text: string): string {
  return text.replace(/\n{3,}/g, "\n\n").replace(/^\s+/, "");
}

/// Remove entries — steps, applets or groups — from the config text.
export function removeSteps(text: string, steps: Pick<ConfiguredStep, "start" | "end">[]): string {
  return tidy(cut(text, spans(text, steps)));
}

/// Walk back from a step's start over blank lines and `#` comments, so
/// deleting a step takes its banner with it.
function extendOverComments(text: string, start: number): number {
  let at = start;
  for (;;) {
    const lineEnd = text.lastIndexOf("\n", at - 1);
    if (lineEnd < 0) break;
    const prevStart = text.lastIndexOf("\n", lineEnd - 1) + 1;
    const line = text.slice(prevStart, lineEnd).trim();
    if (line !== "" && !line.startsWith("#")) break;
    at = prevStart;
    if (prevStart === 0) break;
  }
  return at;
}

/// Move a group's `[[groups]]` entry and every step and applet filed
/// under it to just above `before`'s entries, or below the last group's
/// when `before` is null — which is how the Sources card's order
/// changes, since it lists groups in the order their `[[groups]]`
/// entries are written. The moved entries land together, in the order
/// they were written, each with its banner: the block the server's
/// sorter (`datalib_dag::config_order`) moves a group as. Nothing else
/// changes — the runner follows `inputs`, not the file — and a result
/// that would parse to different entries is refused (throws), as the
/// sorter refuses one.
export function moveGroup(text: string, groupId: string, before: string | null): string {
  const groups = listGroups(text).filter((g) => g.end > 0);
  const filed = listSteps(text).filter((s) => s.end > 0);
  const entriesOf = (id: string) => [
    ...groups.filter((g) => g.id === id),
    ...filed.filter((s) => s.group === id),
  ];
  const moving = spans(text, entriesOf(groupId));
  const others = groups.filter((g) => g.id !== groupId);
  if (moving.length === 0 || before === groupId) return text;
  let at: number;
  if (before === null) {
    const last = others.at(-1);
    if (!last) return text;
    at = Math.max(...spans(text, entriesOf(last.id)).map(([, end]) => end));
  } else {
    const target = spans(text, entriesOf(before));
    if (target.length === 0) return text;
    at = Math.min(...target.map(([start]) => start));
  }
  const body = moving.map(([start, end]) => text.slice(start, end).trim()).join("\n\n");
  const shift = moving
    .filter(([, end]) => end <= at)
    .reduce((sum, [start, end]) => sum + (end - start), 0);
  const placed = at - shift;
  const next = tidy(splice(cut(text, moving), placed, placed, body));
  if (declaredEntries(next) !== declaredEntries(text)) {
    throw new Error("Moving the group would change what the config says; it was left as it is.");
  }
  return next;
}

/// Every entry a config declares, order aside, as one comparable string.
function declaredEntries(text: string): string {
  const parsed = parseConfig(text).root;
  const sorted = (v: unknown) => (Array.isArray(v) ? v : []).map((e) => JSON.stringify(e)).sort();
  return JSON.stringify([parsed.groups, parsed.steps, parsed.applets].map(sorted));
}

/// The group a move would put on the wrong side of it, when it would:
/// above a group whose steps it reads, or below one that reads it. The
/// file reads in the order data flows (`datalib_dag::config_order`, which
/// `datalib-step topo-sort-config` restores), and a drag keeps it so.
export function moveAgainstDataFlow(
  text: string,
  groupId: string,
  before: string | null,
): { reads: string } | { readBy: string } | null {
  const steps = listSteps(text).filter((s) => s.kind === "step");
  const groupOfStep = new Map(steps.map((s) => [s.id, s.group]));
  const reads = (id: string) =>
    new Set(
      steps
        .filter((s) => s.group === id)
        .flatMap((s) => s.inputs.map((i) => groupOfStep.get(i)))
        .filter((g): g is string => !!g && g !== id),
    );
  const order = listGroups(text)
    .map((g) => g.id)
    .filter((id) => id !== groupId);
  const at = before === null ? order.length : order.indexOf(before);
  if (at < 0) return null;
  const mine = reads(groupId);
  const below = order.slice(at).find((id) => mine.has(id));
  if (below) return { reads: below };
  const above = order.slice(0, at).find((id) => reads(id).has(groupId));
  return above ? { readBy: above } : null;
}

/// Replace a source's steps with freshly generated ones, where the first
/// of them was, so an edit moves nothing. Every cut is made against the
/// text as parsed, the last first. Only safe when
/// `paramsAreRepresentable` said so — see this module's header.
export function replaceSteps(text: string, steps: ConfiguredStep[], body: string): string {
  const [first, ...rest] = spans(text, steps);
  if (!first) return insertEntries(text, body);
  return tidy(splice(cut(text, rest), first[0], first[1], body));
}

/// A human name reduced to something that can be a directory: NFKD
/// normalize, drop combining marks, lowercase, every run of
/// non-alphanumerics to a single `-`, trimmed, capped.
export function slugify(name: string): string {
  const ascii = name
    .normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
  return ascii.slice(0, 40).replace(/-+$/, "");
}

/// The reserved top-level directories, mirroring
/// `dag::config::RESERVED_STANZA_NAMES`. An id landing on one of these
/// is suffixed like any other collision rather than rejected, so the
/// wizard never proposes a name the loader would refuse.
const RESERVED_IDS = new Set(["system", "unified_index"]);

/// Propose an id for a new entry: `base` if it is free, else
/// `base-2`, `base-3`, …
export function suggestId(taken: Set<string>, base: string, fallback: string): string {
  const stem = base || fallback || "source";
  if (!taken.has(stem) && !RESERVED_IDS.has(stem)) return stem;
  for (let n = 2; n < 1000; n++) {
    const candidate = `${stem}-${n}`;
    if (!taken.has(candidate) && !RESERVED_IDS.has(candidate)) return candidate;
  }
  return stem;
}

/// Why the table is empty, when it shouldn't be.
///
/// The grid derives its rows from the config text in the browser, while
/// `GET /api/config` reports what the backend's loader made of the same file.
/// When those disagree the bug is on this side, and the empty state has to say
/// so rather than offering a friendly "nothing configured yet".
export function emptyTableDiagnosis(input: {
  /// Entries the browser parsed out of the config text.
  parsedCount: number;
  /// `source_count` from `GET /api/config` — the backend's own loader.
  serverSourceCount: number;
  /// Length of the config text the browser is holding.
  textLength: number;
  /// Whether the file exists on disk, per the backend.
  exists: boolean;
  path: string;
}): string | null {
  const { parsedCount, serverSourceCount, textLength, exists, path } = input;
  if (parsedCount > 0) return null;

  if (exists && textLength === 0) {
    return (
      `${path} exists but arrived empty, so there is nothing to show. ` +
      `That is not a config problem — the file did not reach this page.`
    );
  }
  if (serverSourceCount > 0) {
    return (
      `The server reads ${serverSourceCount} source${serverSourceCount === 1 ? "" : "s"} from ` +
      `${path}, but this table parsed none out of the ${textLength} characters it received. ` +
      `That disagreement is a bug in this table, not in your config — please report it.`
    );
  }
  return null;
}
