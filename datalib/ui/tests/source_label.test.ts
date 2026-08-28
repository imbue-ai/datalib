// The malleable half of a source's identity.
//
// A source's *name* is its directory under the data root, the stem of
// its step ids, and the prefix inside every `qmd_path` the grid index
// recorded — so it is fixed once written. `label` is the part a person
// can rewrite at any time, and these tests pin the three properties
// that make that safe to offer:
//
//   1. it round-trips through the config text,
//   2. a source without one is displayed and written exactly as before,
//   3. a label containing TOML metacharacters still parses back.
import { describe, expect, it } from "vitest";
import {
  buildStepPair,
  listConfiguredSources,
  removeSource,
  appendSource,
} from "../src/config/sourceSteps";
import { catalogFor, type CatalogEntry } from "../src/config/catalog";

const SLACK = catalogFor("slack_api") as CatalogEntry;

const UNLABELLED = `data_root = "/tmp/data"

[[steps]]
id = "slack.download"
command = "datalib-step download slack_api"
outputs = ["slack/raw"]
[steps.params]
sync = {}

[[steps]]
id = "slack.render"
command = "datalib-step render slack_api"
inputs = ["slack/raw"]
outputs = ["slack/rendered_md"]
`;

const one = (text: string) => {
  const sources = listConfiguredSources(text);
  expect(sources).toHaveLength(1);
  return sources[0];
};

describe("reading a label", () => {
  it("falls back to the name when no step declares one", () => {
    const source = one(UNLABELLED);
    expect(source.name).toBe("slack");
    expect(source.label).toBe("slack");
  });

  it("takes the label off whichever step carries it", () => {
    const source = one(UNLABELLED.replace(
      'id = "slack.download"',
      'id = "slack.download"\nlabel = "Work Slack"',
    ));
    expect(source.name).toBe("slack");
    expect(source.label).toBe("Work Slack");
  });

  it("takes the first when both steps carry one", () => {
    const source = one(
      UNLABELLED.replace(
        'id = "slack.download"',
        'id = "slack.download"\nlabel = "Work Slack"',
      ).replace('id = "slack.render"', 'id = "slack.render"\nlabel = "Ignored"'),
    );
    expect(source.label).toBe("Work Slack");
  });

  it("ignores a blank label rather than showing an empty name", () => {
    const source = one(UNLABELLED.replace(
      'id = "slack.download"',
      'id = "slack.download"\nlabel = "   "',
    ));
    expect(source.label).toBe("slack");
  });
});

describe("writing a label", () => {
  /// Round-trip through the same splice the Edit button performs, so
  /// what's asserted is what the config file would actually hold.
  const save = (text: string, name: string, label: string) => {
    const source = listConfiguredSources(text).find((s) => s.name === name);
    const body = buildStepPair(SLACK, name, label, { "sync.media": true });
    return source
      ? appendSource(removeSource(text, source), body)
      : appendSource(text, body);
  };

  it("round-trips through the config text", () => {
    const next = save(UNLABELLED, "slack", "Work Slack");
    expect(next).toContain('label = "Work Slack"');
    expect(one(next).label).toBe("Work Slack");
    // The name is untouched: it is still the directory and the id stem.
    expect(one(next).name).toBe("slack");
    expect(next).toContain('outputs = ["slack/raw"]');
    expect(next).toContain('id = "slack.download"');
  });

  it("writes no key at all when there is nothing to say", () => {
    expect(save(UNLABELLED, "slack", "")).not.toContain("label =");
    // A label that only respells the directory name is not a label.
    expect(save(UNLABELLED, "slack", "slack")).not.toContain("label =");
    expect(save(UNLABELLED, "slack", "  slack  ")).not.toContain("label =");
  });

  it("clearing a label removes the key", () => {
    const labelled = save(UNLABELLED, "slack", "Work Slack");
    const cleared = save(labelled, "slack", "");
    expect(cleared).not.toContain("label =");
    expect(one(cleared).label).toBe("slack");
  });

  it("survives quotes, backslashes and a pasted newline", () => {
    const nasty = 'Thad\'s "work" \\ slack\nsecond line\ttabbed';
    const next = save(UNLABELLED, "slack", nasty);
    // The real assertion is that the file still parses — an unescaped
    // newline inside a TOML basic string would take the whole config
    // down, not just this key.
    expect(one(next).label).toBe(nasty);
  });

  it("lands on the download step, before its params tables", () => {
    const next = save(UNLABELLED, "slack", "Work Slack");
    expect(next.indexOf("label =")).toBeLessThan(next.indexOf("[steps.params"));
    expect(next.indexOf("label =")).toBeLessThan(next.indexOf('id = "slack.render"'));
  });
});
