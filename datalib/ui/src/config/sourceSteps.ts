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
import { catalogForStep } from "./catalog";
import type { CatalogEntry, Field, FieldPhase, Preset } from "./catalog";

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
  /// [start, end) character offsets covering the `[[groups]]` table.
  start: number;
  end: number;
};

/// The label for a grouped step that wrote no `name` of its own.
function groupedName(group: ConfiguredGroup, id: string, phase: StepPhase): string {
  if (!group.name) return defaultName(id);
  if (phase === "ingest") return group.name;
  if (phase === "render") return `${group.name} (render markdown)`;
  return defaultName(id);
}

/// The built-in functions, each the directory it writes. Mirrors
/// `datalib_step::function::Function`; hand-kept in step with it. Any
/// other function is a custom executable's.
const PHASE_BY_FUNCTION: Record<string, StepPhase> = {
  ingest: "ingest",
  render_markdown: "render",
  grid_index: "index",
  qmd_index: "index",
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
  "unified_index/qmd_index": "Unified Index (QMD)",
  "unified_index": "Unified Index (Applet)",
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
/// table, so the span has to be widened to cover it — same derivation
/// as configSources.ts.
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
    const g = raw as { id?: unknown; name?: unknown; type?: unknown } | null;
    const [start, end] = groupRanges.get(i) ?? [0, 0];
    return {
      id: typeof g?.id === "string" ? g.id : "",
      name: typeof g?.name === "string" && g.name.trim() !== "" ? g.name.trim() : null,
      type: typeof g?.type === "string" ? g.type : null,
      start,
      end,
    };
  });
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
        typeof step?.name === "string" && step.name.trim() !== ""
          ? step.name.trim()
          : null;
      const [start, end] = stepRanges.get(i) ?? [0, 0];
      steps.push({
        id: id || `step ${i + 1}`,
        kind: "step",
        group,
        function: fn,
        name:
          name ??
          (groupEntry
            ? groupedName(groupEntry, id, phase)
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
  const unknown = leafPaths(step.params).filter((path) => !known.has(path));
  return unknown.length === 0 ? { ok: true } : { ok: false, unknown };
}

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
export function fieldsFor(entry: CatalogEntry, phase: FieldPhase): Field[] {
  return (entry.fields ?? []).filter((f) => (f.phase ?? "download") === phase);
}

export type FieldValues = Record<string, unknown>;

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
/// difference is load-bearing for `int` fields: an `int` default is a
/// policy this wizard imposes where the backend has none, so applying it
/// on edit would cap a deliberately-uncapped source. `bool` and `select`
/// defaults mirror the backend's own and seed either way.
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
            : existing;
    } else if (field.kind === "int" && field.default !== undefined && !steps) {
      next[field.target] = field.default;
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
    .map(([table, lines]) =>
      `[steps.params${table ? `.${table}` : ""}]\n${lines.join("\n")}`,
    )
    .join("\n\n");
}

/// Is this field's gate open? A field with no `requires` always is.
export function fieldIsActive(field: Field, values: FieldValues): boolean {
  return field.requires === undefined || !!values[field.requires];
}

function isSet(field: Field, value: unknown): boolean {
  if (value === undefined || value === null) return false;
  if (field.kind === "string_list") return Array.isArray(value) && value.length > 0;
  if (field.kind === "text" || field.kind === "date") return String(value).trim() !== "";
  if (field.kind === "int") return value !== "" && Number.isFinite(Number(value));
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
    case "string_list":
      return `[${(value as string[]).map(quote).join(", ")}]`;
    default:
      return quote(String(value));
  }
}

/// TOML basic string. Dates are quoted too: a bare `2026-01-01` parses
/// as a TOML date, and the providers validate a *string*.
function quote(s: string): string {
  const escaped = s
    .replace(/\\/g, "\\\\")
    .replace(/"/g, '\\"')
    .replace(/\n/g, "\\n")
    .replace(/\r/g, "\\r")
    .replace(/\t/g, "\\t")
    // Everything else TOML calls a control char, as \uXXXX.
    .replace(/[\u0000-\u001f\u007f]/g, (c) =>
      `\\u${c.charCodeAt(0).toString(16).padStart(4, "0")}`,
    );
  return `"${escaped}"`;
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
/// is written only when there is one and it says more than the id.
export function buildGroup(opts: { id: string; name: string; type: string }): string {
  const { id, type } = opts;
  const name = opts.name.trim();
  const divider = `# ── ${id} ${"─".repeat(Math.max(4, 66 - id.length))}`;
  const nameLine = name && name !== id ? `\nname = ${quote(name)}` : "";
  return `${divider}\n[[groups]]\nid = ${quote(id)}${nameLine}\ntype = ${quote(type)}`;
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
/// it has turned form values into that body, and what the Sources tab's
/// quick-add snippets write with a hand-written body — the one place
/// the shape of a step is spelled out.
export function stepToml(opts: {
  group: string;
  phase: FieldPhase;
  inputs?: string[];
  params?: string;
}): string {
  const inputs = opts.inputs ?? [];
  const inputsLine = inputs.length
    ? `\ninputs = [${inputs.map(quote).join(", ")}]`
    : "";
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
  values: FieldValues;
  /// Write the `[[groups]]` block too. Off when editing: the group
  /// already exists and is renamed in place.
  withGroup: boolean;
}): { groupBody: string | null; stepsBody: string; renderId: string | null } {
  const { entry, group, values } = opts;
  const ingestId = stepIdFor(group, "download");
  const ingest = buildStep({ entry, group, phase: "download", values });
  const renders = entry.renderStep !== false;
  const render = renders
    ? buildStep({ entry, group, phase: "render", inputs: [ingestId], values })
    : null;
  return {
    groupBody: opts.withGroup ? buildGroup({ id: group, name: opts.name, type: entry.type }) : null,
    stepsBody: render ? `${ingest}\n\n${render}` : ingest,
    renderId: renders ? stepIdFor(group, "render") : null,
  };
}

/// Set, replace or (with an empty name) remove the `name` of one
/// `[[groups]]` entry, leaving everything else in the text alone.
export function renameGroup(text: string, groupId: string, name: string): string {
  const group = listGroups(text).find((g) => g.id === groupId);
  if (!group || group.end === 0) return text;
  const body = text.slice(group.start, group.end);
  const next = name.trim();
  const line = next && next !== groupId ? `name = ${quote(next)}` : null;
  const nameRe = /^[ \t]*name[ \t]*=.*$/m;
  let edited: string;
  // Function replacers: a name is user text, and as a replacement
  // *string* `$1`, `$&` and `$$` in it would be expanded.
  if (nameRe.test(body)) {
    edited = body.replace(nameRe, () => line ?? "").replace(/\n\n(?=\S)/, "\n");
  } else if (line) {
    edited = body.replace(/^([ \t]*id[ \t]*=.*)$/m, (_m, idLine: string) => `${idLine}\n${line}`);
  } else {
    edited = body;
  }
  return text.slice(0, group.start) + edited + text.slice(group.end);
}

/// Wire a render step into every fan-in that consumes rendered markdown.
///
/// The fan-ins name their inputs by id, so a source added without this renders
/// happily and is never indexed — invisible in search, with nothing on screen
/// to say why.
/// A fan-in step's `inputs = [...]`, keyed on the step being filed
/// under the `unified_index` group (or, for a custom step, writing an
/// `unified_index/…` id), within its own table.
const FAN_IN_INPUTS =
  /((?:group\s*=\s*"unified_index"|id\s*=\s*"unified_index\/[^"]*")[^\[]*?inputs\s*=\s*\[)([^\]]*)(\])/g;

export function wireIntoFanIns(text: string, renderStepId: string): string {
  return text.replace(
    FAN_IN_INPUTS,
    (whole, head: string, body: string, tail: string) => {
      const ids = body
        .split(",")
        .map((t) => t.trim())
        .filter(Boolean);
      if (ids.includes(`"${renderStepId}"`)) return whole;
      ids.push(`"${renderStepId}"`);
      return `${head}${ids.join(", ")}${tail}`;
    },
  );
}

/// Drop a render step from every fan-in's inputs. The mirror of
/// [`wireIntoFanIns`]: an input naming a step that no longer exists is
/// a config the runner refuses outright, so deleting a source has to
/// take its edges with it.
export function unwireFromFanIns(text: string, renderStepId: string): string {
  return text.replace(
    FAN_IN_INPUTS,
    (_whole, head: string, body: string, tail: string) => {
      const ids = body
        .split(",")
        .map((t) => t.trim())
        .filter(Boolean)
        .filter((t) => t !== `"${renderStepId}"`);
      return `${head}${ids.join(", ")}${tail}`;
    },
  );
}

/// Append entries to the config text. Always at the end: the DAG
/// derives execution order from declared inputs rather than file order,
/// and in TOML the end is the only safe insertion point — every key
/// after a `[[…]]` header belongs to that table, so a mid-file splice
/// would reparent whatever followed.
export function appendSource(text: string, body: string): string {
  return `${text.replace(/\s*$/, "")}\n\n${body}\n`;
}

/// Remove entries — steps, applets or groups — from the config text.
export function removeSteps(
  text: string,
  steps: Pick<ConfiguredStep, "start" | "end">[],
): string {
  const cuts = steps
    .filter((s) => s.end > 0)
    .map((s) => [extendOverComments(text, s.start), s.end] as const)
    .sort((a, b) => b[0] - a[0]);
  let out = text;
  for (const [start, end] of cuts) {
    out = out.slice(0, start) + out.slice(end);
  }
  return out.replace(/\n{3,}/g, "\n\n").replace(/^\s+/, "");
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

/// Replace a source's steps with freshly generated ones. All the cuts
/// happen against the text as parsed, then one append: cutting and
/// appending one step at a time would leave the second step's offsets
/// pointing into text the first cut had already shifted. Only safe
/// when `paramsAreRepresentable` said so — see this module's header.
export function replaceSteps(text: string, steps: ConfiguredStep[], body: string): string {
  return appendSource(removeSteps(text, steps), body);
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
