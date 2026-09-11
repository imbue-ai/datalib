// `listSteps` and the splice-based writers behind the Pipeline table.
//
// A source is a `[[groups]]` entry with an ingest step and a render step
// filed under it; the group carries the name and the type, and each
// step's id is composed from its group and its function. The wizard
// writes and rewrites all three as one unit (`buildSource`,
// `replaceSteps`).
import { describe, expect, it } from "vitest";
import {
  appendSource,
  buildGroup,
  buildSource,
  buildStep,
  fieldIsActive,
  listGroups,
  listSteps,
  paramsAreRepresentable,
  paramsObject,
  producerOf,
  removeSteps,
  renameGroup,
  replaceSteps,
  seedFieldValues,
  sourceStepsOf,
  stepIdFor,
  unwireFromFanIns,
  wireIntoFanIns,
} from "../src/config/sourceSteps";
import { catalogFor } from "../src/config/catalog";

const SLACK = catalogFor("slack")!;
const CLAUDE = catalogFor("claude")!;
const LIGHTROOM = catalogFor("lightroom")!;
const SIGNAL = catalogFor("signal")!;

/** One source's group and two steps plus the index group and its steps. */
const PAIR = `data_root = "~/datalib"

[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = ["slack/render_markdown"]

[[steps]]
group = "unified_index"
function = "qmd_index"
inputs = ["slack/render_markdown"]

# ── slack ─────────────────────────────────────────────────────────────
[[groups]]
id = "slack"
name = "Work Slack"
type = "slack"

[[steps]]
group = "slack"
function = "ingest"
[steps.params.api]
channels = ["general"]

[[steps]]
group = "slack"
function = "render_markdown"
inputs = ["slack/ingest"]
`;

describe("listSteps", () => {
  it("gives every step its own row, in file order, under its composed id", () => {
    expect(listSteps(PAIR).map((s) => s.id)).toEqual([
      "unified_index/grid_index",
      "unified_index/qmd_index",
      "slack/ingest",
      "slack/render_markdown",
    ]);
  });

  // The distinction the step-role mark shows, and what gates every row
  // action. Read off the step's `function` — never off the shape of
  // its id, which nothing here takes apart.
  it("classifies a step by its function", () => {
    const by = new Map(listSteps(PAIR).map((s) => [s.id, s.phase]));
    expect(by.get("slack/ingest")).toBe("ingest");
    expect(by.get("slack/render_markdown")).toBe("render");
    expect(by.get("unified_index/grid_index")).toBe("index");
    expect(by.get("unified_index/qmd_index")).toBe("index");
    // A custom function under a group, and a step outside any group,
    // are steps and nothing more — whatever their ids look like.
    const custom = listSteps(`[[steps]]
group = "slack"
function = "embed"
command = "my-embedder"

[[steps]]
id = "exports/render_markdown"
command = "my-exporter"
`);
    expect(custom.map((s) => s.phase)).toEqual(["other", "other"]);
  });

  it("reads group, function, type and inputs off each step", () => {
    const by = new Map(listSteps(PAIR).map((s) => [s.id, s]));
    const fetch = by.get("slack/ingest")!;
    expect(fetch.group).toBe("slack");
    expect(fetch.function).toBe("ingest");
    // The type comes from the group, not from the command.
    expect(fetch.type).toBe("slack");
    expect(fetch.inputs).toEqual([]);
    expect(by.get("slack/render_markdown")!.inputs).toEqual(["slack/ingest"]);
    expect(by.get("unified_index/grid_index")!.type).toBeNull();
  });

  /// The group's name labels its fetch step; its render step is the same
  /// name said again, so the two rows still read as one source.
  it("labels grouped steps from the group's name", () => {
    const by = new Map(listSteps(PAIR).map((s) => [s.id, s]));
    expect(by.get("slack/ingest")!.name).toBe("Work Slack");
    expect(by.get("slack/render_markdown")!.name).toBe("Work Slack (render markdown)");
    // The index group has no name, so its steps take the shared defaults.
    expect(by.get("unified_index/grid_index")!.name).toBe("Unified Index (table)");
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
    expect(groups[1]).toMatchObject({ name: "Work Slack", type: "slack" });
    expect(PAIR.slice(groups[1].start, groups[1].end)).toContain('type = "slack"');
  });
});

describe("a source's two steps", () => {
  // The one place this side composes an id, and it composes it the way
  // the loader does: group, slash, function.
  it("stepIdFor composes group and function", () => {
    expect(stepIdFor("work-slack", "download")).toBe("work-slack/ingest");
    expect(stepIdFor("work-slack", "render")).toBe("work-slack/render_markdown");
  });

  it("sourceStepsOf finds a group's ingest and render steps by phase", () => {
    const all = listSteps(PAIR);
    const { ingest, render } = sourceStepsOf("slack", all);
    expect(ingest?.id).toBe("slack/ingest");
    expect(render?.id).toBe("slack/render_markdown");
    // The index group has neither.
    expect(sourceStepsOf("unified_index", all)).toEqual({ ingest: undefined, render: undefined });
  });

  // A render step's producer is what its inputs name; one that declares
  // none reads its group's ingest step, which is the fallback
  // `datalib-step` itself makes. Nothing splits the id to find it.
  it("producerOf follows inputs, then the group's ingest step", () => {
    const all = listSteps(PAIR);
    const render = all.find((s) => s.id === "slack/render_markdown")!;
    expect(producerOf(render, all)?.id).toBe("slack/ingest");
    const orphan = listSteps(
      PAIR.replace('inputs = ["slack/ingest"]\n', ""),
    );
    const unlinked = orphan.find((s) => s.id === "slack/render_markdown")!;
    expect(unlinked.inputs).toEqual([]);
    expect(producerOf(unlinked, orphan)?.id).toBe("slack/ingest");
    // A step outside any group has no group to fall back to.
    const [solo] = listSteps('[[steps]]\nid = "solo/render_markdown"\ncommand = "x"\n');
    expect(producerOf(solo, [solo])).toBeUndefined();
  });
});

describe("buildSource", () => {
  it("writes the group, the ingest step and a render step reading it", () => {
    const out = buildSource({
      entry: SLACK,
      group: "slack",
      name: "Work Slack",
      values: { "api.channels": ["general"] },
      withGroup: true,
    });
    expect(out.groupBody).toContain('id = "slack"');
    expect(out.groupBody).toContain('name = "Work Slack"');
    expect(out.stepsBody).toContain('function = "ingest"');
    expect(out.stepsBody).toContain('channels = ["general"]');
    expect(out.stepsBody).toContain('function = "render_markdown"');
    expect(out.stepsBody).toContain('inputs = ["slack/ingest"]');
    expect(out.renderId).toBe("slack/render_markdown");
    // Ingest first: the render step names it.
    expect(out.stepsBody.indexOf('function = "ingest"')).toBeLessThan(
      out.stepsBody.indexOf('function = "render_markdown"'),
    );
    // And the whole thing parses back as the two steps under the group.
    const text = `${out.groupBody}\n\n${out.stepsBody}`;
    expect(listSteps(text).map((s) => s.id)).toEqual(["slack/ingest", "slack/render_markdown"]);
  });

  it("writes no group when editing, and no render step for a provider that renders nothing", () => {
    const out = buildSource({
      entry: LIGHTROOM,
      group: "photos",
      name: "",
      values: { "catalog.path": "~/cat.lrcat" },
      withGroup: false,
    });
    expect(out.groupBody).toBeNull();
    expect(out.stepsBody).not.toContain("render_markdown");
    expect(out.renderId).toBeNull();
  });

  // Each phase's fields land on its own step and nowhere else — a
  // render knob on the ingest step is a config the ingest side's
  // deny_unknown_fields refuses at run time.
  it("puts render fields on the render step", () => {
    const out = buildSource({
      entry: SIGNAL,
      group: "signal",
      name: "",
      values: { "backup.path": "~/backups", period: "year" },
      withGroup: true,
    });
    const [ingest, render] = out.stepsBody.split("\n\n[[steps]]");
    expect(ingest).toContain('[steps.params.backup]\npath = "~/backups"');
    expect(ingest).not.toContain("period");
    expect(render).toContain('period = "year"');
    expect(render).not.toContain("backup");
  });
});

describe("buildGroup", () => {
  it("writes the id, the name and the type", () => {
    const body = buildGroup({ id: "slack", name: "Work Slack", type: "slack" });
    expect(body).toContain("[[groups]]");
    expect(body).toContain('id = "slack"');
    expect(body).toContain('name = "Work Slack"');
    expect(body).toContain('type = "slack"');
  });

  it("writes no name when there is nothing to say", () => {
    expect(buildGroup({ id: "slack", name: "", type: "slack" })).not.toContain("name =");
    // A name that only respells the id is not a name.
    expect(buildGroup({ id: "slack", name: " slack ", type: "slack" })).not.toContain("name =");
  });
});

describe("buildStep", () => {
  const fetch = (values = {}) =>
    buildStep({ entry: SLACK, group: "slack", phase: "download", values });

  it("writes a fetch step as group + function, with its params and no inputs", () => {
    const body = fetch({ "api.channels": ["general", "random"], "api.since": "" });
    expect(body).toContain('group = "slack"');
    expect(body).toContain('function = "ingest"');
    // No command: a built-in step is `datalib-step`, and the loader
    // supplies that from the absence.
    expect(body).not.toContain("command");
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
      inputs: ["slack/ingest"],
      values: {},
    });
    expect(body).toContain('group = "slack"');
    expect(body).toContain('function = "render_markdown"');
    expect(body).not.toContain("command");
    expect(body).toContain('inputs = ["slack/ingest"]');
  });

  // Only download-phase params land on a download step, and vice versa.
  // Getting this wrong writes a config the step's own deny_unknown
  // render config rejects at run time.
  it("writes only the phase's own params", () => {
    const values = { "api.channels": ["general"] };
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
      values: { "catalog.path": "~/Pictures/cat.lrcat", skip_xmp: true },
    });
    expect(body.indexOf("[steps.params]")).toBeLessThan(body.indexOf("[steps.params.catalog]"));
  });

  // The method table is what names the ingest method, so it is written
  // even when none of its knobs is: `api = {}` is a complete selection,
  // and a step naming no method is refused by `datalib-step`.
  it("writes the method table even when nothing under it is set", () => {
    expect(fetch({})).toContain("[steps.params]\napi = {}");
    // …and not once a knob under it is written, whatever else is.
    const knobs = fetch({ "api.media": true, "common.blob_size_limit_bytes": 5 });
    expect(knobs).not.toContain("api = {}");
    expect(knobs).toContain("[steps.params.api]\nmedia = true");
    expect(knobs).toContain("[steps.params.common]");
  });

  // A bare `2026-01-01` is a TOML date; the providers validate a string.
  it("quotes dates", () => {
    expect(fetch({ "api.since": "2026-01-01" })).toContain('since = "2026-01-01"');
  });

  // Off is a real setting, and it is the backward-compatible one — a
  // config that omits `dms` gets DMs off, so writing it explicitly is
  // what makes the wizard's answer visible in the file.
  it("writes the direct-message switch even when it is off", () => {
    const body = fetch({ "api.dms": false });
    expect(body).toContain("dms = false");
  });

  it("writes the DM allowlist when direct messages are on", () => {
    const body = fetch({
      "api.dms": true,
      "api.dm_users": ["@riker", "Jean-Luc Picard"],
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
      "api.dms": false,
      "api.dm_users": ["@riker"],
    });
    expect(body).toContain("dms = false");
    expect(body).not.toContain("dm_users");
  });
});

describe("fieldIsActive", () => {
  const dmUsers = SLACK.fields!.find((f) => f.target === "api.dm_users")!;
  const channels = SLACK.fields!.find((f) => f.target === "api.channels")!;

  it("gates a field on its `requires` target", () => {
    expect(fieldIsActive(dmUsers, { "api.dms": true })).toBe(true);
    expect(fieldIsActive(dmUsers, { "api.dms": false })).toBe(false);
    // Unset reads as off, which is what a freshly opened form has.
    expect(fieldIsActive(dmUsers, {})).toBe(false);
  });

  it("leaves an ungated field alone", () => {
    expect(fieldIsActive(channels, {})).toBe(true);
  });
});

describe("the Slack pickers", () => {
  const dmUsers = SLACK.fields!.find((f) => f.target === "api.dm_users")!;
  const channels = SLACK.fields!.find((f) => f.target === "api.channels")!;

  /// Same probe, same grid as email and Claude: `channels` is filled
  /// from the workspace's channel items and `dm_users` from the people
  /// behind its DMs, and neither writes anything the downloader would
  /// not have matched by hand.
  it("offer channels and people from the one probe", () => {
    expect(SLACK.canProbe).toBe(true);
    expect(channels.kind === "string_list" && channels.probe).toBe("channels");
    expect(dmUsers.kind === "string_list" && dmUsers.probe).toBe("people");
  });

  /// What "Test connection" authenticates with is what Save writes —
  /// the `api` table that selects the live method, defaults included.
  it("probe with the ingest params the form would write", () => {
    expect(paramsObject(SLACK, seedFieldValues(SLACK), "download")).toEqual({
      api: { media: true, all_channels: false, dms: false },
      common: { blob_size_limit_bytes: 5_000_000 },
    });
  });
});

describe("paramsAreRepresentable", () => {
  it("accepts params the descriptor models", () => {
    const step = listSteps(PAIR).find((s) => s.id === "slack/ingest")!;
    expect(paramsAreRepresentable(step, SLACK)).toEqual({ ok: true });
  });

  // A hand-written knob the form can't show would be dropped on save,
  // so the grid disables Edit instead of losing it silently.
  it("names the params it cannot model", () => {
    const [step] = listSteps(`[[steps]]
group = "slack"
function = "ingest"
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

describe("removeSteps / replaceSteps", () => {
  it("removes one step and leaves its group, its sibling and the rest", () => {
    const fetch = listSteps(PAIR).find((s) => s.id === "slack/ingest")!;
    const after = removeSteps(PAIR, [fetch]);
    expect(after).not.toContain('function = "ingest"');
    expect(after).toContain('function = "render_markdown"');
    expect(after).toContain("── slack");
    expect(after).toContain('name = "Work Slack"');
    expect(after).toContain('function = "grid_index"');
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
    const after = unwireFromFanIns(removeSteps(PAIR, [...both, group]), "slack/render_markdown");
    expect(after).not.toContain("slack");
    expect(listSteps(after).map((s) => s.id)).toEqual([
      "unified_index/grid_index",
      "unified_index/qmd_index",
    ]);
    expect(listGroups(after).map((g) => g.id)).toEqual(["unified_index"]);
  });

  it("replaces one step without touching its sibling or its group", () => {
    const fetch = listSteps(PAIR).find((s) => s.id === "slack/ingest")!;
    const body = buildStep({
      entry: SLACK,
      group: "slack",
      phase: "download",
      values: { "api.channels": ["random"] },
    });
    const after = replaceSteps(PAIR, [fetch], body);
    expect(after).toContain('channels = ["random"]');
    expect(after).not.toContain('channels = ["general"]');
    expect(after).toContain('name = "Work Slack"');
    // Still exactly four steps, and the render step is as it was.
    expect(listSteps(after).map((s) => s.id).sort()).toEqual([
      "slack/ingest",
      "slack/render_markdown",
      "unified_index/grid_index",
      "unified_index/qmd_index",
    ]);
  });

  // The edit path: both steps cut against the text as parsed, then one
  // append. Cutting one, appending, then cutting the other would use
  // offsets into text the first cut had already shifted.
  it("replaces a source's pair in one pass, leaving exactly one of each", () => {
    const { ingest, render } = sourceStepsOf("slack", listSteps(PAIR));
    const out = buildSource({
      entry: SLACK,
      group: "slack",
      name: "Work Slack",
      values: { "api.channels": ["random"] },
      withGroup: false,
    });
    const after = replaceSteps(PAIR, [ingest!, render!], out.stepsBody);
    expect(after.match(/function = "ingest"/g)).toHaveLength(1);
    expect(after.match(/function = "render_markdown"/g)).toHaveLength(1);
    expect(after).toContain('channels = ["random"]');
    expect(after).not.toContain('channels = ["general"]');
    // The group and the index steps are untouched.
    expect(after).toContain('name = "Work Slack"');
    expect(listSteps(after).map((s) => s.id).sort()).toEqual([
      "slack/ingest",
      "slack/render_markdown",
      "unified_index/grid_index",
      "unified_index/qmd_index",
    ]);
  });

  // A hand-edited group can be missing one of its steps; saving from
  // the form writes it alongside the other, and nothing is duplicated.
  it("adds the step a source was missing", () => {
    const fetchOnly = PAIR.replace(
      /\n\[\[steps\]\]\ngroup = "slack"\nfunction = "render_markdown"\ninputs = \["slack\/ingest"\]\n/,
      "\n",
    );
    const { ingest, render } = sourceStepsOf("slack", listSteps(fetchOnly));
    expect(render).toBeUndefined();
    const out = buildSource({
      entry: SLACK,
      group: "slack",
      name: "",
      values: {},
      withGroup: false,
    });
    const after = replaceSteps(fetchOnly, [ingest!], out.stepsBody);
    expect(listSteps(after).map((s) => s.id).sort()).toEqual([
      "slack/ingest",
      "slack/render_markdown",
      "unified_index/grid_index",
      "unified_index/qmd_index",
    ]);
  });

  it("appends where a new source can safely go", () => {
    const body = `${buildGroup({ id: "extra", name: "", type: "slack" })}\n\n${buildStep({
      entry: SLACK,
      group: "extra",
      phase: "download",
      values: {},
    })}`;
    const after = appendSource(PAIR, body);
    expect(listSteps(after).map((s) => s.id)).toContain("extra/ingest");
    expect(listGroups(after).map((g) => g.id)).toContain("extra");
  });
});

describe("renameGroup", () => {
  it("replaces an existing name in place", () => {
    const after = renameGroup(PAIR, "slack", "Sabbatical Slack");
    expect(after).toContain('name = "Sabbatical Slack"');
    expect(after).not.toContain("Work Slack");
    expect(listSteps(after).find((s) => s.id === "slack/ingest")!.name).toBe("Sabbatical Slack");
  });

  it("adds a name to a group that had none, right after its id", () => {
    const after = renameGroup(PAIR, "unified_index", "Everything");
    expect(after).toContain('id = "unified_index"\nname = "Everything"');
  });

  it("removes the key when the name is cleared or only respells the id", () => {
    for (const cleared of ["", "  ", "slack"]) {
      const after = renameGroup(PAIR, "slack", cleared);
      expect(after).not.toContain("name =");
      expect(listSteps(after).find((s) => s.id === "slack/ingest")!.name).toBe("slack/ingest");
      // The rest of the entry is intact.
      expect(after).toContain('id = "slack"\ntype = "slack"');
    }
  });

  it("leaves the text alone for a group it cannot find", () => {
    expect(renameGroup(PAIR, "nope", "X")).toBe(PAIR);
  });

  // A name is user text; `$1`, `$&` and `$$` in it must land verbatim,
  // not be read as replacement patterns.
  it("writes a name containing dollar signs verbatim", () => {
    const name = "Top $1 & $& costs $$";
    // Replacing an existing name, and inserting one after the id.
    for (const groupId of ["slack", "unified_index"]) {
      expect(renameGroup(PAIR, groupId, name)).toContain(`name = "${name}"`);
    }
    expect(listSteps(renameGroup(PAIR, "slack", name)).find((s) => s.id === "slack/ingest")!.name).toBe(
      name,
    );
  });
});

describe("fan-in wiring", () => {
  // Adding a render step without naming it in the index steps renders
  // happily and is never indexed — invisible in search, with nothing on
  // screen to say why.
  it("adds an id once, to every index step", () => {
    const wired = wireIntoFanIns(PAIR, "email/render_markdown");
    expect(wired.match(/"email\/render_markdown"/g)).toHaveLength(2);
    expect(wireIntoFanIns(wired, "email/render_markdown")).toBe(wired);
    expect(wired).toContain('data_root = "~/datalib"');
    expect(wired).toContain("── slack");
  });

  it("removes an id from every index step, and only from their inputs", () => {
    const bare = unwireFromFanIns(PAIR, "slack/render_markdown");
    // Both fan-ins now read nothing...
    expect(bare.match(/inputs = \[\]/g)).toHaveLength(2);
    // ...but the render step itself is untouched. Unwiring is about
    // edges; removing the step is `removeSteps`, and the two are
    // separate because deleting a source needs both.
    expect(bare).toContain('function = "render_markdown"');
    expect(bare).toContain('function = "grid_index"');
  });

  /// The scaffold's index steps start with `inputs = []`, and the applet
  /// beside them is filed under the same group: the wiring must find the
  /// steps and not stumble on the applet.
  it("wires into a scaffold whose fan-ins are empty", () => {
    const scaffold = `[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = []

[[applets]]
group = "unified_index"
id = "unified_index"
command = "datalib-applet unified_index"
`;
    const wired = wireIntoFanIns(scaffold, "pdfs/render_markdown");
    expect(wired).toContain('inputs = ["pdfs/render_markdown"]');
    expect(listSteps(wired).find((s) => s.kind === "applet")!.inputs).toEqual([]);
  });
});

describe("what Test connection authenticates as", () => {
  /// The probe runs as `latchkey --account <acct> curl`, so an account
  /// left out of its params tests a different identity from the one
  /// picked — and comes back looking perfectly healthy while describing
  /// somebody else's conversations.
  it("carries the chosen latchkey account", () => {
    const params = paramsObject(
      CLAUDE,
      { "latchkey_settings.account": "picard@enterprise.gov" },
      "download",
    );
    expect(params).toMatchObject({
      latchkey_settings: { account: "picard@enterprise.gov" },
    });
  });

  /// Empty is latchkey's own name for its one unnamed account, and it
  /// is addressed by writing no `account` at all — not by sending "".
  it("writes no account for latchkey's default", () => {
    const params = paramsObject(CLAUDE, { "latchkey_settings.account": "" }, "download");
    expect(params).not.toHaveProperty("latchkey_settings");
    // The method table still has to be there: it is what says this
    // source is the live API rather than an unpacked export.
    expect(params).toHaveProperty("api");
  });
});
