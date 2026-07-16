// Read-only view of the `sources:` list in a config.yaml text, for the
// Sources table that sits next to the raw editor. Each entry carries
// its character range in the text so the table's "Locate config" button can select
// the stanza in the editor. The text itself is the single source of
// truth — there is no fragment editing or reassembly.

import { parseDocument, isMap, isSeq } from "yaml";

export type SourceRow = {
  name: string;
  /// `source.type` discriminator, "" when missing.
  type: string;
  enabled: boolean;
  /// [start, end) character offsets of the entry in the text. `start`
  /// is extended back to the beginning of the `- ` line so a selection
  /// covers the whole stanza as written.
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
  const seq = doc.get("sources", true);
  if (!isSeq(seq)) return [];
  return seq.items.map((item) => {
    let name = "";
    let type = "";
    let enabled = true;
    if (isMap(item)) {
      const js = item.toJSON() as {
        name?: unknown;
        enabled?: unknown;
        source?: { type?: unknown };
      };
      if (typeof js.name === "string") name = js.name;
      if (typeof js.source?.type === "string") type = js.source.type;
      enabled = js.enabled !== false;
    }
    const range = (item as { range?: [number, number, number] }).range;
    const valueStart = range?.[0] ?? 0;
    const end = range?.[1] ?? valueStart;
    // range starts at the item's value (after the `- ` marker); walk
    // back to the line start so the selection includes the marker.
    const start = text.lastIndexOf("\n", Math.max(valueStart - 1, 0)) + 1;
    return { name, type, enabled, start, end };
  });
}
