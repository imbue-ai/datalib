// Split/join between the raw `config.yaml` text and the Sources table's
// two-part view of it: one YAML fragment per `sources:` entry, plus the
// "rest" of the document (every other stanza). Built on the `yaml`
// package's Document API so comments survive the round trip — a comment
// written above a source stanza stays attached to that source, and
// comments in the non-source stanzas stay in `rest`.
//
// The table view edits fragments/rest; the raw view edits the whole
// text. `splitConfig` and `joinConfig` are the only bridge between the
// two representations.

import { Document, parseDocument, YAMLMap, YAMLSeq, isMap, isSeq } from "yaml";

export type SplitResult = {
  /// One YAML document per source entry, each a top-level map
  /// (`name: …` / `source: …`) — the seq item without its `- ` marker.
  sources: string[];
  /// The document minus the `sources:` key. "" when nothing else is set.
  rest: string;
};

export type SourceSummary = {
  name: string;
  /// `source.type` discriminator, "" when missing.
  type: string;
  enabled: boolean;
};

// Serialize a single node (with its attached comments) as its own
// document.
function nodeToString(node: unknown): string {
  const d = new Document();
  // Reuse the parsed node directly instead of round-tripping through
  // JS values — that's what preserves comments and formatting.
  d.contents = node as Document["contents"];
  return d.toString();
}

/// Parse the whole config text and split it. Throws Error with the YAML
/// parser's message when the text doesn't parse.
export function splitConfig(text: string): SplitResult {
  const doc = parseDocument(text);
  if (doc.errors.length > 0) {
    throw new Error(doc.errors[0].message);
  }
  const sources: string[] = [];
  const seq = doc.get("sources", true);
  if (isSeq(seq)) {
    // A comment between `sources:` and the first `- ` item is stored on
    // the seq, not the item; move it onto the first item so it travels
    // with that fragment (joinConfig renders it identically from there).
    const first = seq.items[0] as { commentBefore?: string } | undefined;
    if (seq.commentBefore && first) {
      first.commentBefore = first.commentBefore
        ? `${seq.commentBefore}\n${first.commentBefore}`
        : seq.commentBefore;
    }
    for (const item of seq.items) {
      sources.push(nodeToString(item));
    }
  }
  if (doc.contents !== null && isMap(doc.contents)) {
    doc.delete("sources");
    // A now-empty mapping would print as `{}`; the scaffold and the
    // table's "additional options" box both want "" for "nothing here".
    if (doc.contents.items.length === 0) {
      return { sources, rest: "" };
    }
  }
  const rest = doc.contents === null ? "" : doc.toString();
  return { sources, rest };
}

/// Validate one source fragment: it must parse as YAML and be a mapping
/// (`name: …`). Returns an error message, or null when fine. Full
/// semantic validation stays server-side (PUT /api/config).
export function fragmentError(fragment: string): string | null {
  const doc = parseDocument(fragment);
  if (doc.errors.length > 0) return doc.errors[0].message;
  if (doc.contents === null) return "empty source entry";
  if (!isMap(doc.contents)) return "a source entry must be a YAML mapping (name: …)";
  return null;
}

/// Best-effort summary of a fragment for the table row. Null when the
/// fragment doesn't parse into a map.
export function summarizeFragment(fragment: string): SourceSummary | null {
  const doc = parseDocument(fragment);
  if (doc.errors.length > 0 || !isMap(doc.contents)) return null;
  const js = doc.toJS() as {
    name?: unknown;
    enabled?: unknown;
    source?: { type?: unknown };
  };
  return {
    name: typeof js.name === "string" ? js.name : "",
    type: typeof js.source?.type === "string" ? js.source.type : "",
    enabled: js.enabled !== false,
  };
}

/// Reassemble the full config text from the rest-document and the source
/// fragments. Throws when `rest` or any fragment doesn't parse (with the
/// fragment's index in the message). The `sources:` key is placed after
/// the other stanzas.
export function joinConfig(rest: string, sources: string[]): string {
  const doc = parseDocument(rest);
  if (doc.errors.length > 0) {
    throw new Error(`additional config options: ${doc.errors[0].message}`);
  }
  if (doc.contents === null) {
    // Empty rest: start from an empty *block* mapping. (Parsing "{}"
    // instead would flag the whole document as flow-style and the added
    // `sources:` would print as `{ sources: [...] }`.)
    (doc as Document).contents = new YAMLMap();
  } else if (!isMap(doc.contents)) {
    throw new Error("additional config options must be YAML key: value stanzas");
  }
  const seq = new YAMLSeq();
  sources.forEach((frag, i) => {
    const fdoc = parseDocument(frag);
    if (fdoc.errors.length > 0) {
      throw new Error(`source ${i + 1}: ${fdoc.errors[0].message}`);
    }
    if (!isMap(fdoc.contents)) {
      throw new Error(`source ${i + 1}: must be a YAML mapping (name: …)`);
    }
    seq.items.push(fdoc.contents);
  });
  doc.set("sources", seq);
  return doc.toString();
}
