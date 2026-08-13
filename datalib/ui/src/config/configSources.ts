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
// TOML is the only format here, as it is everywhere else in the app.
// A data root written before the switch is converted once, out of
// band, by the `datalib-migrate-config` program.

import { parseTOML, getStaticTOMLValue } from "toml-eslint-parser";

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
export function listSources(text: string): SourceRow[] {
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
    // Only an explicitly declared, non-empty `inputs` disqualifies a
    // step from being a source.
    const inputs = (step as { inputs?: unknown } | null)?.inputs;
    if (Array.isArray(inputs) && inputs.length > 0) return;
    // A step written inline (`steps = [{…}]`) has no table node of its
    // own; it gets a zero range, which the UI reads as "not locatable"
    // rather than selecting some unrelated span.
    const [start, end] = ranges.get(i) ?? [0, 0];
    const id = (step as { id?: unknown } | null)?.id;
    rows.push({ id: typeof id === "string" ? id : "", start, end });
  });
  return rows;
}
