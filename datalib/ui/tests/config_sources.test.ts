import { describe, expect, it } from "vitest";
import { listSources } from "../src/config/configSources";

const FULL = `# Datalib config for this data root.
data_root = "/tmp/data"

[[steps]]
id = "unified_index/grid"
command = "datalib-step grid_index"
inputs = ["claude/rendered_md"]

# my main claude account
[[steps]]
id = "claude/raw"
command = "datalib-step download claude_api"
[steps.params]
sync = {}

[[steps]]
id = "claude/rendered_md"
command = "datalib-step render claude_api"
inputs = ["claude/raw"]

[[steps]]
id = "custom/out"
command = "my-exporter --flag"
`;

describe("listSources", () => {
  it("lists every step without inputs, by id", () => {
    const rows = listSources(FULL);
    // grid_index and claude.render declare inputs → infrastructure;
    // any input-less step is a source, whatever its command runs.
    expect(rows.map((r) => r.id)).toEqual(["claude/raw", "custom/out"]);
  });

  it("returns ranges that select the step entry", () => {
    const rows = listSources(FULL);
    const claude = FULL.slice(rows[0].start, rows[0].end);
    expect(claude.startsWith("[[steps]]")).toBe(true);
    // The range is widened past the step's own table to cover its
    // [steps.params] sub-table, which is a sibling in the document.
    expect(claude).toContain("sync = {}");
    expect(claude).not.toContain("claude/rendered_md");
    const custom = FULL.slice(rows[1].start, rows[1].end);
    expect(custom.startsWith("[[steps]]")).toBe(true);
    expect(custom).toContain("my-exporter --flag");
  });

  it("treats an empty inputs list as input-less", () => {
    const rows = listSources(
      '[[steps]]\nid = "x/raw"\ncommand = "fetch-x"\ninputs = []\noutputs = ["x/raw"]\n',
    );
    expect(rows.map((r) => r.id)).toEqual(["x/raw"]);
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
      'steps = [{id = "i/raw", command = "c", outputs = ["i/raw"]}]\n',
    );
    expect(rows.map((r) => r.id)).toEqual(["i/raw"]);
    expect(rows[0]).toMatchObject({ start: 0, end: 0 });
  });

  it("throws on unparseable TOML", () => {
    expect(() => listSources("a = [unclosed")).toThrow();
  });

  // TOML is the only format the app reads; a legacy config is a parse
  // error here, never a silently empty source list. Converting one is
  // `datalib-migrate-config`'s job, out of band.
  it("rejects a legacy YAML config rather than reading it as empty", () => {
    expect(() => listSources("steps:\n  - id: x\n    command: c\n")).toThrow();
    expect(() => listSources("sources:\n  - name: x\n")).toThrow();
  });
});
