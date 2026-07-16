import { describe, expect, it } from "vitest";
import {
  fragmentError,
  joinConfig,
  splitConfig,
  summarizeFragment,
} from "../src/config/configSplit";

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

describe("splitConfig", () => {
  it("splits sources from the rest", () => {
    const { sources, rest } = splitConfig(FULL);
    expect(sources).toHaveLength(2);
    expect(sources[0]).toContain("name: claude");
    expect(sources[0]).toContain("type: claude_api");
    // Comment above the entry stays attached to the fragment.
    expect(sources[0]).toContain("# my main claude account");
    expect(sources[1]).toContain("enabled: false");
    // Rest keeps the other stanzas (and the file header comment) but
    // not the sources.
    expect(rest).toContain("data_root: /tmp/data");
    expect(rest).toContain("blob_size_limit_bytes");
    expect(rest).toContain("# Frankweiler config for this data root.");
    expect(rest).not.toContain("sources:");
    expect(rest).not.toContain("claude");
  });

  it("handles a sources-only file with empty rest", () => {
    const { sources, rest } = splitConfig("sources:\n  - name: a\n    source:\n      type: perseus\n");
    expect(sources).toHaveLength(1);
    expect(rest).toBe("");
  });

  it("handles empty and scaffold files", () => {
    expect(splitConfig("")).toEqual({ sources: [], rest: "" });
    expect(splitConfig("sources: []\n")).toEqual({ sources: [], rest: "" });
  });

  it("throws on unparseable YAML", () => {
    expect(() => splitConfig("a: [unclosed")).toThrow();
  });
});

describe("joinConfig", () => {
  it("round-trips content and comments through split + join", () => {
    const { sources, rest } = splitConfig(FULL);
    const joined = joinConfig(rest, sources);
    const again = splitConfig(joined);
    expect(again.sources).toEqual(sources);
    expect(again.rest).toEqual(rest);
    expect(joined).toContain("# my main claude account");
    expect(joined).toContain("data_root: /tmp/data");
  });

  it("joins with an empty rest", () => {
    const joined = joinConfig("", ["name: a\nsource:\n  type: perseus\n"]);
    expect(joined).toContain("sources:");
    expect(joined).toContain("- name: a");
    // No flow-style artifacts from the empty rest.
    expect(joined).not.toContain("{}");
  });

  it("joins with no sources", () => {
    const joined = joinConfig("data_root: /x\n", []);
    expect(joined).toContain("data_root: /x");
    expect(joined).toContain("sources: []");
  });

  it("reports which fragment is broken", () => {
    expect(() => joinConfig("", ["name: ok\nsource: {type: perseus}", ": ["]))
      .toThrow(/source 2/);
  });

  it("rejects a non-mapping rest", () => {
    expect(() => joinConfig("- just\n- a list\n", [])).toThrow(/additional config/);
  });
});

describe("fragmentError", () => {
  it("accepts a mapping", () => {
    expect(fragmentError("name: x\nsource:\n  type: perseus\n")).toBeNull();
  });
  it("rejects empty / non-map / broken fragments", () => {
    expect(fragmentError("")).toBeTruthy();
    expect(fragmentError("- a list item")).toBeTruthy();
    expect(fragmentError("a: [unclosed")).toBeTruthy();
  });
});

describe("summarizeFragment", () => {
  it("summarizes name/type/enabled", () => {
    expect(
      summarizeFragment("name: slack\nenabled: false\nsource:\n  type: slack_api\n"),
    ).toEqual({ name: "slack", type: "slack_api", enabled: false });
    expect(summarizeFragment("name: a\nsource: {type: perseus}")).toEqual({
      name: "a",
      type: "perseus",
      enabled: true,
    });
  });
  it("returns null for broken fragments", () => {
    expect(summarizeFragment(": [")).toBeNull();
  });
});
