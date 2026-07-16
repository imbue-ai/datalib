import { describe, expect, it } from "vitest";
import { listSources } from "../src/config/configSources";

const FULL = `# Frankweiler config for this data root.
data_root: /tmp/data

defaults:
  blob_size_limit_bytes: 5000000

sources:
  # my main claude account
  - name: claude
    source:
      type: claude_api
      sync: {}
  - name: slack
    enabled: false
    source:
      type: slack_api
      sync:
        channels: ["general"]
`;

describe("listSources", () => {
  it("summarizes name/type/enabled per entry", () => {
    const rows = listSources(FULL);
    expect(rows).toHaveLength(2);
    expect(rows[0]).toMatchObject({ name: "claude", type: "claude_api", enabled: true });
    expect(rows[1]).toMatchObject({ name: "slack", type: "slack_api", enabled: false });
  });

  it("returns ranges that select the whole stanza", () => {
    const rows = listSources(FULL);
    const claude = FULL.slice(rows[0].start, rows[0].end);
    // Starts at the `- ` marker's line (indent included) and covers the
    // nested block.
    expect(claude.trimStart().startsWith("- name: claude")).toBe(true);
    expect(claude).toContain("type: claude_api");
    expect(claude).not.toContain("slack");
    const slack = FULL.slice(rows[1].start, rows[1].end);
    expect(slack.trimStart().startsWith("- name: slack")).toBe(true);
    expect(slack).toContain("enabled: false");
    expect(slack).toContain('channels: ["general"]');
  });

  it("handles empty, scaffold, and sourceless files", () => {
    expect(listSources("")).toEqual([]);
    expect(listSources("sources: []\n")).toEqual([]);
    expect(listSources("data_root: /x\n")).toEqual([]);
  });

  it("tolerates malformed entries without crashing", () => {
    const rows = listSources("sources:\n  - just a string\n  - name: ok\n    source: {type: perseus}\n");
    expect(rows).toHaveLength(2);
    expect(rows[0]).toMatchObject({ name: "", type: "", enabled: true });
    expect(rows[1]).toMatchObject({ name: "ok", type: "perseus" });
  });

  it("throws on unparseable YAML", () => {
    expect(() => listSources("a: [unclosed")).toThrow();
  });
});
