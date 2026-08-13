// Read-only view of the `[[steps]]` tables in a DAG config.toml text,
// for the Sources table that sits next to the raw editor. A source is
// any step with no declared `inputs` — a fringe step, exactly what the
// runner's `--sync` can target — shown by its step id. Nothing about
// the step's command matters here; the derivation is fully generic
// (and mirrors the backend's in http/src/lib.rs `load_dag_config`).
// Each row carries the character range covering its step entry so the
// table's "Locate config" button can select it in the editor. The
// text itself is the single source of truth — there is no fragment
// editing or reassembly.
//
// Data roots written before the TOML switch are still served as YAML
// (see `ConfigResponse.format`), so both parsers live here; which one
// runs is the caller's choice, never a guess about the content.

import { parseDocument, isMap, isSeq } from "yaml";
import { parseTOML, getStaticTOMLValue } from "toml-eslint-parser";
import type { ConfigFormat } from "../api";

export type SourceRow = {
  /// The step's `id` ("" for malformed entries).
  id: string;
  /// [start, end) character offsets covering the step entry.
  start: number;
  end: number;
};

/// Parse the whole config text and list its source steps (the ones
/// with no inputs). Throws Error with the parser's message when the
/// text doesn't parse.
export function listSources(text: string, format: ConfigFormat): SourceRow[] {
  return format === "yaml" ? listYamlSources(text) : listTomlSources(text);
}

/// Does this step entry count as a source? Only an explicitly declared,
/// non-empty `inputs` disqualifies it.
function isSource(step: unknown): boolean {
  const inputs = (step as { inputs?: unknown } | null)?.inputs;
  return !(Array.isArray(inputs) && inputs.length > 0);
}

function listTomlSources(text: string): SourceRow[] {
  let ast;
  try {
    ast = parseTOML(text);
  } catch (e) {
    const err = e as { message?: string; lineNumber?: number; column?: number };
    const at =
      err.lineNumber !== undefined ? ` (line ${err.lineNumber})` : "";
    throw new Error(`${err.message ?? String(e)}${at}`);
  }
  const steps = (getStaticTOMLValue(ast) as { steps?: unknown }).steps;
  if (!Array.isArray(steps)) return [];

  // Every step index's range, from the `[[steps]]` table that opens it
  // plus any `[steps.N.…]` sub-tables that follow — `[steps.params]`
  // is a sibling node in the AST, not a child of the step's own table,
  // so the selection has to be widened to cover it.
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

  const rows: SourceRow[] = [];
  steps.forEach((step, i) => {
    if (!isSource(step)) return;
    // A step written inline (`steps = [{…}]`) has no table node of its
    // own; it gets a zero range, which the UI reads as "not locatable"
    // rather than selecting some unrelated span.
    const [start, end] = ranges.get(i) ?? [0, 0];
    const id = (step as { id?: unknown } | null)?.id;
    rows.push({ id: typeof id === "string" ? id : "", start, end });
  });
  return rows;
}

/// The pre-TOML path: same derivation over a YAML `steps:` sequence.
function listYamlSources(text: string): SourceRow[] {
  const doc = parseDocument(text);
  if (doc.errors.length > 0) {
    throw new Error(doc.errors[0].message);
  }
  const seq = doc.get("steps", true);
  if (!isSeq(seq)) return [];

  const rows: SourceRow[] = [];
  for (const item of seq.items) {
    if (!isMap(item)) continue;
    const js = item.toJSON() as { id?: unknown; inputs?: unknown };
    if (!isSource(js)) continue;
    const range = (item as { range?: [number, number, number] }).range;
    const valueStart = range?.[0] ?? 0;
    const end = range?.[1] ?? valueStart;
    // range starts at the item's value (after the `- ` marker); walk
    // back to the line start so the selection includes the marker.
    const start = text.lastIndexOf("\n", Math.max(valueStart - 1, 0)) + 1;
    rows.push({
      id: typeof js.id === "string" ? js.id : "",
      start,
      end,
    });
  }
  return rows;
}
