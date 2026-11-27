// The config as the Manage screen reads and writes it: groups, the steps
// and applets filed under them, and the TOML the wizard produces.
//
// A `[[groups]]` entry is a source: an `id` that is the directory its
// steps write into, a `name` that is free text, and a `type`. A step
// writes its `id` as `<group>/<function>`: the part before the first `/`
// is its group, and the rest is what it does there (ingest, render,
// index). A step whose first part names no `[[groups]]` entry is a custom
// executable the wizard knows nothing about.
//
// The wizard writes a source as one unit — the group, its `ingest`
// step and its `render_markdown` step — from one form. Adding and
// deleting splice whole entries into the text the editor holds. Editing
// changes only the keys the form owns whose value changed, one key at a
// time (`tomlPatch.ts`), so whatever else a person wrote into the source
// — a key the form has no field for, `env`, a comment — is kept as it is.
//
// An applet is never scheduled and owns no artifacts, so most row
// actions don't apply to it — but it is configured, it can fail to
// start, and that should be visible here rather than as a 502 in
// another tab.

import { parseTOML, getStaticTOMLValue } from "toml-eslint-parser";

import { NAME_IT_HELP } from "./accountNaming";
import { formatBytes, parseByteSize } from "./byteSize";
import { expandInputs, namesStep, patternFunction } from "./inputPattern";
import { catalogForStep, packageLockSlots } from "./catalog";
import type { Answer, CatalogEntry, Field, FieldPhase, PackageOrigin, Preset } from "./catalog";
import type { PackageCommand } from "@/api";
import { editStringArray, quote } from "./tomlText";
import { deleteIn, setIn } from "./tomlPatch";

/// Which phase a step belongs to, for display and for picking the right
/// half of a catalog entry's fields. Read off the function part of its id.
export type StepPhase = "ingest" | "render" | "index" | "other";

export type EntryKind = "step" | "applet";

export type ConfiguredStep = {
  /// Identity, and the tree this step writes: its `id`, which a step
  /// the wizard writes spells `<group>/<function>`. Path-safe, unique, and what the directory structure is formed
  /// from — so changing it moves data on disk and strands the paths the
  /// index recorded, which is why the wizard holds it fixed after
  /// creation.
  id: string;
  kind: EntryKind;
  /// The step's group, the first part of its id, and what it does
  /// there, the rest. `function` is null for an id with no `/`; both are
  /// null for an applet, whose `group` is its own key.
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
  /// The `[[packages]]` entry whose program it runs, if any: the one
  /// whose directory its command's program is in (`packageOfCommand`).
  package: string | null;
  /// The `command` it declares; null when it declares none, which the
  /// loader refuses.
  command: string | null;
  /// The ids this step reads: its `inputs`, each pattern expanded.
  inputs: string[];
  /// The patterns among its `inputs` as written (`*/embed`), which also
  /// name a step not in the file yet.
  patterns?: string[];
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
  if (fn === "render_time_series") return `${group.name} (render time series)`;
  if (fn === "render_occurrences") return `${group.name} (render occurrences)`;
  if (phase === "render") return `${group.name} (render markdown)`;
  if (fn === "embed") return `${group.name} (Semantic indexing)`;
  return defaultName(id);
}

/// The built-in functions, each the directory it writes. Mirrors
/// `datalib_step::function::Function`; hand-kept in step with it. Any
/// other function is a custom executable's.
const PHASE_BY_FUNCTION: Record<string, StepPhase> = {
  ingest: "ingest",
  render_markdown: "render",
  render_time_series: "render",
  render_occurrences: "render",
  unified_grid_and_search_index: "index",
  time_series_index: "index",
  embed: "index",
  embedding_map: "index",
};

/// A step's phase, from its function. A step whose id has no function
/// part is a custom executable: `other`.
function phaseOfFunction(fn: string | null): StepPhase {
  return fn === null ? "other" : (PHASE_BY_FUNCTION[fn] ?? "other");
}

/// What to call the shared entries when nobody has named them — a default
/// that lives here rather than in anyone's config file. A `name =` someone did
/// set still wins, and the id stays visible beside the name in the grid.
const DEFAULT_NAMES: Record<string, string> = {
  "unified_grid_and_search_index/unified_grid_and_search_index": "Grid and Search Index (table)",
  "unified_grid_and_search_index/embedding_map": "Grid and Search Index (map)",
  unified_grid_and_search_index: "Grid and Search Index (Applet)",
  "unified_time_series_index/time_series_index": "Time Series Index",
  unified_time_series_index: "Time Series Index (Applet)",
  datalib_contacts: "Contacts (Applet)",
};

/// The label to show for an entry that declares no `name`. Falls back
/// to the id, which is what an unnamed step has always shown.
export function defaultName(id: string): string {
  return DEFAULT_NAMES[id] ?? id;
}

type ParsedConfig = {
  ast: ReturnType<typeof parseTOML>;
  root: {
    groups?: unknown;
    steps?: unknown;
    applets?: unknown;
    packages?: unknown;
    programs?: unknown;
    locks?: unknown;
  };
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

/// A `[[packages]]` entry and where it is in the text.
export type ConfiguredPackage = {
  id: string;
  source: string | null;
  url: string;
  sha256: string;
  start: number;
  end: number;
};

/// The program every built-in step runs, told what to do by its
/// arguments (`datalibStepCommand`).
export const DATALIB_STEP = "datalib-step";

/// The verbs `datalib-step` takes, as its `[[programs]]` entry lists them.
export const DATALIB_STEP_VERBS = ["reset", "prepare", "compact", "collect", "check"];

/// The first word of a command, split shell-style as the runner splits
/// it: the program it starts, which a `[[programs]]` entry names.
export function programOf(command: string): string {
  let word = "";
  let quote: string | null = null;
  for (let i = 0; i < command.length; i++) {
    const c = command[i]!;
    if (quote === "'") {
      if (c === "'") quote = null;
      else word += c;
    } else if (quote === '"') {
      if (c === '"') quote = null;
      else if (c === "\\" && i + 1 < command.length) word += command[++i];
      else word += c;
    } else if (c === "'" || c === '"') {
      quote = c;
    } else if (c === "\\" && i + 1 < command.length) {
      word += command[++i];
    } else if (/\s/.test(c)) {
      if (word) break;
    } else {
      word += c;
    }
  }
  return word;
}

/// Whether a step's `command` runs `datalib-step`.
export function isDatalibStep(command: string | null | undefined): boolean {
  const program = programOf(command ?? "");
  return program === DATALIB_STEP || program.endsWith(`/${DATALIB_STEP}`);
}

/// The command a built-in step runs: `datalib-step run <function>`, then
/// its group's `type` when the group has one, then, under a comparison,
/// the type of the source it compares.
export function datalibStepCommand(
  fn: string,
  type: string | null | undefined,
  sourceType?: string | null,
): string {
  return [DATALIB_STEP, "run", fn, type, sourceType ? `--source-type ${sourceType}` : null]
    .filter((w) => w)
    .join(" ");
}

/// Where the package pinned at `sha256` is unpacked, relative to the data
/// root. Mirrors `datalib_runtime::layout::package_dir_rel`.
export function packageDir(sha256: string): string {
  return `system/packages/${sha256.slice(0, 16)}`;
}

/// A package's command, as a step or applet runs it from the data root:
/// its program, a path inside the package, joined to the package's
/// directory.
export function inPackage(dir: string, command: string): string {
  return `${dir}/${command.trimStart()}`;
}

/// The `[[packages]]` entry whose directory `command`'s program is in.
export function packageOfCommand(
  command: string | null,
  packages: ConfiguredPackage[],
): string | null {
  const program = programOf(command ?? "");
  return packages.find((p) => program.startsWith(`${packageDir(p.sha256)}/`))?.id ?? null;
}

/// The `[[packages]]` entries a config declares, in file order.
export function listPackages(text: string): ConfiguredPackage[] {
  return packagesOf(parseConfig(text));
}

function packagesOf({ ast, root }: ParsedConfig): ConfiguredPackage[] {
  const raw = root.packages;
  if (!Array.isArray(raw)) return [];
  const at = ranges(ast, "packages");
  return raw.map((p, i) => {
    const e = (p ?? {}) as Record<string, unknown>;
    const str = (v: unknown) => (typeof v === "string" ? v : "");
    const [start, end] = at.get(i) ?? [0, 0];
    return {
      id: str(e.id),
      source: typeof e.source === "string" ? e.source : null,
      url: str(e.url),
      sha256: str(e.sha256),
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
  const packages = packagesOf(parsed);

  const steps: ConfiguredStep[] = [];

  if (Array.isArray(root.steps)) {
    const stepRanges = ranges(ast, "steps");
    root.steps.forEach((raw, i) => {
      const step = raw as {
        id?: unknown;
        name?: unknown;
        command?: unknown;
        inputs?: unknown;
        params?: unknown;
      } | null;
      // A step with no id is malformed and the loader will say so; it
      // just needs to be addressable here.
      const id = typeof step?.id === "string" ? step.id : "";
      const { group, fn } = splitStepId(id);
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
        // written; a step whose group has no entry has none.
        type: groupEntry?.type ?? null,
        package: packageOfCommand(
          typeof step?.command === "string" ? step.command : null,
          packages,
        ),
        command: typeof step?.command === "string" ? step.command : null,
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

  // A pattern (`*/render_markdown`) is read as the ids it names, as the
  // loader reads it; the text keeps the pattern.
  const ids = steps.map((s) => s.id);
  for (const s of steps) {
    s.patterns = s.inputs.filter((i) => patternFunction(i) !== null);
    s.inputs = expandInputs(s.id, s.inputs, ids);
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
        package: packageOfCommand(
          typeof applet?.command === "string" ? applet.command : null,
          packages,
        ),
        command: typeof applet?.command === "string" ? applet.command : null,
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

/// A step id's group, the part before the first `/`, and its function,
/// the rest: null for an id with no `/`.
export function splitStepId(id: string): { group: string | null; fn: string | null } {
  if (id === "") return { group: null, fn: null };
  const slash = id.indexOf("/");
  return slash < 0
    ? { group: id, fn: null }
    : { group: id.slice(0, slash), fn: id.slice(slash + 1) };
}

/// `datalib-applet unified_grid_and_search_index` → `unified_grid_and_search_index`. Null for anything
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

/// Does the form have a field for every param this step holds? Saving an
/// edit keeps the ones it has none for (`patchSource`); the form names
/// them (`untouchedSettings`).
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
/// (`datalib-pipeline check` warns at each). Saving the form drops them,
/// which is what the warning asks for.
const INERT = new Set([
  "backup.period",
  "common.always_clear_before_ingest",
  "export.fetch_photos",
  "period",
]);

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

/// A group's ingest and render steps, by phase. A source may have more
/// than one render (`render_time_series`, `render_occurrences`); the
/// one the wizard edits is its markdown render.
export function sourceStepsOf(groupId: string, all: ConfiguredStep[]): SourceSteps {
  const under = all.filter((s) => s.kind === "step" && s.group === groupId);
  return {
    ingest: under.find((s) => s.phase === "ingest"),
    render:
      under.find((s) => s.function === "render_markdown") ??
      under.find((s) => s.phase === "render"),
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
  // `fieldsFor`, not `entry.fields`: the account field a latchkey source
  // gets without declaring one has to be read back too.
  for (const field of [...fieldsFor(entry, "download"), ...fieldsFor(entry, "render")]) {
    const step = (field.phase ?? "download") === "render" ? steps?.render : steps?.ingest;
    const existing = step ? getParam(step.params, field.target) : undefined;
    if (existing !== undefined) {
      next[field.target] = formValue(field, existing);
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

/// A value from the config as the form holds it.
function formValue(field: Field, existing: unknown): unknown {
  switch (field.kind) {
    case "string_list":
      return Array.isArray(existing) ? existing : [];
    case "select":
      return matchOption(field.options, existing);
    case "bytes":
      return bytesText(existing);
    default:
      return existing;
  }
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
    if (isSet(field, value)) {
      out.push({ target: field.target, value, field });
    } else if (field.kind === "text" && field.envVar) {
      // The runner hands a step only the variables its params name
      // (docs/dev/step_protocol.md), so the default is written out.
      out.push({ target: field.target, value: field.envVar.default, field });
    }
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

/// The id the wizard writes for a group's step of this phase.
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
/// comes from its group and its function. A built-in step runs
/// `datalib-step`; a package source's step runs the package's program.
export function buildStep(opts: {
  entry: CatalogEntry;
  group: string;
  phase: FieldPhase;
  inputs?: string[];
  values: FieldValues;
}): string {
  const { entry, group, phase, values } = opts;
  const command = entry.package?.commands[functionOf(phase)];
  return stepToml({
    group,
    fn: functionOf(phase),
    type: entry.type,
    inputs: opts.inputs,
    command: entry.package && command ? inPackage(entry.package.dir, command.command) : undefined,
    scheduling: command,
    params: paramsToml(entry, values, phase),
  });
}

/// The functions whose `datalib-step` reads its inputs' files as they
/// are, so no producer of an input may run beside it.
const READS_FILES = new Set(["embed", "embedding_map"]);

/// The lock a `datalib-step` function holds: every embed loads the model
/// and writes the search index's vectors, so one runs at a time.
const LOCK_OF_FUNCTION: Record<string, string> = {
  embed: "embedder",
};

/// The locks a step the wizard writes may hold, and the slots each
/// `[[locks]]` entry declares.
export const LOCK_SLOTS: Record<string, number> = { embedder: 1, index: 2 };

/// `reads` and `locks` for a step. A package's step takes what its
/// manifest's command says, and `reads = "pinned"` once it has inputs
/// where the manifest says nothing. A `datalib-step` step follows the
/// rules `datalib-http`'s config upgrade writes (docs/dev/config_model.md
/// §"The retired shapes"): one other than `ingest` gets both whether or
/// not its inputs are empty yet (a fan-in's are, until a source is
/// added); any other step only once it has inputs.
function schedulingLines(opts: {
  fn: string;
  type: string | null;
  inputs: string[] | undefined;
  builtin: boolean;
  scheduling?: PackageCommand;
}): string[] {
  const { fn, type, inputs, builtin, scheduling } = opts;
  if (scheduling) {
    const reads = scheduling.reads ?? ((inputs ?? []).length > 0 ? "pinned" : null);
    const locks = scheduling.locks ?? [];
    return [
      ...(reads ? [`reads = ${quote(reads)}`] : []),
      ...(locks.length ? [`locks = [${locks.map(quote).join(", ")}]`] : []),
    ];
  }
  const reads = builtin ? fn !== "ingest" : (inputs ?? []).length > 0;
  if (!reads) return [];
  const readsFiles =
    builtin && (READS_FILES.has(fn) || (fn === "render_markdown" && type === "perseus"));
  const lock = builtin ? LOCK_OF_FUNCTION[fn] : undefined;
  const locks = lock ? [lock] : type ? [] : ["index"];
  return [
    `reads = ${quote(readsFiles ? "files" : "pinned")}`,
    ...(locks.length ? [`locks = [${locks.map(quote).join(", ")}]`] : []),
  ];
}

/// The `[[steps]]` block itself: id, command, inputs, how it reads and
/// what it locks, then a params body already rendered as TOML. What
/// `buildStep` writes once it has turned form values into that body — the
/// one place the shape of a step is spelled out.
export function stepToml(opts: {
  group: string;
  fn: string;
  /// The group's `type`; null for a group with none.
  type: string | null;
  /// The compared source's type, for a step under a comparison.
  sourceType?: string | null;
  /// The steps it reads; written, even empty, when present.
  inputs?: string[];
  /// What it runs; `datalib-step run …` when absent.
  command?: string;
  /// A package step's command as its manifest has it, whose `reads` and
  /// `locks` the step takes.
  scheduling?: PackageCommand;
  params?: string;
}): string {
  const { fn, type, inputs } = opts;
  const command = opts.command ?? datalibStepCommand(fn, type, opts.sourceType);
  const lines = [
    "[[steps]]",
    `id = ${quote(`${opts.group}/${fn}`)}`,
    `command = ${quote(command)}`,
    ...(inputs !== undefined ? [`inputs = [${inputs.map(quote).join(", ")}]`] : []),
    ...schedulingLines({
      fn,
      type,
      inputs,
      builtin: isDatalibStep(command),
      scheduling: opts.scheduling,
    }),
  ];
  const params = opts.params ?? "";
  return `${lines.join("\n")}${params ? `\n${params}` : ""}`.trimEnd();
}

/// Whether a source of `entry` gets its render step, given what the
/// person answered (`wanted`). A download-only provider always does; a
/// package source whose package has no `render_markdown` program cannot.
export function writesRenderStep(entry: CatalogEntry, wanted?: boolean): boolean {
  if (entry.package && !entry.package.commands.render_markdown) return false;
  return entry.rendersDocuments === false || wanted !== false;
}

/// Everything the wizard writes for one source, in the order it goes
/// into the file: the group (when creating), the ingest step, and the
/// render step unless the source turned rendering off. A download-only
/// provider (`rendersDocuments: false`) always gets its render step.
export function buildSource(opts: {
  entry: CatalogEntry;
  group: string;
  name: string;
  description?: string;
  values: FieldValues;
  /// Write the `[[groups]]` block too. Off when editing: the group
  /// already exists and is renamed in place.
  withGroup: boolean;
  /// Whether this source wants its render step: yes by default, and
  /// the wizard passes the answer the person gave. Ignored for a
  /// download-only provider.
  renders?: boolean;
}): { groupBody: string | null; stepsBody: string; renderId: string | null } {
  const { entry, group, values } = opts;
  const ingestId = stepIdFor(group, "download");
  const ingest = buildStep({ entry, group, phase: "download", values });
  const renders = writesRenderStep(entry, opts.renders);
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
/// raw store — `docs/dev/config_model.md`. Not a source type
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
  /// The compared source's group `type`.
  sourceType: string;
  from: string;
  to: string;
  maxDocuments: number;
}): { groupBody: string; stepsBody: string; renderId: string } {
  const { id, source } = opts;
  const divider = `# ── ${id} ${"─".repeat(Math.max(4, 66 - id.length))}`;
  const lines = [`id = ${quote(id)}`, nameLine(id, opts.name), `type = ${quote(DIFF_TYPE)}`].filter(
    (l): l is string => l !== null,
  );
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
      fn: functionOf("render"),
      type: DIFF_TYPE,
      sourceType: opts.sourceType,
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
/// consumes rendered markdown; the map, which reads every source's
/// `embed`; and the time-series index, which reads each source's time
/// series. A source can be in one and not another.
export type FanInFunction = "unified_grid_and_search_index" | "embedding_map" | "time_series_index";

/// The groups the fan-ins are filed under: the document index and the
/// time-series index.
const FAN_IN_GROUPS = ["unified_grid_and_search_index", "unified_time_series_index"];

/// The fan-ins a render step feeds.
const RENDER_FAN_INS: FanInFunction[] = ["unified_grid_and_search_index"];

/// One `[[steps]]` table and its body: up to the next line opening a
/// table — its own `[steps.params…]` sub-table, or the next entry.
/// Stopping on a *line* that starts with `[` rather than on any `[` is
/// what lets the body hold an array. Keys inside a table can be written
/// in any order, so the body is *tested* rather than pattern-matched.
const STEP_TABLE = /(\[\[steps\]\])([\s\S]*?)(?=\n[ \t]*\[|$)/g;

const INPUTS_OPEN = /^[ \t]*inputs[ \t]*=[ \t]*\[/m;

/// Is this step body a fan-in — its id under one of `FAN_IN_GROUPS` —
/// and, when `only` names some, one of those?
function isFanIn(body: string, only?: FanInFunction[]): boolean {
  const id = /^[ \t]*id[ \t]*=[ \t]*"([^"]*)"/m.exec(body)?.[1];
  const { group, fn } = splitStepId(id ?? "");
  if (group === null || !FAN_IN_GROUPS.includes(group)) return false;
  return !only || only.includes(fn as FanInFunction);
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
/// A fan-in that reads by pattern (`*/render_markdown`) already names the
/// step and is left as it is. One that names its inputs by id needs this,
/// or the source renders happily and is never indexed — invisible in
/// search, with nothing on screen to say why.
export function wireIntoFanIns(text: string, stepId: string, only?: FanInFunction): string {
  return editFanInInputs(text, only ? [only] : RENDER_FAN_INS, (ids) =>
    ids.some((i) => namesStep(i, stepId)) ? ids : [...ids, stepId],
  );
}

/// Drop a step from the fan-ins' inputs — every fan-in's, or the one
/// `only` names. The mirror of [`wireIntoFanIns`]: an input naming a
/// step that no longer exists costs the step that names it, so deleting
/// a source has to take its edges with it.
export function unwireFromFanIns(text: string, stepId: string, only?: FanInFunction): string {
  return editFanInInputs(text, only ? [only] : undefined, (ids) => ids.filter((t) => t !== stepId));
}

/// Which fan-in a step is, or null for a step that is not one: the
/// function of a step whose group is one of `FAN_IN_GROUPS`.
export function fanInFunctionOf(step: ConfiguredStep): string | null {
  return step.kind === "step" && step.group !== null && FAN_IN_GROUPS.includes(step.group)
    ? step.function
    : null;
}

/// The source types whose data is also time series. Mirrors
/// `SourceType::renders_time_series` in datalib-step; hand-kept in step with it.
export const TIME_SERIES_TYPES = ["garmin", "yolink", "airvisual"];

const TIME_SERIES_GROUP = "unified_time_series_index";

/// Whether a source of `type` may have an `embed` step. A time-series
/// source's markdown is daily summaries of numbers: the keyword index
/// finds everything in it, and embedding it costs minutes and gigabytes
/// for nothing. Mirrors `datalib_migrate_config`'s
/// `upgrade_time_series_embeds`, which takes the step out of a config
/// that has one.
export function embedAllowed(type: string | null | undefined): boolean {
  return !type || !TIME_SERIES_TYPES.includes(type);
}

/// Give a source of a time-series type its `render_time_series` step and
/// a place in the time-series index's inputs, adding the index and the
/// applet that serves it when the config has none. A package source is
/// one when its package has a `render_time_series` program, which its
/// step then runs. A source of any other type is left as it is.
export function setTimeSeriesStep(
  text: string,
  group: string,
  type: string,
  pkg?: PackageOrigin,
): string {
  const command = pkg?.commands.render_time_series;
  if (pkg ? !command : !TIME_SERIES_TYPES.includes(type)) return text;
  const id = `${group}/render_time_series`;
  let next = text;
  if (!listSteps(next).some((s) => s.id === id)) {
    next = insertEntries(
      next,
      stepToml({
        group,
        fn: "render_time_series",
        type,
        command: pkg && command ? inPackage(pkg.dir, command.command) : undefined,
        scheduling: command,
        inputs: [stepIdFor(group, "download")],
      }),
    );
  }
  if (!listSteps(next).some((s) => fanInFunctionOf(s) === "time_series_index")) {
    const index = stepToml({
      group: TIME_SERIES_GROUP,
      fn: "time_series_index",
      type: null,
      inputs: ["*/render_time_series"],
    });
    next = withDeclarations(
      `${next.replace(/\s*$/, "")}\n\n[[groups]]\nid = ${quote(TIME_SERIES_GROUP)}\n` +
        `name = "Time Series Index"\n\n${index}\n\n[[applets]]\n` +
        `group = ${quote(TIME_SERIES_GROUP)}\nid = ${quote(TIME_SERIES_GROUP)}\n` +
        `command = ${quote(`datalib-applet ${TIME_SERIES_GROUP}`)}\n`,
    );
  }
  return wireIntoFanIns(next, id, "time_series_index");
}

/// The source types whose events have occurrences. Mirrors
/// `datalib_step::source_type::SourceType::renders_occurrences`;
/// hand-kept in step with it.
export const OCCURRENCE_TYPES = ["calendar"];

/// Give a source of a type with occurrences its `render_occurrences`
/// step, which lists every occurrence in a window of days. Its rows are
/// read by the person or the agent who asks; no index reads it.
export function setOccurrencesStep(text: string, group: string, type: string): string {
  if (!OCCURRENCE_TYPES.includes(type)) return text;
  const id = `${group}/render_occurrences`;
  if (listSteps(text).some((s) => s.id === id)) return text;
  return insertEntries(
    text,
    stepToml({
      group,
      fn: "render_occurrences",
      type,
      inputs: [stepIdFor(group, "download")],
    }),
  );
}

/// The step that puts every source it reads in the keyword search.
const UNIFIED_GRID_AND_SEARCH_INDEX_STEP =
  "unified_grid_and_search_index/unified_grid_and_search_index";

/// A source's own search step, as a `[[steps]]` block: its embeddings,
/// which read the `unified_grid_and_search_index` step, whose keyword pass
/// cuts what they embed. `type` is the group's, and `sourceType` the
/// compared source's for a comparison.
export function buildEmbedStep(
  group: string,
  type: string | null,
  sourceType?: string | null,
): { id: string; body: string } {
  return {
    id: `${group}/embed`,
    body: stepToml({
      group,
      fn: "embed",
      type,
      sourceType,
      inputs: [UNIFIED_GRID_AND_SEARCH_INDEX_STEP],
    }),
  };
}

/// The compared source's type a comparison's steps run with, read off
/// the `--source-type` of a step already under `group`.
function sourceTypeIn(steps: ConfiguredStep[], group: string): string | null {
  for (const s of steps) {
    if (s.kind !== "step" || s.group !== group) continue;
    const m = /(?:^|\s)--source-type\s+(\S+)/.exec(s.command ?? "");
    if (m) return m[1]!;
  }
  return null;
}

/// How far into search a source goes: not at all, by its words, or by its
/// words and its meaning. There is no meaning alone: the embeddings are
/// of what the keyword pass cut.
export type SearchIndexing = "none" | "keyword" | "keyword_and_embed";

/// The sources the `unified_grid_and_search_index` step keeps out of search
/// (`params.not_searched`).
function notSearched(steps: ConfiguredStep[]): string[] {
  const listed = steps.find((s) => s.kind === "step" && s.id === UNIFIED_GRID_AND_SEARCH_INDEX_STEP)
    ?.params?.not_searched;
  return Array.isArray(listed) ? listed.filter((g): g is string => typeof g === "string") : [];
}

/// How far into search the config puts a source, as a `SearchIndexing`.
export function searchIndexingOf(steps: ConfiguredStep[], group: string): SearchIndexing {
  const hasIndexStep = steps.some(
    (s) => s.kind === "step" && s.id === UNIFIED_GRID_AND_SEARCH_INDEX_STEP,
  );
  if (!hasIndexStep || notSearched(steps).includes(group)) return "none";
  return steps.some((s) => s.id === `${group}/embed`) ? "keyword_and_embed" : "keyword";
}

/// Put `group` in the `unified_grid_and_search_index` step's `not_searched`, or take it out.
function setNotSearched(text: string, group: string, out: boolean): string {
  const steps = listSteps(text).filter((s) => s.kind === "step");
  const at = steps.findIndex((s) => s.id === UNIFIED_GRID_AND_SEARCH_INDEX_STEP);
  if (at < 0) return text;
  const had = notSearched(steps);
  const want = out ? (had.includes(group) ? had : [...had, group]) : had.filter((g) => g !== group);
  if (want.length === had.length) return text;
  const ref = { array: "steps", index: at };
  const path = ["params", "not_searched"];
  return want.length
    ? setIn(text, ref, path, `[${want.map(quote).join(", ")}]`)
    : deleteIn(text, ref, path);
}

/// Put a source in search as far as `asked` says: out of the `unified_grid_and_search_index` step's
/// `not_searched` for its words, and an `embed` step for its meaning, or back. Never an `embed`
/// step for a type that may not have one (`embedAllowed`), nor where the config has no
/// `unified_grid_and_search_index` step for it to read; taking one away takes every step that reads
/// it.
export function setSearchSteps(text: string, group: string, asked: SearchIndexing): string {
  const type = listGroups(text).find((g) => g.id === group)?.type;
  const indexing = asked === "keyword_and_embed" && !embedAllowed(type) ? "keyword" : asked;
  let next = setNotSearched(text, group, indexing === "none");
  const steps = listSteps(next);
  const hasIndexStep = steps.some(
    (s) => s.kind === "step" && s.id === UNIFIED_GRID_AND_SEARCH_INDEX_STEP,
  );
  const embed = buildEmbedStep(
    group,
    type ?? null,
    type === DIFF_TYPE ? sourceTypeIn(steps, group) : null,
  );
  const has = steps.some((s) => s.id === embed.id);
  if (hasIndexStep && indexing === "keyword_and_embed") {
    if (!has) next = wireIntoFanIns(insertEntries(next, embed.body), embed.id, "embedding_map");
  } else if (has) {
    const gone = [...steps.filter((s) => s.id === embed.id), ...readersOf([embed.id], steps)];
    next = unwireFromFanIns(removeSteps(next, gone), embed.id, "embedding_map");
  }
  return next;
}

/// What has to leave the config with `ids`: every step that reads one of
/// them ([`readersOf`]), and a source's `embed` when its render goes: it
/// embeds that render's documents, though it reads the `unified_grid_and_search_index` step.
/// Each `embed` reads the `unified_grid_and_search_index` step, so that step takes them all
/// with it.
export function removedWith(ids: string[], all: ConfiguredStep[]): ConfiguredStep[] {
  const readers = readersOf(ids, all);
  const going = new Set([...ids, ...readers.map((s) => s.id)]);
  const unrendered = all.filter(
    (s) =>
      s.kind === "step" &&
      s.function === "embed" &&
      !going.has(s.id) &&
      going.has(`${s.group}/render_markdown`),
  );
  return [...readers, ...unrendered];
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
const INDEX_GROUP = "unified_grid_and_search_index";

/// Add entries — a source, a comparison, a source's search steps — where the
/// file keeps reading in the order data flows: each step below the steps
/// it reads, a source's entries together. That is beside its own group's
/// entries when the group is already in the file, and otherwise after the
/// last source, just above the index every source feeds. A file already
/// out of that order may have no such place; then the entries go at the
/// end, which loads the same — the runner follows `inputs`, not the file.
///
/// The `[[programs]]` and `[[locks]]` entries the added steps need come
/// with them (`withDeclarations`).
export function insertEntries(text: string, body: string): string {
  const at = placeFor(text, body);
  return withDeclarations(
    at === null ? `${text.replace(/\s*$/, "")}\n\n${body}\n` : splice(text, at, at, body),
  );
}

/// `text` with the top-level entries its steps need and it lacks: a
/// `[[programs]]` entry for each spelling of `datalib-step` a step runs,
/// listing the verbs it takes, and a `[[locks]]` entry for each lock of
/// `LOCK_SLOTS` or of a package's manifest (`packageLockSlots`) a step
/// holds. Without the first the runner sends
/// `datalib-step` no verb; without the second it refuses the step. They
/// go after the file's last `[[packages]]`, `[[programs]]` or `[[locks]]`
/// entry, else above its first table. Unchanged text when nothing is
/// missing.
export function withDeclarations(text: string): string {
  const { ast, root } = parseConfig(text);
  const entries = (v: unknown) =>
    (Array.isArray(v) ? v : []).filter(
      (e): e is Record<string, unknown> => e !== null && typeof e === "object",
    );
  const steps = entries(root.steps);
  const programs = new Set(
    entries(root.programs).map((p) => (typeof p.command === "string" ? p.command.trim() : "")),
  );
  const locks = new Set(entries(root.locks).map((l) => l.name));
  const slots: Record<string, number> = { ...packageLockSlots(), ...LOCK_SLOTS };
  const blocks: string[] = [];
  const seen = new Set<string>();
  for (const step of steps) {
    const command = typeof step.command === "string" ? step.command : "";
    const program = programOf(command);
    // A `[[programs]]` entry names one word, so a path with a space in it
    // cannot be declared.
    if (!isDatalibStep(command) || /\s/.test(program)) continue;
    if (programs.has(program) || seen.has(program)) continue;
    seen.add(program);
    blocks.push(
      `[[programs]]\ncommand = ${quote(program)}\nverbs = [${DATALIB_STEP_VERBS.map(quote).join(", ")}]`,
    );
  }
  for (const step of steps) {
    for (const name of Array.isArray(step.locks) ? step.locks : []) {
      if (typeof name !== "string" || !(name in slots)) continue;
      if (locks.has(name) || seen.has(`lock:${name}`)) continue;
      seen.add(`lock:${name}`);
      blocks.push(`[[locks]]\nname = ${quote(name)}\nslots = ${slots[name]}`);
    }
  }
  if (blocks.length === 0) return text;
  const body = blocks.join("\n\n");
  const after = ["packages", "programs", "locks"].flatMap((k) =>
    [...ranges(ast, k).values()].map(([, end]) => end),
  );
  const at = after.length ? Math.max(...after) : firstTableStart(text);
  return at === null ? `${text.replace(/\s*$/, "")}\n\n${body}\n` : splice(text, at, at, body);
}

/// Where the first `[[…]]` table starts, with the comment lines directly
/// above it (its banner); null when the file has none.
function firstTableStart(text: string): number | null {
  const m = /^[ \t]*\[\[/m.exec(text);
  if (!m) return null;
  let at = m.index;
  while (at > 0) {
    const prevStart = text.lastIndexOf("\n", at - 2) + 1;
    if (
      !text
        .slice(prevStart, at - 1)
        .trim()
        .startsWith("#")
    )
      break;
    at = prevStart;
  }
  return at;
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
  const isIndex = (s: ConfiguredStep) => s.group === INDEX_GROUP || fanInFunctionOf(s) !== null;
  // A source's entries go above the index that reads them all; one that
  // reads the index itself (an `embed`) goes below it, above whatever
  // reads it in turn.
  const readsIndex = steps.some((s) => reads.has(s.id) && isIndex(s));
  const readsAdded = (s: ConfiguredStep) =>
    s.inputs.some((id) => addedIds.has(id)) ||
    (s.patterns ?? []).some((p) => [...addedIds].some((id) => namesStep(p, id)));
  // The map reads every embed, though one written as a list only names a
  // new one once it is wired in.
  const embeds = added.some((s) => s.function === "embed");
  const readers = [
    ...(readsIndex ? [] : steps.filter(isIndex)),
    ...steps.filter(readsAdded),
    ...(embeds ? steps.filter((s) => fanInFunctionOf(s) === "embedding_map") : []),
    ...(readsIndex ? [] : groups.filter((g) => g.id === INDEX_GROUP)),
  ];
  const latest = Math.min(text.length, ...readers.map((r) => extendOverComments(text, r.start)));
  const own = new Set(added.map((s) => s.group).filter((g) => g !== null));
  const beside = listGroups(body).length
    ? []
    : [
        ...steps.filter((s) => s.group !== null && own.has(s.group)),
        ...groups.filter((g) => own.has(g.id)),
      ];
  const besideAt = beside.length ? Math.max(...beside.map((e) => e.end)) : null;
  const at = besideAt !== null && besideAt >= earliest ? besideAt : latest;
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
/// sorter (`datalib_config::order`) moves a group as. Nothing else
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
/// file reads in the order data flows (`datalib_config::order`, which
/// `datalib-step topo-sort-config` restores), and a drag keeps it so. A
/// source's `embed` reads the index group that reads the source, so a
/// source group's reads of an index group are left out, or no source
/// could move.
export function moveAgainstDataFlow(
  text: string,
  groupId: string,
  before: string | null,
): { reads: string } | { readBy: string } | null {
  const steps = listSteps(text).filter((s) => s.kind === "step");
  const groupOfStep = new Map(steps.map((s) => [s.id, s.group]));
  const fanInGroups = new Set(steps.filter((s) => fanInFunctionOf(s) !== null).map((s) => s.group));
  const reads = (id: string) =>
    new Set(
      steps
        .filter((s) => s.group === id)
        .flatMap((s) => s.inputs.map((i) => groupOfStep.get(i)))
        .filter((g): g is string => !!g && g !== id)
        .filter((g) => fanInGroups.has(id) || !fanInGroups.has(g)),
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

/// Save an edited source's form into `text`. Each of its steps keeps its
/// place and everything the form does not own; of the keys the form does
/// own, only those whose value changed are written. A step the source
/// lacks is added, and a render step it no longer wants is removed. The
/// group's own lines are `renameGroup`'s and `describeGroup`'s. Throws
/// when a key cannot be written without changing something else.
export function patchSource(
  text: string,
  opts: { entry: CatalogEntry; group: string; values: FieldValues; renders?: boolean },
): string {
  const { entry, group, values } = opts;
  const renders = writesRenderStep(entry, opts.renders);
  let next = text;
  for (const phase of ["download", "render"] as const) {
    if (phase === "render" && !renders) continue;
    next = patchStep(next, group, phase, entry, values);
  }
  const have = sourceStepsOf(group, listSteps(next));
  if (!have.ingest) {
    const ingest = buildStep({ entry, group, phase: "download", values });
    // Above the render step that reads it, so the file still reads in
    // the order data flows.
    next = have.render
      ? tidy(
          splice(
            next,
            extendOverComments(next, have.render.start),
            extendOverComments(next, have.render.start),
            ingest,
          ),
        )
      : insertEntries(next, ingest);
  }
  if (renders && !have.render) {
    const inputs = [stepIdFor(group, "download")];
    next = insertEntries(next, buildStep({ entry, group, phase: "render", inputs, values }));
  } else if (!renders && have.render) {
    next = removeSteps(next, [have.render]);
  }
  return next;
}

/// One step's params brought to what the form says, key by key.
function patchStep(
  text: string,
  group: string,
  phase: FieldPhase,
  entry: CatalogEntry,
  values: FieldValues,
): string {
  const fn = functionOf(phase);
  const at = listSteps(text)
    .filter((s) => s.kind === "step")
    .findIndex((s) => s.group === group && s.function === fn);
  if (at < 0) return text;
  const params = () => listSteps(text).filter((s) => s.kind === "step")[at].params;
  const ref = { array: "steps", index: at };
  const before = params();
  const wanted = new Map(paramEntries(entry, values, phase).map((e) => [e.target, e]));
  const owned = new Set([
    ...presetsFor(entry, phase).map((p) => p.target),
    ...fieldsFor(entry, phase).map((f) => f.target),
    ...INERT,
  ]);
  const fields = new Map(fieldsFor(entry, phase).map((f) => [f.target, f]));
  for (const target of owned) {
    const path = ["params", ...target.split(".")];
    const want = wanted.get(target);
    const had = getParam(before, target);
    if (want) {
      if (alreadySays(want.field, had, want.value)) continue;
      text = setIn(text, ref, path, tomlValue(want.field, want.value));
    } else if (had !== undefined && readsBack(fields.get(target), had)) {
      text = deleteIn(text, ref, path);
    }
  }
  // Removing a method's last key can take its table with it, and the
  // table's presence is what names the method (see `paramsToml`).
  if (phase === "download" && entry.method && getParam(params(), entry.method) === undefined) {
    text = setIn(text, ref, ["params", entry.method], "{}");
  }
  return text;
}

/// Whether a field can show the value the config holds. One it cannot
/// (Google Drive's `exports` written as a table per kind, where the
/// field takes a list) shows empty, and an empty box must not delete
/// it; `untouchedSettings` names it instead.
function readsBack(field: Field | undefined, had: unknown): boolean {
  return field?.kind !== "string_list" || Array.isArray(had);
}

/// Does the config's value `had` already say what the form's `want`
/// does? A value the form shows unchanged is left as it is written, so
/// `5_000_000` is not rewritten as "5 MB", nor a select's `"API"` as
/// `"api"`. A `bool` or `select` the config leaves out reads as its
/// default, which mirrors the backend's, and is not written in.
function alreadySays(field: Field | undefined, had: unknown, want: unknown): boolean {
  if (!field) return JSON.stringify(had) === JSON.stringify(want);
  if (had === undefined) {
    if (field.kind === "bool") return !!want === (field.default ?? false);
    if (field.kind === "select") return want === field.default;
    return false;
  }
  if (field.kind === "bytes") {
    return parseByteSize(bytesText(had))?.bytes === parseByteSize(bytesText(want))?.bytes;
  }
  const read = jsonValue(field, formValue(field, had));
  return JSON.stringify(read) === JSON.stringify(jsonValue(field, want));
}

/// What a source's steps hold that its form has no field for: keys on the
/// step itself (`env`, `command`, `locks`, …) and params no field or
/// preset names. Saving an edit keeps every one of them.
export function untouchedSettings(
  text: string,
  group: string,
  entry: CatalogEntry,
): { step: string; keys: string[] }[] {
  const raw = (parseConfig(text).root.steps ?? []) as Record<string, unknown>[];
  const listed = listSteps(text).filter((s) => s.kind === "step");
  const out: { step: string; keys: string[] }[] = [];
  listed.forEach((step, i) => {
    if (step.group !== group || (step.phase !== "ingest" && step.phase !== "render")) return;
    const own = Object.keys(raw[i] ?? {}).filter((k) => !FORM_STEP_KEYS.has(k));
    const rep = paramsAreRepresentable(step, entry);
    const params = rep.ok ? [] : rep.unknown.map((k) => `params.${k}`);
    const keys = [...own, ...params];
    if (keys.length) out.push({ step: step.id, keys });
  });
  return out;
}

/// The keys of a `[[steps]]` entry the wizard writes or reads its
/// meaning from. `command`, `inputs`, `reads` and `locks` are here because
/// the form never changes them, so there is nothing to warn about.
const FORM_STEP_KEYS = new Set(["id", "command", "inputs", "reads", "locks", "params"]);

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

/// The ids a source may not take: the directories the runner reserves
/// (`dag::config::RESERVED_DIRS`), and the index groups' ids, which a
/// source would collide with. An id landing on one of these is suffixed
/// like any other collision rather than rejected, so the wizard never
/// proposes a name the loader would refuse or an index already holds.
const RESERVED_IDS = new Set([
  "system",
  "datalib_curated",
  "unified_index",
  "unified_document_index",
  "unified_grid_and_search_index",
  "unified_time_series_index",
]);

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

/// What the Sources table is while it has no rows: still waiting for the
/// first `GET /api/manage/rows`, unable to get it, or truly empty. Until
/// the first answer, no rows means nothing — that request can take
/// seconds while a sync runs, and "Nothing configured yet" in the
/// meantime reads as a lost config.
export type EmptyTable = "loading" | "failed" | "empty";

export function emptyTableState(input: {
  /// Whether any answer has arrived.
  answered: boolean;
  /// Why the first request failed, before any answer arrived.
  error: string | null;
  rowCount: number;
}): EmptyTable | null {
  if (input.rowCount > 0) return null;
  if (input.answered) return "empty";
  return input.error ? "failed" : "loading";
}

/// Why the table is empty, when it shouldn't be.
///
/// The browser parses the config text for the wizard, while
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
