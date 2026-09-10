// The wizard's attachment cap: it has to appear on new Slack sources
// and stay away from existing ones.

import { describe, expect, it } from "vitest";

import { catalogFor, type CatalogEntry, type Field } from "./catalog";
import {
  buildStep,
  seedFieldValues,
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
    expect(field!.kind).toBe("int");
    // Slack skips the blob path entirely when `media` is off, so a cap
    // written alongside `media = false` would be inert config.
    expect(field!.requires).toBe("api.media");
    expect((field as Field & { kind: "int" }).default).toBe(5_000_000);
  });

  it("defaults to 5 MB on a new source", () => {
    expect(seedFieldValues(SLACK)[CAP]).toBe(5_000_000);
  });

  it("writes the cap into the step's common table", () => {
    const out = toml(seedFieldValues(SLACK));
    expect(out).toContain("[steps.params.common]");
    expect(out).toContain("blob_size_limit_bytes = 5000000");
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
    expect(seeded[CAP]).toBe(250);
    expect(toml(seeded)).toContain("blob_size_limit_bytes = 250");
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
