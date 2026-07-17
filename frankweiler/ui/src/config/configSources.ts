// Read-only view of the `steps:` list in a DAG config.yaml text, for
// the Sources table that sits next to the raw editor. Steps written as
// `<source_type>.download` / `<source_type>.render` are grouped back
// into per-source rows (we only handle the standard shape where a
// source's steps sit adjacent in the file — the templates emit them
// that way); source-independent steps (`index`, `qmd`, raw `run:`
// entries) are infrastructure and don't get rows. Each row carries the
// character range covering its step entries so the table's "Locate
// config" button can select the whole pair in the editor. The text
// itself is the single source of truth — there is no fragment editing
// or reassembly.

import { parseDocument, isMap, isSeq } from "yaml";

export type SourceRow = {
  name: string;
  /// Source type from the step type (`slack_api` in
  /// `slack_api.download`), "" when missing.
  type: string;
  /// The new format has no per-source enable flag (delete or comment
  /// the steps instead); kept for table compatibility.
  enabled: boolean;
  /// [start, end) character offsets covering the source's adjacent
  /// step entries. `start` is extended back to the beginning of the
  /// first `- ` line so a selection covers the stanzas as written.
  start: number;
  end: number;
};

type StepItem = {
  /// Source name (from params.name), "" for non-source steps.
  name: string;
  /// Source type from a dotted step type, "" otherwise.
  type: string;
  start: number;
  end: number;
};

/// Parse the whole config text and list its sources. Throws Error with
/// the YAML parser's message when the text doesn't parse.
export function listSources(text: string): SourceRow[] {
  const doc = parseDocument(text);
  if (doc.errors.length > 0) {
    throw new Error(doc.errors[0].message);
  }
  const seq = doc.get("steps", true);
  if (!isSeq(seq)) return [];

  const items: StepItem[] = seq.items.map((item) => {
    let name = "";
    let type = "";
    if (isMap(item)) {
      const js = item.toJSON() as {
        step?: unknown;
        params?: { name?: unknown };
      };
      if (typeof js.step === "string") {
        const dot = js.step.lastIndexOf(".");
        if (dot > 0) type = js.step.slice(0, dot);
      }
      if (typeof js.params?.name === "string") name = js.params.name;
    }
    const range = (item as { range?: [number, number, number] }).range;
    const valueStart = range?.[0] ?? 0;
    const end = range?.[1] ?? valueStart;
    // range starts at the item's value (after the `- ` marker); walk
    // back to the line start so the selection includes the marker.
    const start = text.lastIndexOf("\n", Math.max(valueStart - 1, 0)) + 1;
    return { name, type, start, end };
  });

  // Group adjacent steps that share a source name into one row.
  const rows: SourceRow[] = [];
  for (const it of items) {
    if (!it.name || !it.type) continue;
    const last = rows[rows.length - 1];
    if (last && last.name === it.name) {
      last.end = Math.max(last.end, it.end);
      if (!last.type) last.type = it.type;
    } else {
      rows.push({
        name: it.name,
        type: it.type,
        enabled: true,
        start: it.start,
        end: it.end,
      });
    }
  }
  return rows;
}
