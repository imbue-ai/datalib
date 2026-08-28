// Per-*source* view of a DAG config, for the Manager2 grid.
//
// `configSources.ts` lists fringe *steps* — what `--sync` accepts. This
// module groups steps into the thing a person configured: a source
// stanza, identified the same way the backend identifies one, by the
// first path segment of a `<name>/raw` or `<name>/rendered_md` output
// (`datalib_dag::config::validate_steps`). That deliberately excludes
// the aggregate index steps, whose outputs are `unified_index/grid` and
// `unified_index/qmd`.
//
// Writes are whole-text: a source's steps occupy a contiguous-ish set
// of character ranges, and add/delete splice the text the editor holds.
// Field-level editing that preserves comments needs a format-preserving
// TOML writer (`toml_edit`, backend-side) — see docs/dev/source_wizard.md.
// Until then `paramsAreRepresentable` gates the Edit button, so the
// wizard never silently drops something it can't model.

import { parseTOML, getStaticTOMLValue } from "toml-eslint-parser";
import type { CatalogEntry, Field, FieldPhase } from "./catalog";

export type SourcePhase = "download" | "render";

export type SourceStep = {
  id: string;
  phase: SourcePhase;
  /// The `datalib-step download|render <type>` word, when the command
  /// is a `datalib-step` invocation; null for a custom executable.
  type: string | null;
  params: Record<string, unknown>;
  /// [start, end) character offsets covering this step's TOML tables.
  start: number;
  end: number;
};

export type ConfiguredSource = {
  /// The stanza name — its directory under the data root, and the stem
  /// of its step ids.
  name: string;
  /// The source's type, taken from whichever phase declares one.
  type: string | null;
  steps: SourceStep[];
  /// Union span of every step, for delete.
  start: number;
  end: number;
};

const PHASE_BY_SUFFIX: Record<string, SourcePhase> = {
  raw: "download",
  rendered_md: "render",
};

/// Parse the config text and group its steps into sources. Throws with
/// the parser's message (and line, when it has one) on malformed TOML.
export function listConfiguredSources(text: string): ConfiguredSource[] {
  let ast;
  try {
    ast = parseTOML(text);
  } catch (e) {
    const err = e as { message?: string; lineNumber?: number };
    const at = err.lineNumber !== undefined ? ` (line ${err.lineNumber})` : "";
    throw new Error(`${err.message ?? String(e)}${at}`);
  }
  const steps = (getStaticTOMLValue(ast) as { steps?: unknown }).steps;
  if (!Array.isArray(steps)) return [];

  // Every step index's character range. `[steps.params]` is a sibling
  // node in the AST rather than a child of the step's own table, so the
  // span has to be widened to cover it — same derivation as
  // configSources.ts.
  const ranges = new Map<number, [number, number]>();
  for (const node of ast.body[0].body) {
    if (node.type !== "TOMLTable") continue;
    const [key, index] = node.resolvedKey;
    if (key !== "steps" || typeof index !== "number") continue;
    const prev = ranges.get(index);
    ranges.set(
      index,
      prev
        ? [Math.min(prev[0], node.range[0]), Math.max(prev[1], node.range[1])]
        : [node.range[0], node.range[1]],
    );
  }

  const byName = new Map<string, ConfiguredSource>();
  steps.forEach((raw, i) => {
    const step = raw as {
      id?: unknown;
      command?: unknown;
      outputs?: unknown;
      params?: unknown;
    } | null;
    const outputs = Array.isArray(step?.outputs) ? (step!.outputs as unknown[]) : [];
    for (const out of outputs) {
      if (typeof out !== "string") continue;
      const segments = out.split("/");
      if (segments.length !== 2) continue;
      const phase = PHASE_BY_SUFFIX[segments[1]];
      if (!phase) continue;
      const name = segments[0];

      const [start, end] = ranges.get(i) ?? [0, 0];
      const entry: SourceStep = {
        id: typeof step?.id === "string" ? step.id : "",
        phase,
        type: stepType(typeof step?.command === "string" ? step.command : ""),
        params:
          step?.params && typeof step.params === "object"
            ? (step.params as Record<string, unknown>)
            : {},
        start,
        end,
      };
      const existing = byName.get(name);
      if (existing) {
        existing.steps.push(entry);
        existing.type = existing.type ?? entry.type;
        existing.start = Math.min(existing.start, start || existing.start);
        existing.end = Math.max(existing.end, end);
      } else {
        byName.set(name, {
          name,
          type: entry.type,
          steps: [entry],
          start,
          end,
        });
      }
      break; // one stanza per step; the first stanza-shaped output wins
    }
  });
  return [...byName.values()];
}

/// `datalib-step download slack_api` → `slack_api`. Null for anything
/// that isn't a `datalib-step` download/render invocation, which is a
/// legitimate config (any executable can be a step) but has no
/// catalog entry.
function stepType(command: string): string | null {
  const words = command.trim().split(/\s+/);
  const i = words.findIndex((w) => w === "download" || w === "render");
  if (i < 0 || i + 1 >= words.length) return null;
  if (!/(^|\/)datalib-step$/.test(words[0])) return null;
  return words[i + 1];
}

/// Read a dotted path (`sync.channels`) out of a params tree.
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

/// Can the wizard round-trip this source without losing anything?
///
/// Editing regenerates the step pair from the form, so any params key
/// the descriptor doesn't model would be dropped. Rather than silently
/// lose a hand-written `common.download_params` block, the grid
/// disables Edit and points at the config editor. A form that quietly
/// drops a setting is worse than no form.
export function paramsAreRepresentable(
  source: ConfiguredSource,
  entry: CatalogEntry,
): { ok: true } | { ok: false; unknown: string[] } {
  const unknown: string[] = [];
  for (const step of source.steps) {
    const known = new Set(fieldsFor(entry, step.phase).map((f) => f.target));
    for (const path of leafPaths(step.params)) {
      // Name the phase in the message: `common.input_path` means
      // different things on a download and a render step, and the
      // person reading this is about to go find it in the file.
      if (!known.has(path)) unknown.push(`${step.phase}.${path}`);
    }
  }
  return unknown.length === 0 ? { ok: true } : { ok: false, unknown };
}

/// A descriptor's fields for one phase. `phase` is optional on a field
/// and defaults to `download`, which is where all but a handful sit.
export function fieldsFor(entry: CatalogEntry, phase: FieldPhase): Field[] {
  return (entry.fields ?? []).filter((f) => (f.phase ?? "download") === phase);
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

export type FieldValues = Record<string, unknown>;

/// Render the download step's `[steps.params.…]` body from form values.
/// Emitted as sub-table headers, so it must come last within its step —
/// in TOML every key after a table header belongs to that table.
function paramsToml(entry: CatalogEntry, values: FieldValues, phase: FieldPhase): string {
  // Group by the table each target sits in (`sync.channels` → `sync`).
  const tables = new Map<string, string[]>();
  for (const field of fieldsFor(entry, phase)) {
    const value = values[field.target];
    if (!isSet(field, value)) continue;
    const dot = field.target.lastIndexOf(".");
    const table = dot < 0 ? "" : field.target.slice(0, dot);
    const key = dot < 0 ? field.target : field.target.slice(dot + 1);
    const lines = tables.get(table) ?? [];
    lines.push(`${key} = ${tomlValue(field, value)}`);
    tables.set(table, lines);
  }
  if (tables.size === 0) {
    // On a render step, no knobs means no params at all.
    if (phase === "render") return "";
    // On a download step, an empty `sync` table is not the same as no
    // sync block: for several providers its *presence* selects the
    // live-download path over a file-backed one.
    return "[steps.params]\nsync = {}";
  }
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

function isSet(field: Field, value: unknown): boolean {
  if (value === undefined || value === null) return false;
  if (field.kind === "string_list") return Array.isArray(value) && value.length > 0;
  if (field.kind === "text" || field.kind === "date") return String(value).trim() !== "";
  if (field.kind === "int") return value !== "" && Number.isFinite(Number(value));
  // A boolean is always meaningful — false is a real setting, and for
  // `media` (which defaults true) omitting it would change behavior.
  return true;
}

function tomlValue(field: Field, value: unknown): string {
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
  return `"${s.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
}

/// The download + render step pair for one source, with a divider so
/// sources stay visually separated in the raw file.
export function buildStepPair(
  entry: CatalogEntry,
  name: string,
  values: FieldValues,
): string {
  const divider = `# ── ${name} ${"─".repeat(Math.max(4, 66 - name.length))}`;
  const download = `[[steps]]
id = "${name}.download"
command = "datalib-step download ${entry.type}"
outputs = ["${name}/raw"]
${paramsToml(entry, values, "download")}`;

  // Download-only providers (lightroom, fsindex) render nothing, so
  // they declare no render step at all.
  if (entry.renderStep === false) return `${divider}\n${download.trimEnd()}`;

  const renderParams = paramsToml(entry, values, "render");
  const render = `[[steps]]
id = "${name}.render"
command = "datalib-step render ${entry.type}"
inputs = ["${name}/raw"]
outputs = ["${name}/rendered_md"]${renderParams ? `\n${renderParams}` : ""}`;

  return `${divider}\n${download.trimEnd()}\n\n${render}`;
}

/// Append a step pair to the config text. Always at the end: the DAG
/// derives execution order from artifact paths rather than file order,
/// and in TOML the end is the only safe insertion point — every key
/// after a `[[steps]]` header belongs to that table, so a mid-file
/// splice would reparent whatever followed.
export function appendSource(text: string, body: string): string {
  return `${text.replace(/\s*$/, "")}\n\n${body}\n`;
}

/// Remove a source's steps from the config text.
///
/// Splices each step's range individually, back to front, so ranges
/// stay valid as earlier text shifts. The divider comment above a step
/// isn't part of any AST node, so it's swept by extending each cut back
/// over immediately-preceding comment lines.
export function removeSource(text: string, source: ConfiguredSource): string {
  const cuts = source.steps
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
/// deleting a source takes its banner with it.
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

/// Replace a source's steps with a freshly generated pair. Only safe
/// when `paramsAreRepresentable` said so — see this module's header.
export function replaceSource(
  text: string,
  source: ConfiguredSource,
  body: string,
): string {
  return appendSource(removeSource(text, source), body);
}

/// Names already taken, so the wizard can avoid proposing a collision
/// that `validate_steps` would reject on save.
export function suggestName(taken: Set<string>, base: string): string {
  if (!taken.has(base)) return base;
  for (let n = 2; n < 1000; n++) {
    const candidate = `${base}-${n}`;
    if (!taken.has(candidate)) return candidate;
  }
  return base;
}
