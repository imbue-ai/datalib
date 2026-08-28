// Tests for the per-source view of a DAG config that drives the
// Manager2 grid, and for the text it writes back.
//
// The cases that matter most here are the malformed ones. The grid is
// derived from a file anyone can hand-edit (or an agent can PUT), so
// "what does this do with a config that is wrong in some particular
// way" is the question, not the happy path.

import { describe, expect, it } from "vitest";
import {
  appendSource,
  buildStepPair,
  listConfiguredSources,
  paramsAreRepresentable,
  removeSource,
  replaceSource,
  suggestName,
} from "../src/config/sourceSteps";
import { catalogFor } from "../src/config/catalog";

const SLACK = catalogFor("slack_api")!;
const LIGHTROOM = catalogFor("lightroom")!;

/** A config with one ordinary source plus both shared index steps. */
const TWO_STEP_SOURCE = `data_root = "~/datalib"

[[steps]]
id = "grid_index"
command = "datalib-step grid_index"
inputs = ["**/rendered_md"]
outputs = ["unified_index/grid"]

[[steps]]
id = "qmd_index"
command = "datalib-step qmd_index"
inputs = ["**/rendered_md"]
outputs = ["unified_index/qmd"]

# ── slack ─────────────────────────────────────────────────────────────
[[steps]]
id = "slack.download"
command = "datalib-step download slack_api"
outputs = ["slack/raw"]
[steps.params.sync]
channels = ["general"]
media = true

[[steps]]
id = "slack.render"
command = "datalib-step render slack_api"
inputs = ["slack/raw"]
outputs = ["slack/rendered_md"]
`;

describe("listConfiguredSources", () => {
  it("groups a download/render pair into one source", () => {
    const sources = listConfiguredSources(TWO_STEP_SOURCE);
    expect(sources.map((s) => s.name)).toEqual(["slack"]);
    expect(sources[0].type).toBe("slack_api");
    expect(sources[0].steps.map((s) => s.phase).sort()).toEqual(["download", "render"]);
  });

  // The aggregate index steps write under a reserved top-level
  // directory, not a stanza. Counting them as sources would put
  // `unified_index` in the grid with a Delete button next to it.
  it("does not mistake the index steps for sources", () => {
    const names = listConfiguredSources(TWO_STEP_SOURCE).map((s) => s.name);
    expect(names).not.toContain("unified_index");
  });

  it("ignores applets, which declare no artifacts", () => {
    const withApplet = `${TWO_STEP_SOURCE}
[[applets]]
id = "unified_index"
command = "datalib-applet unified_index"
`;
    expect(listConfiguredSources(withApplet).map((s) => s.name)).toEqual(["slack"]);
  });

  it("throws with the parser's message and line on malformed TOML", () => {
    expect(() => listConfiguredSources('[[steps]\nid = "x"')).toThrowError(/line \d+/);
  });

  it("returns nothing for a config with no steps at all", () => {
    expect(listConfiguredSources('data_root = "~/datalib"')).toEqual([]);
  });

  // A step whose command isn't a `datalib-step download|render` — any
  // executable may be a step — has no catalog entry, and the grid has
  // to cope rather than crash.
  it("reports a null type for a custom executable", () => {
    const custom = `[[steps]]
id = "custom.download"
command = "my-exporter --flag"
outputs = ["custom/raw"]
`;
    const [source] = listConfiguredSources(custom);
    expect(source.name).toBe("custom");
    expect(source.type).toBeNull();
  });

  // Duplicate step ids are rejected by `validate_steps` on save, so
  // this can only arrive by hand-editing the file. It must not throw:
  // the grid still has to render, and the backend's own error banner
  // is what tells the user the config is unrunnable.
  it("survives duplicate step ids by collapsing them into one row", () => {
    const dupe = `[[steps]]
id = "slack.download"
command = "datalib-step download slack_api"
outputs = ["slack/raw"]

[[steps]]
id = "slack.download"
command = "datalib-step download slack_api"
outputs = ["slack/raw"]
`;
    const sources = listConfiguredSources(dupe);
    expect(sources.map((s) => s.name)).toEqual(["slack"]);
    expect(sources[0].steps).toHaveLength(2);
  });
});

describe("paramsAreRepresentable", () => {
  it("accepts params the descriptor models", () => {
    const [slack] = listConfiguredSources(TWO_STEP_SOURCE);
    expect(paramsAreRepresentable(slack, SLACK)).toEqual({ ok: true });
  });

  // The property the Edit button rests on: a knob no field models must
  // be *named*, so the grid can refuse rather than silently drop it.
  it("names the params it cannot model, phase included", () => {
    const withExtra = TWO_STEP_SOURCE.replace(
      "channels = [\"general\"]",
      "channels = [\"general\"]\n[steps.params.common.download_params]\nmaximum_sequential_failed_requests = 100",
    );
    const [slack] = listConfiguredSources(withExtra);
    const verdict = paramsAreRepresentable(slack, SLACK);
    expect(verdict.ok).toBe(false);
    if (!verdict.ok) {
      expect(verdict.unknown).toContain(
        "download.common.download_params.maximum_sequential_failed_requests",
      );
    }
  });
});

describe("buildStepPair", () => {
  it("writes a download/render pair with the params it was given", () => {
    const body = buildStepPair(SLACK, "slack", {
      "sync.channels": ["general", "random"],
      "sync.media": true,
      "sync.all_channels": false,
      "sync.since": "",
    });
    expect(body).toContain('id = "slack.download"');
    expect(body).toContain('id = "slack.render"');
    expect(body).toContain('channels = ["general", "random"]');
    // An empty optional stays out of the file rather than landing as "".
    expect(body).not.toContain("since");
  });

  // Download-only providers render nothing, so a render step would
  // declare an output no one writes.
  it("omits the render step for a download-only provider", () => {
    const body = buildStepPair(LIGHTROOM, "lightroom", {
      "common.input_path": "~/Pictures/cat.lrcat",
    });
    expect(body).toContain('id = "lightroom.download"');
    expect(body).not.toContain("lightroom.render");
  });

  // TOML allows a super-table after its sub-table, but a generated file
  // people are meant to read shouldn't make them work that out.
  it("emits a parent table before its children", () => {
    const body = buildStepPair(LIGHTROOM, "lightroom", {
      "common.input_path": "~/Pictures/cat.lrcat",
      "skip_xmp": true,
    });
    expect(body.indexOf("[steps.params]")).toBeLessThan(body.indexOf("[steps.params.common]"));
  });

  // A bare `2026-01-01` is a TOML date; the providers validate a string.
  it("quotes dates", () => {
    const body = buildStepPair(SLACK, "slack", { "sync.since": "2026-01-01" });
    expect(body).toContain('since = "2026-01-01"');
  });
});

describe("removeSource / replaceSource", () => {
  it("takes the divider comment with the source", () => {
    const [slack] = listConfiguredSources(TWO_STEP_SOURCE);
    const after = removeSource(TWO_STEP_SOURCE, slack);
    expect(after).not.toContain("slack");
    expect(after).not.toContain("── slack");
    // The index steps and the top-level key are untouched.
    expect(after).toContain('id = "grid_index"');
    expect(after).toContain('data_root = "~/datalib"');
  });

  it("leaves a config that still parses, with one fewer source", () => {
    const [slack] = listConfiguredSources(TWO_STEP_SOURCE);
    expect(listConfiguredSources(removeSource(TWO_STEP_SOURCE, slack))).toEqual([]);
  });

  it("replaces in place rather than duplicating", () => {
    const [slack] = listConfiguredSources(TWO_STEP_SOURCE);
    const body = buildStepPair(SLACK, "slack", { "sync.channels": ["random"] });
    const after = replaceSource(TWO_STEP_SOURCE, slack, body);
    const sources = listConfiguredSources(after);
    expect(sources.map((s) => s.name)).toEqual(["slack"]);
    expect(after).toContain('channels = ["random"]');
    expect(after).not.toContain('channels = ["general"]');
  });

  it("appends where a new source can safely go", () => {
    const body = buildStepPair(SLACK, "extra", {});
    const after = appendSource(TWO_STEP_SOURCE, body);
    expect(listConfiguredSources(after).map((s) => s.name).sort()).toEqual(["extra", "slack"]);
  });
});

describe("suggestName", () => {
  it("keeps the default when it is free", () => {
    expect(suggestName(new Set(), "slack")).toBe("slack");
  });

  // What stops "Add Data Source" producing a second `slack` that the
  // loader would then reject.
  it("suffixes past every taken name", () => {
    expect(suggestName(new Set(["slack"]), "slack")).toBe("slack-2");
    expect(suggestName(new Set(["slack", "slack-2"]), "slack")).toBe("slack-3");
  });
});
