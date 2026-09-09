// `listSteps` and the splice-based writers behind the Pipeline table.
//
// The table is one row per step — no grouping yet. A fetch step and the
// render step that reads it are two rows, two forms and two things to
// run; what relates them is the `[[groups]]` entry they are both filed
// under, which is where the source's name and type live.
import { describe, expect, it } from "vitest";
import {
  appendSource,
  buildGroup,
  buildStep,
  fieldIsActive,
  listGroups,
  listSteps,
  paramsAreRepresentable,
  phaseOf,
  removeSteps,
  renameGroup,
  renderIdFor,
  replaceStep,
  stemOf,
  unwireFromFanIns,
  wireIntoFanIns,
} from "../src/config/sourceSteps";
import { catalogFor } from "../src/config/catalog";

const SLACK = catalogFor("slack_api")!;
const LIGHTROOM = catalogFor("lightroom")!;
const SIGNAL = catalogFor("signal_backup")!;

/** One source's group and two steps plus the index group and its steps. */
const PAIR = `data_root = "~/datalib"

[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid"
command = "datalib-step grid_index"
inputs = ["slack/rendered_md"]

[[steps]]
group = "unified_index"
function = "qmd"
command = "datalib-step qmd_index"
inputs = ["slack/rendered_md"]

# ── slack ─────────────────────────────────────────────────────────────
[[groups]]
id = "slack"
name = "Work Slack"
type = "slack_api"

[[steps]]
group = "slack"
function = "raw"
command = "datalib-step download slack_api"
[steps.params.sync]
channels = ["general"]

[[steps]]
group = "slack"
function = "rendered_md"
command = "datalib-step render slack_api"
inputs = ["slack/raw"]
`;

describe("listSteps", () => {
  it("gives every step its own row, in file order, under its composed id", () => {
    expect(listSteps(PAIR).map((s) => s.id)).toEqual([
      "unified_index/grid",
      "unified_index/qmd",
      "slack/raw",
      "slack/rendered_md",
    ]);
  });

  // The distinction the Kind column shows, and what gates every row
  // action. Derived from the id's shape and nothing else.
  it("classifies a step by the shape of its id", () => {
    expect(phaseOf("slack/raw")).toBe("fetch");
    expect(phaseOf("slack/rendered_md")).toBe("render");
    expect(phaseOf("unified_index/grid")).toBe("index");
    expect(phaseOf("unified_index/qmd")).toBe("index");
    // A custom executable writing its own tree is a step and nothing more.
    expect(phaseOf("exports/csv")).toBe("other");
    expect(phaseOf("solo")).toBe("other");
  });

  it("reads group, function, type and inputs off each step", () => {
    const by = new Map(listSteps(PAIR).map((s) => [s.id, s]));
    const fetch = by.get("slack/raw")!;
    expect(fetch.group).toBe("slack");
    expect(fetch.function).toBe("raw");
    // The type comes from the group, not from the command.
    expect(fetch.type).toBe("slack_api");
    expect(fetch.inputs).toEqual([]);
    expect(by.get("slack/rendered_md")!.inputs).toEqual(["slack/raw"]);
    expect(by.get("unified_index/grid")!.type).toBeNull();
  });

  /// The group's name labels its fetch step; its render step is the same
  /// name said again, so the two rows still read as one source.
  it("labels grouped steps from the group's name", () => {
    const by = new Map(listSteps(PAIR).map((s) => [s.id, s]));
    expect(by.get("slack/raw")!.name).toBe("Work Slack");
    expect(by.get("slack/rendered_md")!.name).toBe("Work Slack (render markdown)");
    // The index group has no name, so its steps take the shared defaults.
    expect(by.get("unified_index/grid")!.name).toBe("Unified Index (table)");
  });

  it("lists applets, after the steps, with no inputs", () => {
    const withApplet = `${PAIR}
[[applets]]
group = "unified_index"
id = "unified_index"
command = "datalib-applet unified_index"
`;
    const entries = listSteps(withApplet);
    expect(entries.at(-1)!.kind).toBe("applet");
    expect(entries.at(-1)!.id).toBe("unified_index");
    expect(entries.at(-1)!.group).toBe("unified_index");
    expect(entries.at(-1)!.type).toBe("unified_index");
    expect(entries.at(-1)!.inputs).toEqual([]);
  });

  it("throws with the parser's message and line on malformed TOML", () => {
    expect(() => listSteps('[[steps]\nid = "x"')).toThrowError(/line \d+/);
  });

  it("returns nothing for a config that declares nothing", () => {
    expect(listSteps('data_root = "~/datalib"')).toEqual([]);
  });

  // Any executable may be a step, so a command the catalog doesn't know
  // has to produce a row rather than a crash — and a step outside any
  // group keeps the id it wrote.
  it("reports a null type for a custom executable", () => {
    const [step] = listSteps('[[steps]]\nid = "custom/out"\ncommand = "my-exporter --flag"\n');
    expect(step.id).toBe("custom/out");
    expect(step.group).toBeNull();
    expect(step.phase).toBe("other");
    expect(step.type).toBeNull();
    expect(step.name).toBe("custom/out");
  });
});

describe("listGroups", () => {
  it("lists every group with its name, type and range", () => {
    const groups = listGroups(PAIR);
    expect(groups.map((g) => g.id)).toEqual(["unified_index", "slack"]);
    expect(groups[0]).toMatchObject({ name: null, type: null });
    expect(groups[1]).toMatchObject({ name: "Work Slack", type: "slack_api" });
    expect(PAIR.slice(groups[1].start, groups[1].end)).toContain('type = "slack_api"');
  });
});

describe("stems and siblings", () => {
  it("stemOf takes the first segment", () => {
    expect(stemOf("work-slack/raw")).toBe("work-slack");
    expect(stemOf("a/b/c")).toBe("a");
    expect(stemOf("solo")).toBe("solo");
  });

  // The one place a `/` is split off an id to mint another. Both the
  // chained "also render this?" and the standalone row action come
  // through here, because two code paths minting one string is how they
  // drift apart.
  it("renderIdFor names the sibling under the same group", () => {
    expect(renderIdFor("work-slack/raw")).toBe("work-slack/rendered_md");
    expect(renderIdFor("slack-2/raw")).toBe("slack-2/rendered_md");
  });
});

describe("buildGroup", () => {
  it("writes the id, the name and the type", () => {
    const body = buildGroup({ id: "slack", name: "Work Slack", type: "slack_api" });
    expect(body).toContain("[[groups]]");
    expect(body).toContain('id = "slack"');
    expect(body).toContain('name = "Work Slack"');
    expect(body).toContain('type = "slack_api"');
  });

  it("writes no name when there is nothing to say", () => {
    expect(buildGroup({ id: "slack", name: "", type: "slack_api" })).not.toContain("name =");
    // A name that only respells the id is not a name.
    expect(buildGroup({ id: "slack", name: " slack ", type: "slack_api" })).not.toContain("name =");
  });
});

describe("buildStep", () => {
  const fetch = (values = {}) =>
    buildStep({ entry: SLACK, group: "slack", phase: "download", values });

  it("writes a fetch step as group + function, with its params and no inputs", () => {
    const body = fetch({ "sync.channels": ["general", "random"], "sync.since": "" });
    expect(body).toContain('group = "slack"');
    expect(body).toContain('function = "raw"');
    expect(body).toContain("command = \"datalib-step download slack_api\"");
    expect(body).toContain('channels = ["general", "random"]');
    expect(body).not.toContain("inputs =");
    expect(body).not.toContain("id =");
    // A step carries no name: the label comes from the group.
    expect(body).not.toContain("name =");
    // An empty optional stays out of the file rather than landing as "".
    expect(body).not.toContain("since");
  });

  it("writes a render step that names what it reads", () => {
    const body = buildStep({
      entry: SLACK,
      group: "slack",
      phase: "render",
      inputs: ["slack/raw"],
      values: {},
    });
    expect(body).toContain('group = "slack"');
    expect(body).toContain('function = "rendered_md"');
    expect(body).toContain('command = "datalib-step render slack_api"');
    expect(body).toContain('inputs = ["slack/raw"]');
  });

  // Only download-phase params land on a download step, and vice versa.
  // Getting this wrong writes a config the step's own deny_unknown
  // render config rejects at run time.
  it("writes only the phase's own params", () => {
    const values = { "sync.channels": ["general"] };
    expect(fetch(values)).toContain("channels");
    const render = buildStep({
      entry: SLACK,
      group: "slack",
      phase: "render",
      values,
    });
    expect(render).not.toContain("channels");
  });

  it("emits a parent table before its children", () => {
    const body = buildStep({
      entry: LIGHTROOM,
      group: "lightroom",
      phase: "download",
      values: { "common.input_path": "~/Pictures/cat.lrcat", skip_xmp: true },
    });
    expect(body.indexOf("[steps.params]")).toBeLessThan(body.indexOf("[steps.params.common]"));
  });

  // A bare `2026-01-01` is a TOML date; the providers validate a string.
  it("quotes dates", () => {
    expect(fetch({ "sync.since": "2026-01-01" })).toContain('since = "2026-01-01"');
  });

  // Off is a real setting, and it is the backward-compatible one — a
  // config that omits `dms` gets DMs off, so writing it explicitly is
  // what makes the wizard's answer visible in the file.
  it("writes the direct-message switch even when it is off", () => {
    const body = fetch({ "sync.dms": false });
    expect(body).toContain("dms = false");
  });

  it("writes the DM allowlist when direct messages are on", () => {
    const body = fetch({
      "sync.dms": true,
      "sync.dm_users": ["@riker", "Jean-Luc Picard"],
    });
    expect(body).toContain("dms = true");
    expect(body).toContain('dm_users = ["@riker", "Jean-Luc Picard"]');
  });

  // The one `select` field in the catalog. Its value is always written:
  // the form seeds the backend's own default rather than offering an
  // "unset" choice, so what the dropdown shows is what the file says.
  it("writes a select's value", () => {
    const body = buildStep({
      entry: SIGNAL,
      group: "signal",
      phase: "render",
      values: { period: "year" },
    });
    expect(body).toContain('period = "year"');
  });

  // A hand-edited config can hold a value no option offers. Dropping it
  // on save would silently rewrite someone's config; carrying it
  // through means `Period::from_config` gets to reject it by name.
  it("carries a select value the dropdown doesn't know", () => {
    const body = buildStep({
      entry: SIGNAL,
      group: "signal",
      phase: "render",
      values: { period: "fortnight" },
    });
    expect(body).toContain('period = "fortnight"');
  });

  // `SlackApiSync::validate` rejects `dm_users` with `dms = false`, so
  // a form that emitted it would write a config the backend refuses.
  // The gate has to drop the value, not just hide the input.
  it("drops a gated field whose switch is off", () => {
    const body = fetch({
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

describe("paramsAreRepresentable", () => {
  it("accepts params the descriptor models", () => {
    const step = listSteps(PAIR).find((s) => s.id === "slack/raw")!;
    expect(paramsAreRepresentable(step, SLACK)).toEqual({ ok: true });
  });

  // A hand-written knob the form can't show would be dropped on save,
  // so the grid disables Edit instead of losing it silently.
  it("names the params it cannot model", () => {
    const [step] = listSteps(`[[steps]]
group = "slack"
function = "raw"
command = "datalib-step download slack_api"
[steps.params.common]
download_params = { maximum_sequential_failed_requests = 3 }
`);
    const rep = paramsAreRepresentable(step, SLACK);
    expect(rep.ok).toBe(false);
    if (!rep.ok) {
      expect(rep.unknown).toContain("common.download_params.maximum_sequential_failed_requests");
    }
  });
});

describe("removeSteps / replaceStep", () => {
  it("removes one step and leaves its group, its sibling and the rest", () => {
    const fetch = listSteps(PAIR).find((s) => s.id === "slack/raw")!;
    const after = removeSteps(PAIR, [fetch]);
    expect(after).not.toContain('function = "raw"');
    expect(after).toContain('function = "rendered_md"');
    expect(after).toContain("── slack");
    expect(after).toContain('name = "Work Slack"');
    expect(after).toContain('function = "grid"');
    expect(after).toContain('data_root = "~/datalib"');
  });

  /// Deleting a fetch step alone leaves its render step naming an input
  /// that no longer exists, which the loader refuses outright — a whole
  /// config broken by a partial delete. Manager2 deletes the pair, and
  /// the group with them, taking its divider comment along.
  it("removes a pair and its group together, leaving a config that still parses", () => {
    const both = listSteps(PAIR).filter((s) => s.group === "slack");
    expect(both).toHaveLength(2);
    const group = listGroups(PAIR).find((g) => g.id === "slack")!;
    const after = unwireFromFanIns(removeSteps(PAIR, [...both, group]), "slack/rendered_md");
    expect(after).not.toContain("slack");
    expect(listSteps(after).map((s) => s.id)).toEqual([
      "unified_index/grid",
      "unified_index/qmd",
    ]);
    expect(listGroups(after).map((g) => g.id)).toEqual(["unified_index"]);
  });

  it("replaces one step without touching its sibling or its group", () => {
    const fetch = listSteps(PAIR).find((s) => s.id === "slack/raw")!;
    const body = buildStep({
      entry: SLACK,
      group: "slack",
      phase: "download",
      values: { "sync.channels": ["random"] },
    });
    const after = replaceStep(PAIR, fetch, body);
    expect(after).toContain('channels = ["random"]');
    expect(after).not.toContain('channels = ["general"]');
    expect(after).toContain('name = "Work Slack"');
    // Still exactly four steps, and the render step is as it was.
    expect(listSteps(after).map((s) => s.id).sort()).toEqual([
      "slack/raw",
      "slack/rendered_md",
      "unified_index/grid",
      "unified_index/qmd",
    ]);
  });

  it("appends where a new source can safely go", () => {
    const body = `${buildGroup({ id: "extra", name: "", type: "slack_api" })}\n\n${buildStep({
      entry: SLACK,
      group: "extra",
      phase: "download",
      values: {},
    })}`;
    const after = appendSource(PAIR, body);
    expect(listSteps(after).map((s) => s.id)).toContain("extra/raw");
    expect(listGroups(after).map((g) => g.id)).toContain("extra");
  });
});

describe("renameGroup", () => {
  it("replaces an existing name in place", () => {
    const after = renameGroup(PAIR, "slack", "Sabbatical Slack");
    expect(after).toContain('name = "Sabbatical Slack"');
    expect(after).not.toContain("Work Slack");
    expect(listSteps(after).find((s) => s.id === "slack/raw")!.name).toBe("Sabbatical Slack");
  });

  it("adds a name to a group that had none, right after its id", () => {
    const after = renameGroup(PAIR, "unified_index", "Everything");
    expect(after).toContain('id = "unified_index"\nname = "Everything"');
  });

  it("removes the key when the name is cleared or only respells the id", () => {
    for (const cleared of ["", "  ", "slack"]) {
      const after = renameGroup(PAIR, "slack", cleared);
      expect(after).not.toContain("name =");
      expect(listSteps(after).find((s) => s.id === "slack/raw")!.name).toBe("slack/raw");
      // The rest of the entry is intact.
      expect(after).toContain('id = "slack"\ntype = "slack_api"');
    }
  });

  it("leaves the text alone for a group it cannot find", () => {
    expect(renameGroup(PAIR, "nope", "X")).toBe(PAIR);
  });
});

describe("fan-in wiring", () => {
  // Adding a render step without naming it in the index steps renders
  // happily and is never indexed — invisible in search, with nothing on
  // screen to say why.
  it("adds an id once, to every index step", () => {
    const wired = wireIntoFanIns(PAIR, "email/rendered_md");
    expect(wired.match(/"email\/rendered_md"/g)).toHaveLength(2);
    expect(wireIntoFanIns(wired, "email/rendered_md")).toBe(wired);
    expect(wired).toContain('data_root = "~/datalib"');
    expect(wired).toContain("── slack");
  });

  it("removes an id from every index step, and only from their inputs", () => {
    const bare = unwireFromFanIns(PAIR, "slack/rendered_md");
    // Both fan-ins now read nothing...
    expect(bare.match(/inputs = \[\]/g)).toHaveLength(2);
    // ...but the render step itself is untouched. Unwiring is about
    // edges; removing the step is `removeSteps`, and the two are
    // separate because deleting a source needs both.
    expect(bare).toContain('function = "rendered_md"');
    expect(bare).toContain('function = "grid"');
  });

  /// The scaffold's index steps start with `inputs = []`, and the applet
  /// beside them is filed under the same group: the wiring must find the
  /// steps and not stumble on the applet.
  it("wires into a scaffold whose fan-ins are empty", () => {
    const scaffold = `[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid"
command = "datalib-step grid_index"
inputs = []

[[applets]]
group = "unified_index"
id = "unified_index"
command = "datalib-applet unified_index"
`;
    const wired = wireIntoFanIns(scaffold, "pdfs/rendered_md");
    expect(wired).toContain('inputs = ["pdfs/rendered_md"]');
    expect(listSteps(wired).find((s) => s.kind === "applet")!.inputs).toEqual([]);
  });
});
