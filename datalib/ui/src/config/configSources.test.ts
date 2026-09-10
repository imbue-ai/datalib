// What the Sources table calls a source, and the one thing it must not.

import { describe, expect, it } from "vitest";
import { listSources } from "./configSources";

/// What `POST /api/config/init` writes into a fresh root, trimmed to
/// the entries that decide this. Every real data root starts here, so
/// "a scaffolded root has no sources" is the case that matters most.
const SCAFFOLD = `
[[groups]]
id = "unified_index"
name = "Unified Index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = []

[[steps]]
group = "unified_index"
function = "qmd_index"
inputs = []

[[applets]]
group = "unified_index"
id = "unified_index"
command = "datalib-applet unified_index"
`;

describe("listSources", () => {
  /// The index steps declare `inputs = []` because nothing feeds them
  /// until a source exists — so the fringe rule alone calls them
  /// sources, and every root ever created would open claiming two.
  /// The backend already excludes them (`configured_source_count`);
  /// this is the same test on the other side.
  it("counts no sources in a scaffolded root", () => {
    expect(listSources(SCAFFOLD)).toEqual([]);
  });

  it("counts a real source beside them", () => {
    const rows = listSources(`${SCAFFOLD}
[[groups]]
id = "slack"
type = "slack"

[[steps]]
group = "slack"
function = "ingest"

[[steps]]
group = "slack"
function = "render_markdown"
inputs = ["slack/ingest"]
`);
    expect(rows.map((r) => r.id)).toEqual(["slack/ingest"]);
  });

  /// A step that reads another is downstream, not a source — the rule
  /// this file has always had, kept honest alongside the new one.
  it("excludes a step with inputs", () => {
    const rows = listSources(`
[[steps]]
id = "a"
command = "x"

[[steps]]
id = "b"
command = "y"
inputs = ["a"]
`);
    expect(rows.map((r) => r.id)).toEqual(["a"]);
  });

  /// Only the index group is special-cased, and by prefix — a source
  /// whose name merely starts with the same letters is still a source.
  it("does not swallow a source with a similar name", () => {
    const rows = listSources(`
[[steps]]
group = "unified_index_notes"
function = "ingest"
`);
    expect(rows.map((r) => r.id)).toEqual(["unified_index_notes/ingest"]);
  });
});
