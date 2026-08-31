// Per-*entry* view of a DAG config, for the Manager2 grid.
//
// The grid is a picture of the pipeline, not just of the data, so every
// row here is something the config declares. Three kinds:
//
//   source  a `<name>/raw` + `<name>/rendered_md` pair, grouped into one
//           row by its stanza name — identified the way the backend
//           identifies one (`datalib_dag::config::validate_steps`)
//   step    any other `[[steps]]` entry, one row each. In practice the
//           shared `grid_index` / `qmd_index` fan-ins, which write
//           `unified_index/` — often the largest thing on a real data
//           root, and invisible while this listed sources only.
//   applet  an `[[applets]]` entry: a server the http gateway spawns on
//           demand. Never scheduled and owns no artifacts, so most row
//           actions don't apply to it — but it is configured, it can
//           fail to start, and that failure should be visible here
//           rather than as a 502 in another tab.
//
// `configSources.ts` is the older, narrower thing: fringe *step ids*,
// which is what `--sync` accepts.
//
// Every entry has exactly two names, and the distinction is the whole
// point:
//
//   id    identity. Path-safe, unique, and what the directory structure
//         is formed from, so changing it moves data on disk and strands
//         the paths the index recorded. Chosen once, at creation.
//   name  what a person types and what the screen shows. Free text,
//         freely changed, meaningless to every program. Derived from
//         the id when a step declares none, so a config that never set
//         one reads exactly as it always did.
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
  /// The step's `name =`, when it declares one. Free text, and read
  /// nowhere but here — see `StepEntry::name` in dag/src/config.rs.
  name: string | null;
  phase: SourcePhase;
  /// The `datalib-step download|render <type>` word, when the command
  /// is a `datalib-step` invocation; null for a custom executable.
  type: string | null;
  params: Record<string, unknown>;
  /// [start, end) character offsets covering this step's TOML tables.
  start: number;
  end: number;
};

export type EntryKind = "source" | "step" | "applet";

export type ConfiguredSource = {
  /// Identity. A source's stanza (its directory under the data root and
  /// the stem of its step ids), or for the other kinds the entry's own
  /// `id`. Changing it moves data on disk and strands the index's
  /// `qmd_path`s, so the wizard holds it fixed after creation.
  id: string;
  kind: EntryKind;
  /// What to show. The first `name =` any of its steps declares,
  /// falling back to `id` — so an entry that never set one is
  /// displayed exactly as it always was.
  name: string;
  /// The source's type, taken from whichever phase declares one. Null
  /// for kinds that have none.
  type: string | null;
  steps: SourceStep[];
  /// Declared output paths across every step of this entry — what the
  /// storage endpoint's rows are keyed on. Empty for an applet, which
  /// owns no artifacts.
  outputs: string[];
  /// The `id` the DAG runner schedules, for a single-step entry. Null
  /// for a source (whose two steps are targeted by their own ids) and
  /// for an applet (never scheduled).
  stepId: string | null;
  /// Union span of every step, for delete.
  start: number;
  end: number;
};

const PHASE_BY_SUFFIX: Record<string, SourcePhase> = {
  raw: "download",
  rendered_md: "render",
};

/// Parse the config text and list everything it declares. Throws with
/// the parser's message (and line, when it has one) on malformed TOML.
///
/// Order is sources first (the common case, and what the Add button
/// produces), then other steps, then applets — so the table opens on
/// what someone came to look at.
export function listConfiguredSources(text: string): ConfiguredSource[] {
  let ast;
  try {
    ast = parseTOML(text);
  } catch (e) {
    const err = e as { message?: string; lineNumber?: number };
    const at = err.lineNumber !== undefined ? ` (line ${err.lineNumber})` : "";
    throw new Error(`${err.message ?? String(e)}${at}`);
  }
  const root = getStaticTOMLValue(ast) as { steps?: unknown; applets?: unknown };

  // Every entry's character range, per array. `[steps.params]` is a
  // sibling node in the AST rather than a child of the step's own
  // table, so the span has to be widened to cover it — same derivation
  // as configSources.ts.
  const ranges = (key: string) => {
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
  };

  const sources: ConfiguredSource[] = [];
  const plainSteps: ConfiguredSource[] = [];
  const byId = new Map<string, ConfiguredSource>();

  if (Array.isArray(root.steps)) {
    const stepRanges = ranges("steps");
    root.steps.forEach((raw, i) => {
      const step = raw as {
        id?: unknown;
        name?: unknown;
        command?: unknown;
        outputs?: unknown;
        params?: unknown;
      } | null;
      const id = typeof step?.id === "string" ? step.id : "";
      // Blank is the same as absent: `listConfiguredSources` reports the
      // id in both cases, so a whitespace name never blanks a row.
      const name =
        typeof step?.name === "string" && step.name.trim() !== ""
          ? step.name.trim()
          : null;
      const outputs = (Array.isArray(step?.outputs) ? (step!.outputs as unknown[]) : []).filter(
        (o): o is string => typeof o === "string",
      );
      const [start, end] = stepRanges.get(i) ?? [0, 0];
      const type = stepType(typeof step?.command === "string" ? step.command : "");
      const params =
        step?.params && typeof step.params === "object"
          ? (step.params as Record<string, unknown>)
          : {};

      // Is this a source stanza's step? Only `<name>/raw` and
      // `<name>/rendered_md` say so — which is what keeps
      // `unified_index/grid` out of the source bucket.
      const stanza = outputs
        .map((out) => out.split("/"))
        .find((segs) => segs.length === 2 && PHASE_BY_SUFFIX[segs[1]]);

      if (stanza) {
        // The stanza is the *entry's* id; `name` above is this step's
        // own declared name. Distinct locals, because conflating them
        // is exactly the mistake this rename exists to stop.
        const stanzaId = stanza[0];
        const entry: SourceStep = {
          id,
          name,
          phase: PHASE_BY_SUFFIX[stanza[1]],
          type,
          params,
          start,
          end,
        };
        const existing = byId.get(stanzaId);
        if (existing) {
          existing.steps.push(entry);
          existing.outputs.push(...outputs);
          existing.type = existing.type ?? type;
          existing.start = Math.min(existing.start, start || existing.start);
          existing.end = Math.max(existing.end, end);
        } else {
          const source: ConfiguredSource = {
            id: stanzaId,
            // Filled in by the resolution pass at the end of this function.
            name: "",
            kind: "source",
            type,
            steps: [entry],
            outputs: [...outputs],
            stepId: null,
            start,
            end,
          };
          byId.set(stanzaId, source);
          sources.push(source);
        }
        return;
      }

      plainSteps.push({
        id: id || `step ${i + 1}`,
        // Filled in by the resolution pass at the end of this function.
        name: "",
        kind: "step",
        type,
        // A non-source step has no download/render phase; call it
        // download so the phase-keyed helpers have something to key on.
        steps: [{ id, name, phase: "download", type, params, start, end }],
        outputs,
        stepId: id || null,
        start,
        end,
      });
    });
  }

  const applets: ConfiguredSource[] = [];
  if (Array.isArray(root.applets)) {
    const appletRanges = ranges("applets");
    root.applets.forEach((raw, i) => {
      const applet = raw as { id?: unknown; command?: unknown } | null;
      const id = typeof applet?.id === "string" ? applet.id : `applet ${i + 1}`;
      const [start, end] = appletRanges.get(i) ?? [0, 0];
      applets.push({
        id,
        // Filled in by the resolution pass at the end of this function.
        name: "",
        kind: "applet",
        // The word after `datalib-applet`, when it is one — the same
        // shape as a step's provider word, and what names the applet.
        type: appletType(typeof applet?.command === "string" ? applet.command : ""),
        steps: [],
        outputs: [],
        stepId: null,
        start,
        end,
      });
    });
  }

  const entries = [...sources, ...plainSteps, ...applets];
  // Resolved once each group is complete: the first step declaring a
  // name names the entry, and one with no name anywhere is displayed by
  // its id, exactly as before names existed. Applets carry no steps and
  // no `name` key, so they always fall back to their id.
  for (const entry of entries) {
    entry.name = entry.steps.find((s) => s.name)?.name ?? entry.id;
  }
  return entries;
}

/// `datalib-applet unified_index` → `unified_index`. Null for anything
/// else, which is legitimate — an applet may be any executable.
function appletType(command: string): string | null {
  const words = command.trim().split(/\s+/);
  if (words.length < 2) return null;
  if (!/(^|\/)datalib-applet$/.test(words[0])) return null;
  return words[1];
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
///
/// Control characters are escaped rather than passed through: a raw
/// newline or tab inside a basic string is a parse error, so a label or
/// a pasted value containing one would write a `config.toml` that no
/// longer loads.
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

/// The download + render step pair for one source, with a divider so
/// sources stay visually separated in the raw file.
///
/// `id` is the identity — the stanza directory and the stem of both
/// step ids. `name` rides on the download step alone, and only when it
/// says something the id doesn't: a `name` that respells the id would
/// be a second, silent spelling of one string, which is what got the
/// applet `title` key deleted (00633dd5), and it would churn every
/// existing config the first time someone opened its Edit form.
export function buildStepPair(
  entry: CatalogEntry,
  id: string,
  name: string,
  values: FieldValues,
): string {
  const divider = `# ── ${id} ${"─".repeat(Math.max(4, 66 - id.length))}`;
  const nameLine =
    name.trim() && name.trim() !== id ? `\nname = ${quote(name.trim())}` : "";
  const download = `[[steps]]
id = "${id}.download"${nameLine}
command = "datalib-step download ${entry.type}"
outputs = ["${id}/raw"]
${paramsToml(entry, values, "download")}`;

  // Download-only providers (lightroom, fsindex) render nothing, so
  // they declare no render step at all.
  if (entry.renderStep === false) return `${divider}\n${download.trimEnd()}`;

  const renderParams = paramsToml(entry, values, "render");
  const render = `[[steps]]
id = "${id}.render"
command = "datalib-step render ${entry.type}"
inputs = ["${id}/raw"]
outputs = ["${id}/rendered_md"]${renderParams ? `\n${renderParams}` : ""}`;

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

/// A human name reduced to something that can be a directory: NFKD
/// normalize, drop combining marks, lowercase, every run of
/// non-alphanumerics to a single `-`, trimmed, capped.
///
/// Word order is preserved — "Work Slack" is `work-slack`. The cap is
/// 40 because this becomes a path component inside paths that already
/// carry UUIDs.
///
/// Returns `""` when nothing survives, which is the normal outcome for
/// a name written in a non-Latin script or made only of punctuation.
/// Callers fall back to the catalog's default rather than inventing
/// something — see [`suggestId`].
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
///
/// `taken` holds the ids already in the config. A source reserves its
/// whole stanza, since its two steps are `<id>.download` / `<id>.render`
/// writing `<id>/raw` and `<id>/rendered_md` — so the caller passes
/// stanza ids, not step ids.
///
/// `fallback` is used when `base` is empty, which happens whenever the
/// name slugifies to nothing.
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
/// `GET /api/config` reports what the *backend's* loader made of the
/// same file. Those two must agree. When they don't — the server counts
/// sources and the table shows none — the bug is on this side, and the
/// empty state has to say so instead of offering a friendly "nothing
/// configured yet" that sends someone looking at their own config.
///
/// This exists because that exact disagreement was reported from the
/// desktop app and could not be reproduced against the same backend in
/// a browser. A silent empty table gives an investigation nothing to
/// go on; this makes the next occurrence self-describing.
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
