import { describe, expect, it } from "vitest";
import { listSources } from "../src/config/configSources";

const FULL = `# Datalib config for this data root.
data_root = "/tmp/data"

[[steps]]
id = "unified_index/grid_index"
command = "index-it"
inputs = ["claude/render_markdown"]

# my main claude account
[[steps]]
id = "claude/ingest"
command = "fetch-claude"
[steps.params]
sync = {}

[[steps]]
id = "claude/render_markdown"
command = "render-claude"
inputs = ["claude/ingest"]

[[steps]]
id = "custom/out"
command = "my-exporter --flag"
`;

describe("listSources", () => {
  it("lists every step without inputs, by id", () => {
    const rows = listSources(FULL);
    // grid_index and claude.render declare inputs → infrastructure;
    // any input-less step is a source, whatever its command runs.
    expect(rows.map((r) => r.id)).toEqual(["claude/ingest", "custom/out"]);
  });

  it("returns ranges that select the step entry", () => {
    const rows = listSources(FULL);
    const claude = FULL.slice(rows[0].start, rows[0].end);
    expect(claude.startsWith("[[steps]]")).toBe(true);
    // The range is widened past the step's own table to cover its
    // [steps.params] sub-table, which is a sibling in the document.
    expect(claude).toContain("sync = {}");
    expect(claude).not.toContain("claude/render_markdown");
    const custom = FULL.slice(rows[1].start, rows[1].end);
    expect(custom.startsWith("[[steps]]")).toBe(true);
    expect(custom).toContain("my-exporter --flag");
  });

  /// The shape the wizard and the chips write: the step's id is composed
  /// from `group` + `function`, and the `[[groups]]` table before it is
  /// not part of the row's range.
  it("lists a grouped step under its composed id", () => {
    const text =
      '[[groups]]\nid = "perseus"\ntype = "perseus"\n\n' +
      '[[steps]]\ngroup = "perseus"\nfunction = "ingest"\ncommand = "c"\n\n' +
      '[[steps]]\ngroup = "perseus"\nfunction = "render_markdown"\ncommand = "c"\ninputs = ["perseus/ingest"]\n';
    const rows = listSources(text);
    expect(rows.map((r) => r.id)).toEqual(["perseus/ingest"]);
    const range = text.slice(rows[0].start, rows[0].end);
    expect(range.startsWith("[[steps]]")).toBe(true);
    expect(range).not.toContain("[[groups]]");
  });

  it("treats an empty inputs list as input-less", () => {
    const rows = listSources(
      '[[steps]]\nid = "x/ingest"\ncommand = "fetch-x"\ninputs = []\n',
    );
    expect(rows.map((r) => r.id)).toEqual(["x/ingest"]);
  });

  it("handles empty, scaffold, and stepless files", () => {
    expect(listSources("")).toEqual([]);
    expect(listSources("steps = []\n")).toEqual([]);
    expect(listSources('data_root = "/x"\n')).toEqual([]);
  });

  it("tolerates malformed entries without crashing", () => {
    // An inline step has no table of its own, so it lists with a zero
    // range ("not locatable") rather than pointing at something else.
    const rows = listSources(
      'steps = [{id = "i/ingest", command = "c"}]\n',
    );
    expect(rows.map((r) => r.id)).toEqual(["i/ingest"]);
    expect(rows[0]).toMatchObject({ start: 0, end: 0 });
  });

  it("throws on unparseable TOML", () => {
    expect(() => listSources("a = [unclosed")).toThrow();
  });

  // TOML is the only format the app reads; a pre-TOML config is a parse
  // error here, never a silently empty source list.
  it("rejects a legacy YAML config rather than reading it as empty", () => {
    expect(() => listSources("steps:\n  - id: x\n    command: c\n")).toThrow();
    expect(() => listSources("sources:\n  - name: x\n")).toThrow();
  });
});
