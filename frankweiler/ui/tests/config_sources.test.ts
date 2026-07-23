import { describe, expect, it } from "vitest";
import { listSources } from "../src/config/configSources";

const FULL = `# Frankweiler config for this data root.
data_root: /tmp/data

steps:
  - id: grid_index
    command: datalib-step-grid_index
    inputs: ["**/rendered_md"]
    outputs: [system/backend_index]

  # my main claude account
  - id: claude.download
    command: datalib-step-download-claude_api
    outputs: [claude/raw]
    params:
      sync: {}
  - id: claude.render
    command: datalib-step-render-claude_api
    inputs: [claude/raw]
    outputs: [claude/rendered_md]

  - id: custom
    command: my-exporter --flag
    outputs: [custom/out]
`;

describe("listSources", () => {
  it("lists every step without inputs, by id", () => {
    const rows = listSources(FULL);
    // grid_index and claude.render declare inputs → infrastructure;
    // any input-less step is a source, whatever its command runs.
    expect(rows.map((r) => r.id)).toEqual(["claude.download", "custom"]);
  });

  it("returns ranges that select the step entry", () => {
    const rows = listSources(FULL);
    const claude = FULL.slice(rows[0].start, rows[0].end);
    // Starts at the `- ` marker's line (indent included) and covers
    // the nested block.
    expect(claude.trimStart().startsWith("- id: claude.download")).toBe(true);
    expect(claude).toContain("sync: {}");
    expect(claude).not.toContain("claude.render");
    const custom = FULL.slice(rows[1].start, rows[1].end);
    expect(custom.trimStart().startsWith("- id: custom")).toBe(true);
    expect(custom).toContain("my-exporter --flag");
  });

  it("treats an empty inputs list as input-less", () => {
    const rows = listSources(
      "steps:\n  - id: x\n    command: fetch-x\n    inputs: []\n    outputs: [x/raw]\n",
    );
    expect(rows.map((r) => r.id)).toEqual(["x"]);
  });

  it("handles empty, scaffold, and stepless files", () => {
    expect(listSources("")).toEqual([]);
    expect(listSources("steps: []\n")).toEqual([]);
    expect(listSources("data_root: /x\n")).toEqual([]);
  });

  it("tolerates malformed entries without crashing", () => {
    const rows = listSources(
      "steps:\n  - just a string\n  - id: ok\n    command: fetch-ok\n    outputs: [ok/raw]\n",
    );
    expect(rows.map((r) => r.id)).toEqual(["ok"]);
  });

  it("throws on unparseable YAML", () => {
    expect(() => listSources("a: [unclosed")).toThrow();
  });
});
