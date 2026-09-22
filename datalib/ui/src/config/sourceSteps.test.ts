// The wizard's attachment cap: it has to appear on new Slack sources
// and stay away from existing ones.

import { describe, expect, it } from "vitest";

import { catalogFor, type CatalogEntry, type Field } from "./catalog";
import {
  appendSource,
  buildDiffSource,
  buildStep,
  listGroups,
  listSteps,
  seedFieldValues,
  setGroupLoadRemoteImages,
  wireIntoFanIns,
  type ConfiguredStep,
  type FieldValues,
} from "./sourceSteps";

const SLACK = catalogFor("slack")!;
const CAP = "common.blob_size_limit_bytes";

/// A step as `listSteps` would return it, carrying `params`.
function step(params: Record<string, unknown>): ConfiguredStep {
  return {
    id: "slack/ingest",
    kind: "step",
    group: "slack",
    function: "ingest",
    name: "slack/ingest",
    phase: "ingest",
    type: "slack",
    inputs: [],
    params,
    start: 0,
    end: 0,
  };
}

function toml(values: FieldValues, entry: CatalogEntry = SLACK): string {
  return buildStep({ entry, group: "slack", phase: "download", values });
}

describe("the Slack attachment cap", () => {
  it("is declared, gated on attachments being on", () => {
    const field = SLACK.fields?.find((f) => f.target === CAP);
    expect(field, "slack should declare the cap").toBeDefined();
    expect(field!.kind).toBe("bytes");
    // Slack skips the blob path entirely when `media` is off, so a cap
    // written alongside `media = false` would be inert config.
    expect(field!.requires).toBe("api.media");
    expect((field as Field & { kind: "bytes" }).default).toBe(5_000_000);
  });

  it("defaults to 5 MB on a new source", () => {
    expect(seedFieldValues(SLACK)[CAP]).toBe("5 MB");
  });

  // Written as a person would write it, which the backend reads
  // (`datalib_source_common::byte_size`) — never as a count of bytes.
  it("writes the cap into the step's common table", () => {
    const out = toml(seedFieldValues(SLACK));
    expect(out).toContain("[steps.params.common]");
    expect(out).toContain('blob_size_limit_bytes = "5 MB"');
  });

  // The regression that matters. Before the create-only rule, opening
  // an uncapped source's form to change an unrelated field and saving
  // would have silently imposed 5 MB on it.
  it("leaves an existing uncapped source uncapped", () => {
    const existing = step({ api: { media: true, channels: ["general"] } });
    const seeded = seedFieldValues(SLACK, { ingest: existing });
    expect(seeded[CAP]).toBe("");
    expect(toml(seeded)).not.toContain("blob_size_limit_bytes");
  });

  it("round-trips a cap the config already sets, without snapping it to 5 MB", () => {
    const existing = step({ api: { media: true }, common: { blob_size_limit_bytes: 250 } });
    const seeded = seedFieldValues(SLACK, { ingest: existing });
    expect(seeded[CAP]).toBe("250 B");
    expect(toml(seeded)).toContain('blob_size_limit_bytes = "250 B"');
  });

  // A hand-written cap keeps its own spelling: "5000 KB" is not
  // rewritten to "5 MB" by a save that never touched it.
  it("keeps a hand-written cap as written", () => {
    const existing = step({ api: { media: true }, common: { blob_size_limit_bytes: "5000 KB" } });
    const seeded = seedFieldValues(SLACK, { ingest: existing });
    expect(seeded[CAP]).toBe("5000 KB");
    expect(toml(seeded)).toContain('blob_size_limit_bytes = "5000 KB"');
  });

  it("drops a cap it cannot read rather than writing it back", () => {
    const existing = step({ api: { media: true }, common: { blob_size_limit_bytes: "5 XB" } });
    expect(toml(seedFieldValues(SLACK, { ingest: existing }))).not.toContain("blob_size_limit");
  });

  // `requires` gates the write as well as the row, so turning
  // attachments off drops the cap rather than leaving a dangling knob.
  it("is not written when attachments are off", () => {
    const values = { ...seedFieldValues(SLACK), "api.media": false };
    const out = toml(values);
    expect(out).toContain("media = false");
    expect(out).not.toContain("blob_size_limit_bytes");
  });
});

describe("seedFieldValues", () => {
  // The asymmetry the `int` arm relies on: bool/select defaults mirror
  // the backend's own, so unlike an int default they seed on edit too.
  it("still seeds bool and select defaults while editing", () => {
    const seeded = seedFieldValues(SLACK, { ingest: step({ api: { channels: ["general"] } }) });
    expect(seeded["api.media"]).toBe(true);
    expect(seeded["api.dms"]).toBe(false);
  });

  // One form, two steps: a render field reads the render step's params
  // and an ingest field the ingest step's, so a knob with the same
  // spelling on both sides could never be read off the wrong one.
  it("reads each phase's fields off its own step", () => {
    const SIGNAL = catalogFor("signal")!;
    const ingest: ConfiguredStep = {
      ...step({ backup: { path: "~/backups" } }),
      id: "signal/ingest",
      group: "signal",
      type: "signal",
    };
    const render: ConfiguredStep = {
      ...ingest,
      id: "signal/render_markdown",
      function: "render_markdown",
      phase: "render",
      inputs: ["signal/ingest"],
      params: { period: "year" },
    };
    const seeded = seedFieldValues(SIGNAL, { ingest, render });
    expect(seeded["backup.path"]).toBe("~/backups");
    expect(seeded["period"]).toBe("year");
    // With no render step yet, the render field takes its default.
    expect(seedFieldValues(SIGNAL, { ingest })["period"]).toBe("month");
  });

  it("keeps an int a person cleared out of the form empty", () => {
    // `refresh_window_days` has no default, so it starts empty on
    // create — an int with no default must not pick one up.
    expect(seedFieldValues(SLACK)["api.refresh_window_days"]).toBe("");
  });
});

describe("a path field left empty", () => {
  const CLAUDE_CODE = catalogFor("claude_code")!;

  it("writes the bare method table, not an empty path", () => {
    // `sessions = {}` is a complete selection: the standard store. A
    // `path = ""` would send the ingest to the current directory.
    const out = buildStep({
      entry: CLAUDE_CODE,
      group: "claude-code",
      phase: "download",
      values: { "sessions.path": "" },
    });
    expect(out).toContain("sessions = {}");
    expect(out).not.toContain('path = ""');
  });

  it("writes the path once one is typed", () => {
    const out = buildStep({
      entry: CLAUDE_CODE,
      group: "claude-code",
      phase: "download",
      values: { "sessions.path": "/backups/claude-projects" },
    });
    expect(out).toContain("[steps.params.sessions]");
    expect(out).toContain('path = "/backups/claude-projects"');
    expect(out).not.toContain("sessions = {}");
  });
});

describe("buildDiffSource", () => {
  /// What "Compare…" writes reads back as a diff group with its one
  /// render step reading the source's ingest tree, and the fan-ins name
  /// the step like any render step's.
  it("writes a diff group the loader's rules accept", () => {
    const base = `[[groups]]
id = "slack"
type = "slack"

[[steps]]
group = "slack"
function = "ingest"
params.api = {}

[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = ["slack/render_markdown"]
`;
    const built = buildDiffSource({
      id: "slack-diff",
      name: "Slack, this week",
      source: "slack",
      from: "aaa",
      to: "bbb",
      maxDocuments: 50,
    });
    expect(built.renderId).toBe("slack-diff/render_markdown");
    let next = appendSource(base, `${built.groupBody}\n\n${built.stepsBody}`);
    next = wireIntoFanIns(next, built.renderId);
    const group = listGroups(next).find((g) => g.id === "slack-diff")!;
    expect(group.type).toBe("diff");
    expect(group.name).toBe("Slack, this week");
    const step = listSteps(next).find((s) => s.id === "slack-diff/render_markdown")!;
    expect(step.inputs).toEqual(["slack/ingest"]);
    expect(step.params).toEqual({ diff: { from: "aaa", to: "bbb", max_documents: 50 } });
    const fanIn = listSteps(next).find((s) => s.id === "unified_index/grid_index")!;
    expect(fanIn.inputs).toContain("slack-diff/render_markdown");
    expect(next).toContain('source = "slack"');
  });
});

describe("setGroupLoadRemoteImages", () => {
  const base = `[[groups]]
id = "mail"
name = "Fastmail"
type = "email"

[[groups]]
id = "slack"
type = "slack"

[[steps]]
group = "mail"
function = "ingest"
`;

  it("adds the switch under the group's id and removes it again", () => {
    const on = setGroupLoadRemoteImages(base, "mail", true);
    expect(on).toContain('id = "mail"\nload_remote_images = true\nname = "Fastmail"');
    // The other group and the steps are untouched.
    expect(on).toContain('[[groups]]\nid = "slack"\ntype = "slack"\n');
    expect(on).toContain('[[steps]]\ngroup = "mail"');
    // Off is the default, so it is said by absence.
    expect(setGroupLoadRemoteImages(on, "mail", false)).toBe(base);
  });

  it("replaces a switch already there rather than adding a second", () => {
    const twice = setGroupLoadRemoteImages(
      setGroupLoadRemoteImages(base, "mail", true),
      "mail",
      true,
    );
    expect(twice.match(/load_remote_images/g)).toHaveLength(1);
  });

  it("leaves the text alone for a group it cannot find", () => {
    expect(setGroupLoadRemoteImages(base, "nope", true)).toBe(base);
  });
});
