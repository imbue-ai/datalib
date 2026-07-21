// Read-only view of the `steps:` list in a DAG config.yaml text, for
// the Sources table that sits next to the raw editor. A source is any
// step with no declared `inputs:` — a fringe step, exactly what the
// runner's `--sync` can target — shown by its step id. Nothing about
// the step's command matters here; the derivation is fully generic
// (and mirrors the backend's in http/src/lib.rs `load_dag_config`).
// Each row carries the character range covering its step entry so the
// table's "Locate config" button can select it in the editor. The
// text itself is the single source of truth — there is no fragment
// editing or reassembly.

import { parseDocument, isMap, isSeq } from "yaml";

export type SourceRow = {
  /// The step's `id:` ("" for malformed entries).
  id: string;
  /// [start, end) character offsets covering the step entry. `start`
  /// is extended back to the beginning of the `- ` line so a
  /// selection covers the stanza as written.
  start: number;
  end: number;
};

/// Parse the whole config text and list its source steps (the ones
/// with no inputs). Throws Error with the YAML parser's message when
/// the text doesn't parse.
export function listSources(text: string): SourceRow[] {
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
    if (Array.isArray(js.inputs) && js.inputs.length > 0) continue;
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
