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
  emptyTableDiagnosis,
  fieldIsActive,
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
    const slack = listConfiguredSources(TWO_STEP_SOURCE).find((s) => s.kind === "source")!;
    expect(slack.name).toBe("slack");
    expect(slack.type).toBe("slack_api");
    expect(slack.steps.map((s) => s.phase).sort()).toEqual(["download", "render"]);
    expect(slack.outputs.sort()).toEqual(["slack/raw", "slack/rendered_md"]);
  });

  // The aggregate index steps write under a reserved top-level
  // directory, not a stanza. They are steps in their own right — rows
  // in the grid — but calling one a *source* would put `unified_index`
  // in the source namespace and let the wizard collide with it.
  it("lists the index steps as steps, not sources", () => {
    const entries = listConfiguredSources(TWO_STEP_SOURCE);
    const sources = entries.filter((e) => e.kind === "source").map((e) => e.name);
    const steps = entries.filter((e) => e.kind === "step").map((e) => e.name);
    expect(sources).toEqual(["slack"]);
    expect(steps).toEqual(["grid_index", "qmd_index"]);
    expect(entries.find((e) => e.name === "grid_index")!.outputs).toEqual(["unified_index/grid"]);
  });

  // An applet is configured, so it is a row — but it owns no artifacts
  // and is never scheduled, which is what the kind has to carry.
  it("lists applets, with no outputs and no step id", () => {
    const withApplet = `${TWO_STEP_SOURCE}
[[applets]]
id = "unified_index"
command = "datalib-applet unified_index"
`;
    const applet = listConfiguredSources(withApplet).find((e) => e.kind === "applet")!;
    expect(applet.name).toBe("unified_index");
    expect(applet.type).toBe("unified_index");
    expect(applet.outputs).toEqual([]);
    expect(applet.stepId).toBeNull();
  });

  // Sources first: the table opens on what someone came to look at.
  it("orders sources before steps before applets", () => {
    const withApplet = `${TWO_STEP_SOURCE}
[[applets]]
id = "an_applet"
command = "datalib-applet slack"
`;
    expect(listConfiguredSources(withApplet).map((e) => e.kind)).toEqual([
      "source",
      "step",
      "step",
      "applet",
    ]);
  });

  // An applet id and a stanza name are separate namespaces, so the two
  // may coincide without either shadowing the other.
  it("keeps an applet and a source of the same name apart", () => {
    const clash = `${TWO_STEP_SOURCE}
[[applets]]
id = "slack"
command = "datalib-applet slack"
`;
    const named = listConfiguredSources(clash).filter((e) => e.name === "slack");
    expect(named.map((e) => e.kind)).toEqual(["source", "applet"]);
  });

  it("throws with the parser's message and line on malformed TOML", () => {
    expect(() => listConfiguredSources('[[steps]\nid = "x"')).toThrowError(/line \d+/);
  });

  it("returns nothing for a config that declares nothing", () => {
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
    expect(source.kind).toBe("source");
    expect(source.name).toBe("custom");
    expect(source.type).toBeNull();
  });

  // A step writing somewhere that isn't a stanza is a step, and its own
  // id is what `--sync` would target.
  it("carries the step id for a non-source step", () => {
    const [step] = listConfiguredSources(`[[steps]]
id = "grid_index"
command = "datalib-step grid_index"
inputs = ["**/rendered_md"]
outputs = ["unified_index/grid"]
`);
    expect(step.kind).toBe("step");
    expect(step.stepId).toBe("grid_index");
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
    const slack = listConfiguredSources(TWO_STEP_SOURCE).find((s) => s.kind === "source")!;
    expect(paramsAreRepresentable(slack, SLACK)).toEqual({ ok: true });
  });

  // The property the Edit button rests on: a knob no field models must
  // be *named*, so the grid can refuse rather than silently drop it.
  it("names the params it cannot model, phase included", () => {
    const withExtra = TWO_STEP_SOURCE.replace(
      "channels = [\"general\"]",
      "channels = [\"general\"]\n[steps.params.common.download_params]\nmaximum_sequential_failed_requests = 100",
    );
    const slack = listConfiguredSources(withExtra).find((s) => s.kind === "source")!;
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

  // Off is a real setting, and it is the backward-compatible one — a
  // config that omits `dms` gets DMs off, so writing it explicitly is
  // what makes the wizard's answer visible in the file.
  it("writes the direct-message switch even when it is off", () => {
    const body = buildStepPair(SLACK, "slack", { "sync.dms": false });
    expect(body).toContain("dms = false");
  });

  it("writes the DM allowlist when direct messages are on", () => {
    const body = buildStepPair(SLACK, "slack", {
      "sync.dms": true,
      "sync.dm_users": ["@riker", "Jean-Luc Picard"],
    });
    expect(body).toContain("dms = true");
    expect(body).toContain('dm_users = ["@riker", "Jean-Luc Picard"]');
  });

  // `SlackApiSync::validate` rejects `dm_users` with `dms = false`, so
  // a form that emitted it would write a config the backend refuses.
  // The gate has to drop the value, not just hide the input.
  it("drops a gated field whose switch is off", () => {
    const body = buildStepPair(SLACK, "slack", {
      "sync.dms": false,
      "sync.dm_users": ["@riker"],
    });
    expect(body).toContain("dms = false");
    expect(body).not.toContain("dm_users");
  });
});

describe("fieldIsActive", () => {
  const dmUsers = SLACK.fields!.find((f) => f.target === "sync.dm_users")!;
  const channels = SLACK.fields!.find((f) => f.target === "sync.channels")!;

  it("gates a field on its `requires` target", () => {
    expect(fieldIsActive(dmUsers, { "sync.dms": true })).toBe(true);
    expect(fieldIsActive(dmUsers, { "sync.dms": false })).toBe(false);
    // Unset reads as off, which is what a freshly opened form has.
    expect(fieldIsActive(dmUsers, {})).toBe(false);
  });

  it("leaves an ungated field alone", () => {
    expect(fieldIsActive(channels, {})).toBe(true);
  });
});

describe("removeSource / replaceSource", () => {
  it("takes the divider comment with the source", () => {
    const slack = listConfiguredSources(TWO_STEP_SOURCE).find((s) => s.kind === "source")!;
    const after = removeSource(TWO_STEP_SOURCE, slack);
    expect(after).not.toContain("slack");
    expect(after).not.toContain("── slack");
    // The index steps and the top-level key are untouched.
    expect(after).toContain('id = "grid_index"');
    expect(after).toContain('data_root = "~/datalib"');
  });

  it("leaves a config that still parses, with one fewer source", () => {
    const slack = listConfiguredSources(TWO_STEP_SOURCE).find((s) => s.kind === "source")!;
    const after = listConfiguredSources(removeSource(TWO_STEP_SOURCE, slack));
    // The index steps survive: deleting a source must not take the
    // shared pipeline with it.
    expect(after.map((e) => e.name)).toEqual(["grid_index", "qmd_index"]);
  });

  it("replaces in place rather than duplicating", () => {
    const slack = listConfiguredSources(TWO_STEP_SOURCE).find((s) => s.kind === "source")!;
    const body = buildStepPair(SLACK, "slack", { "sync.channels": ["random"] });
    const after = replaceSource(TWO_STEP_SOURCE, slack, body);
    const sources = listConfiguredSources(after).filter((s) => s.kind === "source");
    expect(sources.map((s) => s.name)).toEqual(["slack"]);
    expect(after).toContain('channels = ["random"]');
    expect(after).not.toContain('channels = ["general"]');
  });

  it("appends where a new source can safely go", () => {
    const body = buildStepPair(SLACK, "extra", {});
    const after = appendSource(TWO_STEP_SOURCE, body);
    expect(
      listConfiguredSources(after)
        .filter((s) => s.kind === "source")
        .map((s) => s.name)
        .sort(),
    ).toEqual(["extra", "slack"]);
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

// Reported from the desktop app: Manager2's table empty against a
// config the backend reads 15 sources from, with no error shown. The
// same backend binary and the same data root render all 15 in a
// browser, so the disagreement is on the browser side — and a silent
// empty table gives an investigation nothing to work with.
describe("emptyTableDiagnosis", () => {
  const base = {
    parsedCount: 0,
    serverSourceCount: 0,
    textLength: 0,
    exists: true,
    path: "/root/config.toml",
  };

  it("says nothing when the table has rows", () => {
    expect(emptyTableDiagnosis({ ...base, parsedCount: 3, serverSourceCount: 3 })).toBeNull();
  });

  // The ordinary case: a fresh root. That gets the friendly empty
  // state, not an alarm.
  it("says nothing when the config genuinely declares nothing", () => {
    expect(emptyTableDiagnosis({ ...base, textLength: 42 })).toBeNull();
  });

  // The reported symptom. Both counts come from the same file, so they
  // cannot legitimately disagree.
  it("calls out a table that parsed none of what the server counted", () => {
    const why = emptyTableDiagnosis({
      ...base,
      serverSourceCount: 15,
      textLength: 22309,
    });
    expect(why).toContain("15 sources");
    expect(why).toContain("22309 characters");
    expect(why).toContain("/root/config.toml");
    expect(why).toContain("bug in this table");
  });

  // The other way the table can go silently empty: the file is there
  // but its text never arrived, which `get_config` turns into an empty
  // string via `unwrap_or_default`.
  it("calls out a config that arrived empty despite existing", () => {
    const why = emptyTableDiagnosis({ ...base, exists: true, textLength: 0 });
    expect(why).toContain("arrived empty");
    expect(why).toContain("did not reach this page");
  });

  // A root with no config file yet is the fresh-install path, and the
  // scaffold covers it — no alarm.
  it("says nothing when there is no config file at all", () => {
    expect(emptyTableDiagnosis({ ...base, exists: false })).toBeNull();
  });
});
