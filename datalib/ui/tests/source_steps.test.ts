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
  listConfiguredSources,
  paramsAreRepresentable,
  removeSource,
  replaceSource,
  unwireFromFanIns,
  wireIntoFanIns,
} from "../src/config/sourceSteps";
import { catalogFor } from "../src/config/catalog";

const SLACK = catalogFor("slack_api")!;
const LIGHTROOM = catalogFor("lightroom")!;

/** A config with one ordinary source plus both shared index steps. */
const TWO_STEP_SOURCE = `data_root = "~/datalib"

[[steps]]
id = "unified_index/grid"
command = "datalib-step grid_index"
inputs = [\"slack/rendered_md\"]

[[steps]]
id = "unified_index/qmd"
command = "datalib-step qmd_index"
inputs = [\"slack/rendered_md\"]

# ── slack ─────────────────────────────────────────────────────────────
[[steps]]
id = "slack/raw"
command = "datalib-step download slack_api"
[steps.params.sync]
channels = ["general"]
media = true

[[steps]]
id = "slack/rendered_md"
command = "datalib-step render slack_api"
inputs = ["slack/raw"]
`;

describe("listConfiguredSources", () => {
  it("groups a download/render pair into one source", () => {
    const slack = listConfiguredSources(TWO_STEP_SOURCE).find((s) => s.kind === "source")!;
    expect(slack.id).toBe("slack");
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
    const sources = entries.filter((e) => e.kind === "source").map((e) => e.id);
    const steps = entries.filter((e) => e.kind === "step").map((e) => e.id);
    expect(sources).toEqual(["slack"]);
    expect(steps).toEqual(["unified_index/grid", "unified_index/qmd"]);
    expect(entries.find((e) => e.id === "unified_index/grid")!.outputs).toEqual(["unified_index/grid"]);
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
    expect(applet.id).toBe("unified_index");
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

  // An applet id and a source id are separate namespaces, so the two
  // may coincide without either shadowing the other.
  it("keeps an applet and a source of the same id apart", () => {
    const clash = `${TWO_STEP_SOURCE}
[[applets]]
id = "slack"
command = "datalib-applet slack"
`;
    const clashing = listConfiguredSources(clash).filter((e) => e.id === "slack");
    expect(clashing.map((e) => e.kind)).toEqual(["source", "applet"]);
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
id = "custom/raw"
command = "my-exporter --flag"
`;
    const [source] = listConfiguredSources(custom);
    expect(source.kind).toBe("source");
    expect(source.id).toBe("custom");
    expect(source.type).toBeNull();
  });

  // A step writing somewhere that isn't a stanza is a step, and its own
  // id is what `--sync` would target.
  it("carries the step id for a non-source step", () => {
    const [step] = listConfiguredSources(`[[steps]]
id = "unified_index/grid"
command = "datalib-step grid_index"
inputs = [\"slack/rendered_md\"]
`);
    expect(step.kind).toBe("step");
    expect(step.stepId).toBe("unified_index/grid");
  });

  // Duplicate step ids are rejected by `validate_steps` on save, so
  // this can only arrive by hand-editing the file. It must not throw:
  // the grid still has to render, and the backend's own error banner
  // is what tells the user the config is unrunnable.
  it("survives duplicate step ids by collapsing them into one row", () => {
    const dupe = `[[steps]]
id = "slack/raw"
command = "datalib-step download slack_api"

[[steps]]
id = "slack/raw"
command = "datalib-step download slack_api"
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
    const body = buildStepPair(SLACK, "slack", "", {
      "sync.channels": ["general", "random"],
      "sync.media": true,
      "sync.all_channels": false,
      "sync.since": "",
    });
    expect(body).toContain('id = "slack/raw"');
    expect(body).toContain('id = "slack/rendered_md"');
    expect(body).toContain('channels = ["general", "random"]');
    // An empty optional stays out of the file rather than landing as "".
    expect(body).not.toContain("since");
  });

  // Download-only providers render nothing, so a render step would
  // declare an output no one writes.
  it("omits the render step for a download-only provider", () => {
    const body = buildStepPair(LIGHTROOM, "lightroom", "", {
      "common.input_path": "~/Pictures/cat.lrcat",
    });
    expect(body).toContain('id = "lightroom/raw"');
    expect(body).not.toContain("lightroom/rendered_md");
  });

  // TOML allows a super-table after its sub-table, but a generated file
  // people are meant to read shouldn't make them work that out.
  it("emits a parent table before its children", () => {
    const body = buildStepPair(LIGHTROOM, "lightroom", "", {
      "common.input_path": "~/Pictures/cat.lrcat",
      "skip_xmp": true,
    });
    expect(body.indexOf("[steps.params]")).toBeLessThan(body.indexOf("[steps.params.common]"));
  });

  // A bare `2026-01-01` is a TOML date; the providers validate a string.
  it("quotes dates", () => {
    const body = buildStepPair(SLACK, "slack", "", { "sync.since": "2026-01-01" });
    expect(body).toContain('since = "2026-01-01"');
  });
});

describe("removeSource / replaceSource", () => {
  it("takes the divider comment with the source", () => {
    const slack = listConfiguredSources(TWO_STEP_SOURCE).find((s) => s.kind === "source")!;
    const after = removeSource(TWO_STEP_SOURCE, slack);
    expect(after).not.toContain("── slack");
    expect(after).not.toContain('id = "slack/raw"');
    // The index steps and the top-level key are untouched.
    expect(after).toContain('id = "unified_index/grid"');
    expect(after).toContain('data_root = "~/datalib"');
  });

  /// Deleting a source is two edits, not one: its steps go, and so do
  /// the fan-in inputs naming them. An input pointing at a step that
  /// no longer exists is a config the runner refuses outright — so
  /// removing without unwiring produces a file that will not load.
  it("needs unwireFromFanIns to leave a loadable config", () => {
    const slack = listConfiguredSources(TWO_STEP_SOURCE).find((s) => s.kind === "source")!;
    const cut = removeSource(TWO_STEP_SOURCE, slack);
    expect(cut).toContain('"slack/rendered_md"');

    const clean = unwireFromFanIns(cut, "slack/rendered_md");
    expect(clean).not.toContain("slack");
    expect(clean).toContain('id = "unified_index/grid"');
  });

  /// The mirror: adding a source has to name it in the fan-ins, or it
  /// renders and is never indexed — invisible in search, with nothing
  /// on screen to say why.
  it("wireIntoFanIns adds an id once, to every index step", () => {
    const wired = wireIntoFanIns(TWO_STEP_SOURCE, "email/rendered_md");
    expect(wired.match(/"email\/rendered_md"/g)).toHaveLength(2);
    // Idempotent: re-saving an edit must not duplicate the entry.
    expect(wireIntoFanIns(wired, "email/rendered_md")).toBe(wired);
    // And it leaves the rest of the file alone.
    expect(wired).toContain('data_root = "~/datalib"');
    expect(wired).toContain("── slack");
  });

  it("leaves a config that still parses, with one fewer source", () => {
    const slack = listConfiguredSources(TWO_STEP_SOURCE).find((s) => s.kind === "source")!;
    const after = listConfiguredSources(removeSource(TWO_STEP_SOURCE, slack));
    // The index steps survive: deleting a source must not take the
    // shared pipeline with it.
    expect(after.map((e) => e.id)).toEqual(["unified_index/grid", "unified_index/qmd"]);
  });

  it("replaces in place rather than duplicating", () => {
    const slack = listConfiguredSources(TWO_STEP_SOURCE).find((s) => s.kind === "source")!;
    const body = buildStepPair(SLACK, "slack", "", { "sync.channels": ["random"] });
    const after = replaceSource(TWO_STEP_SOURCE, slack, body);
    const sources = listConfiguredSources(after).filter((s) => s.kind === "source");
    expect(sources.map((s) => s.id)).toEqual(["slack"]);
    expect(after).toContain('channels = ["random"]');
    expect(after).not.toContain('channels = ["general"]');
  });

  it("appends where a new source can safely go", () => {
    const body = buildStepPair(SLACK, "extra", "", {});
    const after = appendSource(TWO_STEP_SOURCE, body);
    expect(
      listConfiguredSources(after)
        .filter((s) => s.kind === "source")
        .map((s) => s.id)
        .sort(),
    ).toEqual(["extra", "slack"]);
  });
});

// `suggestId` — what stops "Add Data Source" producing a second `slack`
// the loader would reject — is covered in source_name.test.ts, next to
// the slugify rules that feed it.

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
