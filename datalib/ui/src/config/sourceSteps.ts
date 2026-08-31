// Per-*entry* view of a DAG config, for the Manager2 grid.
//
// The grid is a picture of the pipeline, so every row here is one thing
// the config declares — one `[[steps]]` entry or one `[[applets]]`
// entry, never a group of them.
//
// **There is no "data source" here, deliberately.** A source used to be
// a row: a `<name>/raw` + `<name>/rendered_md` pair fused into one
// entry, edited by one form, run as one unit. It was never a config
// entity — the grouping was invented here and reconstructed by
// splitting paths — and it cost more than it bought. A fetch step and a
// render step have separate options, separate outputs, separate disk
// footprints and separate reasons to re-run; the runner has always
// treated them as two steps. So does this now.
//
// What survives is a *display* relationship: two steps sharing an id
// stem (`work-slack/raw`, `work-slack/rendered_md`) are siblings under
// one directory, which the table shows and the wizard uses to propose
// the second step's id. Nothing resolves anything by it.
//
// Two kinds of row:
//
//   step    a `[[steps]]` entry. `phase` classifies it for display —
//           fetch, render, or index — from the shape of its id.
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

/// Which wave a step belongs to, for display and for picking the right
/// half of a catalog entry's fields. Derived from the shape of the id,
/// never from anything load-bearing.
///
///   fetch   `<stem>/raw` — brings data in
///   render  `<stem>/rendered_md` — turns it into markdown
///   index   anything under `unified_index/` — the shared fan-ins
///   other   any other step: a custom executable doing its own thing
export type StepPhase = "fetch" | "render" | "index" | "other";

export type EntryKind = "step" | "applet";

export type ConfiguredStep = {
  /// Identity, and the tree this step writes. Path-safe, unique, and
  /// what the directory structure is formed from — so changing it moves
  /// data on disk and strands the paths the index recorded, which is
  /// why the wizard holds it fixed after creation.
  id: string;
  kind: EntryKind;
  /// What to show: the step's `name =`, falling back to `id`. A step
  /// that never set one is displayed exactly as it always was.
  name: string;
  phase: StepPhase;
  /// The `datalib-step download|render <type>` word, when the command
  /// is a `datalib-step` invocation; the word after `datalib-applet`
  /// for an applet; null for anything else, which is a legitimate
  /// config with no catalog entry.
  type: string | null;
  /// The ids this step declares as inputs.
  inputs: string[];
  params: Record<string, unknown>;
  /// [start, end) character offsets covering this entry's TOML tables,
  /// for splice-based edit and delete.
  start: number;
  end: number;
};

/// The id stem two sibling steps share (`work-slack/raw` →
/// `work-slack`). A display convenience and the seed for proposing a
/// sibling's id — never how anything is resolved. Mirrors
/// `ArtifactPath::stem` on the Rust side.
export function stemOf(id: string): string {
  const at = id.indexOf("/");
  return at < 0 ? id : id.slice(0, at);
}

const PHASE_BY_LEAF: Record<string, StepPhase> = {
  raw: "fetch",
  rendered_md: "render",
};

/// A step's phase, from the shape of its id.
export function phaseOf(id: string): StepPhase {
  const segs = id.split("/");
  if (segs[0] === "unified_index") return "index";
  if (segs.length === 2 && PHASE_BY_LEAF[segs[1]]) return PHASE_BY_LEAF[segs[1]];
  return "other";
}

/// Parse the config text and list every entry it declares, one row per
/// entry. Throws with the parser's message (and line, when it has one)
/// on malformed TOML.
///
/// Steps in the order the file writes them, then applets. File order is
/// what someone editing the config expects to see, and it puts sibling
/// fetch/render steps adjacent for free, since that is how they are
/// written.
export function listSteps(text: string): ConfiguredStep[] {
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

  const steps: ConfiguredStep[] = [];

  if (Array.isArray(root.steps)) {
    const stepRanges = ranges("steps");
    root.steps.forEach((raw, i) => {
      const step = raw as {
        id?: unknown;
        name?: unknown;
        command?: unknown;
        inputs?: unknown;
        params?: unknown;
      } | null;
      const id = typeof step?.id === "string" ? step.id : "";
      // Blank is the same as absent: the row falls back to the id in
      // both cases, so a whitespace name never blanks a row.
      const name =
        typeof step?.name === "string" && step.name.trim() !== ""
          ? step.name.trim()
          : null;
      const [start, end] = stepRanges.get(i) ?? [0, 0];
      steps.push({
        // An entry with no `id` is malformed and the loader will say so;
        // give it something addressable rather than an empty row.
        id: id || `step ${i + 1}`,
        kind: "step",
        name: name ?? (id || `step ${i + 1}`),
        phase: phaseOf(id),
        type: stepType(typeof step?.command === "string" ? step.command : ""),
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
    const appletRanges = ranges("applets");
    root.applets.forEach((raw, i) => {
      const applet = raw as { id?: unknown; name?: unknown; command?: unknown } | null;
      const id = typeof applet?.id === "string" ? applet.id : `applet ${i + 1}`;
      const [start, end] = appletRanges.get(i) ?? [0, 0];
      applets.push({
        id,
        kind: "applet",
        // `AppletEntry` has no `name` key — an applet takes its display
        // label through its own `params` — so it is shown by its id.
        name: id,
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

/// Can the wizard round-trip this step without losing anything?
///
/// Editing regenerates the step from the form, so any params key the
/// descriptor doesn't model would be dropped. Rather than silently lose
/// a hand-written `common.download_params` block, the grid disables
/// Edit and points at the config editor. A form that quietly drops a
/// setting is worse than no form.
export function paramsAreRepresentable(
  step: ConfiguredStep,
  entry: CatalogEntry,
): { ok: true } | { ok: false; unknown: string[] } {
  const known = new Set(fieldsFor(entry, fieldPhaseOf(step)).map((f) => f.target));
  const unknown = leafPaths(step.params).filter((path) => !known.has(path));
  return unknown.length === 0 ? { ok: true } : { ok: false, unknown };
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
/// `signal_backup` declares a render knob today, so a render step's
/// form is usually a name and nothing else.
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

/// One step, as a `[[steps]]` block with a divider above it.
///
/// `id` is the identity — the tree the step writes. `name` is written
/// only when it says something the id doesn't: a `name` that respells
/// the id would be a second, silent spelling of one string, which is
/// what got the applet `title` key deleted (00633dd5), and it would
/// churn every existing config the first time someone opened its Edit
/// form.
///
/// `phase` picks which half of the descriptor's fields to write and
/// which `datalib-step` subcommand to invoke. `inputs` is written when
/// non-empty — a fetch step has none (its real input is a remote
/// service or a path in its params), a render step names the fetch step
/// it reads.
export function buildStep(opts: {
  entry: CatalogEntry;
  id: string;
  name: string;
  phase: FieldPhase;
  inputs?: string[];
  values: FieldValues;
}): string {
  const { entry, id, name, phase, values } = opts;
  const inputs = opts.inputs ?? [];
  const divider = `# ── ${id} ${"─".repeat(Math.max(4, 66 - id.length))}`;
  const nameLine =
    name.trim() && name.trim() !== id ? `\nname = ${quote(name.trim())}` : "";
  const inputsLine = inputs.length
    ? `\ninputs = [${inputs.map(quote).join(", ")}]`
    : "";
  const subcommand = phase === "render" ? "render" : "download";
  const params = paramsToml(entry, values, phase);
  const block = `[[steps]]
id = ${quote(id)}${nameLine}
command = "datalib-step ${subcommand} ${entry.type}"${inputsLine}${params ? `\n${params}` : ""}`;
  return `${divider}\n${block.trimEnd()}`;
}

/// The id of the render step that would read `fetchId`: its sibling
/// under the same stem.
///
/// The one place a `/` is split off an id to mint another, and
/// deliberately the only one — the chained wizard and the standalone
/// "render this" action both come through here, because two code paths
/// minting one string is how they drift. It proposes a default from a
/// string the user just chose; nothing resolves identity by it.
export function renderIdFor(fetchId: string): string {
  return `${stemOf(fetchId)}/rendered_md`;
}

/// Wire a render step into every fan-in that consumes rendered markdown.
///
/// The fan-ins name their inputs by id, so a source added without this
/// renders happily and is never indexed — invisible in search, with
/// nothing on screen to say why. The old `**/rendered_md` glob made
/// this automatic; naming steps is the trade, and the wizard paying it
/// is what keeps the config honest rather than implicit.
///
/// Textual, like every other write here: it rewrites the `inputs = [
/// … ]` line of each step whose id is a `unified_index/…` tree, leaving
/// the rest of the file — comments included — exactly as it was. A
/// config with no fan-ins is left alone.
export function wireIntoFanIns(text: string, renderStepId: string): string {
  return text.replace(
    // `id = "unified_index/…"` followed, within its own table, by an
    // `inputs = [...]` line.
    /(id\s*=\s*"unified_index\/[^"]*"[^\[]*?inputs\s*=\s*\[)([^\]]*)(\])/g,
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
    /(id\s*=\s*"unified_index\/[^"]*"[^\[]*?inputs\s*=\s*\[)([^\]]*)(\])/g,
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

/// Append a step pair to the config text. Always at the end: the DAG
/// derives execution order from artifact paths rather than file order,
/// and in TOML the end is the only safe insertion point — every key
/// after a `[[steps]]` header belongs to that table, so a mid-file
/// splice would reparent whatever followed.
export function appendSource(text: string, body: string): string {
  return `${text.replace(/\s*$/, "")}\n\n${body}\n`;
}

/// Remove entries from the config text.
///
/// Takes a list because deleting a fetch step usually means deleting
/// the render step that reads it too: an input naming a step that no
/// longer exists is a config the loader refuses outright, so a partial
/// delete produces a file that will not load.
///
/// Splices each range back to front, so offsets stay valid as earlier
/// text shifts. A divider comment above a step isn't part of any AST
/// node, so it's swept by extending each cut back over
/// immediately-preceding comment lines.
export function removeSteps(text: string, steps: ConfiguredStep[]): string {
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

/// Replace one step with a freshly generated one. Only safe when
/// `paramsAreRepresentable` said so — see this module's header.
///
/// The replacement is appended rather than spliced in place, since the
/// end of the file is the only safe insertion point in TOML (every key
/// after a `[[steps]]` header belongs to that table). A step therefore
/// moves to the bottom when edited, which is cosmetic: the DAG reads
/// `inputs`, not file order.
export function replaceStep(text: string, step: ConfiguredStep, body: string): string {
  return appendSource(removeSteps(text, [step]), body);
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
