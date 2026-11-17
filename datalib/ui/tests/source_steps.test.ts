// `listSteps` and the splice-based writers behind the Pipeline table.
//
// A source is a `[[groups]]` entry with an ingest step and a render step
// filed under it; the group carries the name and the type, and each
// step's id is composed from its group and its function. The wizard
// writes all three as one unit (`buildSource`) and edits them key by
// key (`patchSource`).
import { describe, expect, it } from "vitest";
import {
  insertEntries,
  fieldsFor,
  buildDiffSource,
  buildGroup,
  buildSource,
  buildStep,
  chosenAnswer,
  describeGroup,
  fieldIsActive,
  listGroups,
  listSteps,
  moveAgainstDataFlow,
  moveGroup,
  paramsAreRepresentable,
  paramsObject,
  producerOf,
  removeSteps,
  patchSource,
  renameGroup,
  seedFieldValues,
  sourceStepsOf,
  stepIdFor,
  readersOf,
  removedWith,
  setSearchSteps,
  setTimeSeriesStep,
  setOccurrencesStep,
  searchIndexingOf,
  unwireFromFanIns,
  wireIntoFanIns,
} from "../src/config/sourceSteps";
import { CATALOG, catalogFor, catalogForStep } from "../src/config/catalog";

const SLACK = catalogFor("slack")!;
const CLAUDE = catalogFor("claude")!;
const CHATGPT = catalogFor("chatgpt")!;
const LIGHTROOM = catalogFor("lightroom")!;
const APPLE_PHOTOS = catalogFor("apple_photos")!;
const BEEPER = catalogFor("beeper")!;

/** One source's group and two steps plus the index group and its steps. */
const PAIR = `data_root = "~/datalib"

[[groups]]
id = "unified_grid_and_search_index"

[[steps]]
group = "unified_grid_and_search_index"
function = "grid_index"
inputs = ["slack/render_markdown"]

[[steps]]
group = "unified_grid_and_search_index"
function = "qmd_aggregator"
inputs = []

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
      "unified_grid_and_search_index/grid_index",
      "unified_grid_and_search_index/qmd_aggregator",
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
    expect(by.get("unified_grid_and_search_index/grid_index")).toBe("index");
    expect(by.get("unified_grid_and_search_index/qmd_aggregator")).toBe("index");
    const search = new Map(
      listSteps(setSearchSteps(PAIR, "slack", "keyword_and_embed")).map((s) => [s.id, s]),
    );
    expect(search.get("slack/embed")!.phase).toBe("index");
    expect(search.get("slack/embed")!.name).toBe("Work Slack (Semantic indexing)");
    // A custom function under a group, and a step outside any group,
    // are steps and nothing more — whatever their ids look like.
    const custom = listSteps(`[[steps]]
group = "slack"
function = "summarize"
command = "my-summarizer"

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
    expect(by.get("unified_grid_and_search_index/grid_index")!.type).toBeNull();
  });

  /// The group's name labels its fetch step; its render step is the same
  /// name said again, so the two rows still read as one source.
  it("labels grouped steps from the group's name", () => {
    const by = new Map(listSteps(PAIR).map((s) => [s.id, s]));
    expect(by.get("slack/ingest")!.name).toBe("Work Slack");
    expect(by.get("slack/render_markdown")!.name).toBe("Work Slack (render markdown)");
    // The index group has no name, so its steps take the shared defaults.
    expect(by.get("unified_grid_and_search_index/grid_index")!.name).toBe(
      "Grid and Search Index (table)",
    );
  });

  it("lists applets, after the steps, with no inputs", () => {
    const withApplet = `${PAIR}
[[applets]]
group = "unified_grid_and_search_index"
id = "unified_grid_and_search_index"
command = "datalib-applet unified_grid_and_search_index"
`;
    const entries = listSteps(withApplet);
    expect(entries.at(-1)!.kind).toBe("applet");
    expect(entries.at(-1)!.id).toBe("unified_grid_and_search_index");
    expect(entries.at(-1)!.group).toBe("unified_grid_and_search_index");
    expect(entries.at(-1)!.type).toBe("unified_grid_and_search_index");
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
    expect(groups.map((g) => g.id)).toEqual(["unified_grid_and_search_index", "slack"]);
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
    expect(sourceStepsOf("unified_grid_and_search_index", all)).toEqual({
      ingest: undefined,
      render: undefined,
    });
  });

  // A render step's producer is what its inputs name; one that declares
  // none reads its group's ingest step, which is the fallback
  // `datalib-step` itself makes. Nothing splits the id to find it.
  it("producerOf follows inputs, then the group's ingest step", () => {
    const all = listSteps(PAIR);
    const render = all.find((s) => s.id === "slack/render_markdown")!;
    expect(producerOf(render, all)?.id).toBe("slack/ingest");
    const orphan = listSteps(PAIR.replace('inputs = ["slack/ingest"]\n', ""));
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

  // A download-only source's render step writes no documents, but it
  // is what carries the download's problems into the index; without it
  // the Manage row counts problems the Problems card cannot show.
  it("writes no group when editing, and a render step for a download-only provider even when asked not to", () => {
    const out = buildSource({
      entry: LIGHTROOM,
      group: "photos",
      name: "",
      values: { "catalog.path": "~/cat.lrcat" },
      withGroup: false,
      renders: false,
    });
    expect(out.groupBody).toBeNull();
    expect(out.stepsBody).toContain(
      'function = "render_markdown"\ncommand = "datalib-step"\ninputs = ["photos/ingest"]',
    );
    expect(out.renderId).toBe("photos/render_markdown");
  });

  // Each phase's fields land on its own step and nowhere else — a
  // render knob on the ingest step is a config the ingest side's
  // deny_unknown_fields refuses at run time.
  it("puts render fields on the render step", () => {
    const out = buildSource({
      entry: BEEPER,
      group: "beeper",
      name: "",
      values: { "texts.path": "~/BeeperTexts", networks: ["signal"] },
      withGroup: true,
    });
    const [ingest, render] = out.stepsBody.split("\n\n[[steps]]");
    expect(ingest).toContain('[steps.params.texts]\npath = "~/BeeperTexts"');
    expect(ingest).not.toContain("networks");
    expect(render).toContain('networks = ["signal"]');
    expect(render).not.toContain("texts");
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

  it("writes a description only when there is one", () => {
    const body = buildGroup({
      id: "slack",
      name: "",
      type: "slack",
      description: "  Mostly infra channels.  ",
    });
    expect(body).toContain('type = "slack"\ndescription = "Mostly infra channels."');
    expect(listGroups(body)[0]!.description).toBe("Mostly infra channels.");
    expect(buildGroup({ id: "slack", name: "", type: "slack", description: " " })).not.toContain(
      "description =",
    );
    expect(buildGroup({ id: "slack", name: "", type: "slack" })).not.toContain("description =");
  });
});

describe("buildStep", () => {
  const fetch = (values = {}) =>
    buildStep({ entry: SLACK, group: "slack", phase: "download", values });

  it("writes a fetch step as group + function, with its params and no inputs", () => {
    const body = fetch({ "api.channels": ["general", "random"], "api.since": "" });
    expect(body).toContain('group = "slack"');
    expect(body).toContain('function = "ingest"');
    // A built-in step says it runs `datalib-step`, which reads its
    // function and its group's type from the environment.
    expect(body).toContain('function = "ingest"\ncommand = "datalib-step"');
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
    expect(body).toContain('function = "render_markdown"\ncommand = "datalib-step"');
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

  // The Photos form is the Lightroom form with a different method table
  // and preset: one bundle path, download-only, and the same shape on
  // disk that `apple_photos_config`'s `deny_unknown_fields` accepts.
  it("writes an Apple Photos source as an ingest step under its library table, and its render step", () => {
    const out = buildSource({
      entry: APPLE_PHOTOS,
      group: "apple_photos",
      name: "",
      values: {
        "library.path": "~/Pictures/Photos Library.photoslibrary",
        skip_history: false,
      },
      withGroup: true,
    });
    expect(out.groupBody).toContain('type = "apple_photos"');
    expect(out.stepsBody).toContain("skip_history = false");
    expect(out.stepsBody).toContain(
      '[steps.params.library]\npath = "~/Pictures/Photos Library.photoslibrary"',
    );
    expect(out.stepsBody.indexOf("[steps.params]")).toBeLessThan(
      out.stepsBody.indexOf("[steps.params.library]"),
    );
    const text = `${out.groupBody}\n\n${out.stepsBody}`;
    expect(listSteps(text).map((s) => s.id)).toEqual([
      "apple_photos/ingest",
      "apple_photos/render_markdown",
    ]);
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

  it("writes the DM list when direct messages are on", () => {
    const body = fetch({
      "api.dms": true,
      "api.dm_conversations": ["D024BE7LH", "G0ABC12DE"],
    });
    expect(body).toContain("dms = true");
    expect(body).toContain('dm_conversations = ["D024BE7LH", "G0ABC12DE"]');
  });

  // A select's value is always written: the form seeds the backend's own
  // default rather than offering an "unset" choice, so what the dropdown
  // shows is what the file says.
  it("writes a select's value", () => {
    const body = buildStep({
      entry: SLACK,
      group: "slack",
      phase: "download",
      values: { "api.scope": "involves_me" },
    });
    expect(body).toContain('scope = "involves_me"');
  });

  // A hand-edited config can hold a value no option offers. Dropping it
  // on save would silently rewrite someone's config; carrying it
  // through means the backend gets to reject it by name.
  it("carries a select value the dropdown doesn't know", () => {
    const body = buildStep({
      entry: SLACK,
      group: "slack",
      phase: "download",
      values: { "api.scope": "fortnight" },
    });
    expect(body).toContain('scope = "fortnight"');
  });

  // `SlackApiSync::validate` rejects `dm_conversations` with
  // `dms = false`, so a form that emitted it would write a config the
  // backend refuses. The gate has to drop the value, not just hide
  // the input.
  it("drops a gated field whose switch is off", () => {
    const body = fetch({
      "api.dms": false,
      "api.dm_conversations": ["D024BE7LH"],
    });
    expect(body).toContain("dms = false");
    expect(body).not.toContain("dm_conversations");
  });
});

describe("fieldIsActive", () => {
  const dms = SLACK.fields!.find((f) => f.target === "api.dm_conversations")!;
  const channels = SLACK.fields!.find((f) => f.target === "api.channels")!;

  it("gates a field on its `requires` target", () => {
    expect(fieldIsActive(dms, { "api.dms": true })).toBe(true);
    expect(fieldIsActive(dms, { "api.dms": false })).toBe(false);
    // Unset reads as off, which is what a freshly opened form has.
    expect(fieldIsActive(dms, {})).toBe(false);
  });

  it("leaves an ungated field alone", () => {
    expect(fieldIsActive(channels, {})).toBe(true);
  });
});

describe("the Slack pickers", () => {
  const dms = SLACK.fields!.find((f) => f.target === "api.dm_conversations")!;
  const channels = SLACK.fields!.find((f) => f.target === "api.channels")!;

  /// Same probe, same grid as email and Claude: `channels` is filled
  /// from the workspace's channel items and `dm_conversations` from
  /// its DMs — the same `conversation` kind a Claude chat is, since
  /// both are an id with a title over it.
  it("offer channels and DMs from the one probe", () => {
    expect(SLACK.canProbe).toBe(true);
    expect(channels.kind === "string_list" && channels.probe).toBe("channels");
    expect(dms.kind === "string_list" && dms.probe).toBe("conversations");
  });

  /// What "Check account" authenticates with is what Save writes —
  /// the `api` table that selects the live method, defaults included.
  it("probe with the ingest params the form would write", () => {
    expect(paramsObject(SLACK, seedFieldValues(SLACK), "download")).toEqual({
      api: {
        media: true,
        all_channels: false,
        archived: false,
        dms: false,
        scope: "every_conversation",
      },
      common: { blob_size_limit_bytes: "5 MB" },
    });
  });

  /// The cap reads however the file spells it — TOML's `_` separators
  /// on an integer, or a unit string — and opens as a form the control
  /// can show.
  it("read the cap in either spelling the config allows", () => {
    const seededFrom = (line: string) => {
      const [step] = listSteps(`[[steps]]
group = "slack"
function = "ingest"
[steps.params.common]
${line}
`);
      return seedFieldValues(SLACK, { ingest: step })["common.blob_size_limit_bytes"];
    };
    expect(seededFrom("blob_size_limit_bytes = 5_000_000")).toBe("5 MB");
    expect(seededFrom('blob_size_limit_bytes = "512 MiB"')).toBe("512 MiB");
  });
});

describe("paramsAreRepresentable", () => {
  it("accepts params the descriptor models", () => {
    const step = listSteps(PAIR).find((s) => s.id === "slack/ingest")!;
    expect(paramsAreRepresentable(step, SLACK)).toEqual({ ok: true });
  });

  // The form names a hand-written knob it can't show, and saving keeps it.
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

describe("removeSteps / patchSource", () => {
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
  /// config broken by a partial delete. The sources card deletes the pair, and
  /// the group with them, taking its divider comment along.
  it("removes a pair and its group together, leaving a config that still parses", () => {
    const both = listSteps(PAIR).filter((s) => s.group === "slack");
    expect(both).toHaveLength(2);
    const group = listGroups(PAIR).find((g) => g.id === "slack")!;
    const after = unwireFromFanIns(removeSteps(PAIR, [...both, group]), "slack/render_markdown");
    expect(after).not.toContain("slack");
    expect(listSteps(after).map((s) => s.id)).toEqual([
      "unified_grid_and_search_index/grid_index",
      "unified_grid_and_search_index/qmd_aggregator",
    ]);
    expect(listGroups(after).map((g) => g.id)).toEqual(["unified_grid_and_search_index"]);
  });

  it("edits one step without touching its sibling or its group", () => {
    const values = { ...seedFieldValues(SLACK, sourceStepsOf("slack", listSteps(PAIR))) };
    values["api.channels"] = ["random"];
    const after = patchSource(PAIR, { entry: SLACK, group: "slack", values });
    expect(after).toContain('channels = ["random"]');
    expect(after).not.toContain('channels = ["general"]');
    expect(after).toContain('name = "Work Slack"');
    // Still exactly four steps, and the render step is as it was.
    expect(
      listSteps(after)
        .map((s) => s.id)
        .sort(),
    ).toEqual([
      "slack/ingest",
      "slack/render_markdown",
      "unified_grid_and_search_index/grid_index",
      "unified_grid_and_search_index/qmd_aggregator",
    ]);
  });

  it("edits a source's pair, leaving exactly one of each", () => {
    const after = patchSource(PAIR, {
      entry: SLACK,
      group: "slack",
      values: { "api.channels": ["random"] },
    });
    expect(after.match(/function = "ingest"/g)).toHaveLength(1);
    expect(after.match(/function = "render_markdown"/g)).toHaveLength(1);
    expect(after).toContain('channels = ["random"]');
    expect(after).not.toContain('channels = ["general"]');
    // The group and the index steps are untouched.
    expect(after).toContain('name = "Work Slack"');
    expect(
      listSteps(after)
        .map((s) => s.id)
        .sort(),
    ).toEqual([
      "slack/ingest",
      "slack/render_markdown",
      "unified_grid_and_search_index/grid_index",
      "unified_grid_and_search_index/qmd_aggregator",
    ]);
  });

  // A hand-edited group can be missing one of its steps; saving from
  // the form writes it alongside the other, and nothing is duplicated.
  it("adds the step a source was missing", () => {
    const fetchOnly = PAIR.replace(
      /\n\[\[steps\]\]\ngroup = "slack"\nfunction = "render_markdown"\ninputs = \["slack\/ingest"\]\n/,
      "\n",
    );
    const { render } = sourceStepsOf("slack", listSteps(fetchOnly));
    expect(render).toBeUndefined();
    const after = patchSource(fetchOnly, { entry: SLACK, group: "slack", values: {} });
    expect(
      listSteps(after)
        .map((s) => s.id)
        .sort(),
    ).toEqual([
      "slack/ingest",
      "slack/render_markdown",
      "unified_grid_and_search_index/grid_index",
      "unified_grid_and_search_index/qmd_aggregator",
    ]);
  });

  it("adds a new source where it can safely go", () => {
    const body = `${buildGroup({ id: "extra", name: "", type: "slack" })}\n\n${buildStep({
      entry: SLACK,
      group: "extra",
      phase: "download",
      values: {},
    })}`;
    const after = insertEntries(PAIR, body);
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
    const after = renameGroup(PAIR, "unified_grid_and_search_index", "Everything");
    expect(after).toContain('id = "unified_grid_and_search_index"\nname = "Everything"');
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

  // The description gets the same in-place edit as the name, without
  // the "respells the id" rule.
  it("sets, replaces and clears a description the same way", () => {
    const set = describeGroup(PAIR, "slack", "Mostly infra channels.");
    expect(set).toContain('id = "slack"\ndescription = "Mostly infra channels."');
    expect(listGroups(set).find((g) => g.id === "slack")!.description).toBe(
      "Mostly infra channels.",
    );
    // Unlike a name, a description that respells the id is still text.
    expect(describeGroup(PAIR, "slack", "slack")).toContain('description = "slack"');

    const replaced = describeGroup(set, "slack", "The on-call channels.");
    expect(replaced).toContain('description = "The on-call channels."');
    expect(replaced).not.toContain("infra");

    const cleared = describeGroup(set, "slack", "  ");
    expect(cleared).not.toContain("description =");
    expect(cleared).toContain('id = "slack"\nname = "Work Slack"');
    expect(describeGroup(PAIR, "slack", "")).toBe(PAIR);
  });

  // A name is user text; `$1`, `$&` and `$$` in it must land verbatim,
  // not be read as replacement patterns.
  it("writes a name containing dollar signs verbatim", () => {
    const name = "Top $1 & $& costs $$";
    // Replacing an existing name, and inserting one after the id.
    for (const groupId of ["slack", "unified_grid_and_search_index"]) {
      expect(renameGroup(PAIR, groupId, name)).toContain(`name = "${name}"`);
    }
    expect(
      listSteps(renameGroup(PAIR, "slack", name)).find((s) => s.id === "slack/ingest")!.name,
    ).toBe(name);
  });
});

describe("fan-in wiring", () => {
  // Adding a render step without naming it in the index steps renders
  // happily and is never indexed — invisible in search, with nothing on
  // screen to say why.
  it("adds a render step once, to the grid index alone", () => {
    const wired = wireIntoFanIns(PAIR, "email/render_markdown");
    expect(wired.match(/"email\/render_markdown"/g)).toHaveLength(1);
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

  /// Semantic search is the one fan-in a person can opt out of, so the
  /// wiring has to reach one fan-in without touching its neighbour —
  /// the grid index is not a choice.
  it("wires and unwires one fan-in without touching the other", () => {
    const both = wireIntoFanIns(
      wireIntoFanIns(PAIR, "email/render_markdown"),
      "email/keyword_index",
      "qmd_aggregator",
    );
    const steps = (text: string, id: string) => listSteps(text).find((s) => s.id === id)!.inputs;
    expect(steps(both, "unified_grid_and_search_index/qmd_aggregator")).toEqual([
      "email/keyword_index",
    ]);
    expect(steps(both, "unified_grid_and_search_index/grid_index")).not.toContain(
      "email/keyword_index",
    );

    const gridOnly = unwireFromFanIns(both, "email/keyword_index", "qmd_aggregator");
    expect(steps(gridOnly, "unified_grid_and_search_index/grid_index")).toContain(
      "email/render_markdown",
    );
    expect(steps(gridOnly, "unified_grid_and_search_index/qmd_aggregator")).toEqual([]);
  });

  /// Which fan-in a step is comes off its `function`, not off the order
  /// its keys happen to be written in.
  it("finds a fan-in whose keys are in an unusual order", () => {
    const odd = `[[steps]]
inputs = []
function = "qmd_aggregator"
group = "unified_grid_and_search_index"
`;
    const wired = wireIntoFanIns(odd, "pdfs/keyword_index", "qmd_aggregator");
    expect(
      listSteps(wired).find((s) => s.id === "unified_grid_and_search_index/qmd_aggregator")!.inputs,
    ).toEqual(["pdfs/keyword_index"]);
  });

  /// The scaffold's index steps start with `inputs = []`, and the applet
  /// beside them is filed under the same group: the wiring must find the
  /// steps and not stumble on the applet.
  it("wires into a scaffold whose fan-ins are empty", () => {
    const scaffold = `[[groups]]
id = "unified_grid_and_search_index"

[[steps]]
group = "unified_grid_and_search_index"
function = "grid_index"
inputs = []

[[applets]]
group = "unified_grid_and_search_index"
id = "unified_grid_and_search_index"
command = "datalib-applet unified_grid_and_search_index"
`;
    const wired = wireIntoFanIns(scaffold, "pdfs/render_markdown");
    expect(wired).toContain('inputs = ["pdfs/render_markdown"]');
    expect(listSteps(wired).find((s) => s.kind === "applet")!.inputs).toEqual([]);
  });

  const gridIndex = (inputs: string) => `[[steps]]
group = "unified_grid_and_search_index"
function = "grid_index"
inputs = ${inputs}

[[steps]]
group = "unified_grid_and_search_index"
function = "qmd_aggregator"
inputs = ["bridge/keyword_index"]
`;

  /// #897: wiring rewrote a hand-written one-id-per-line array onto one
  /// line, so adding a comparison reflowed every fan-in in the file.
  it("keeps a one-id-per-line array one id per line", () => {
    const before = gridIndex(`[
    "bridge/render_markdown",
    "sickbay/render_markdown",
]`);
    const wired = wireIntoFanIns(before, "holodeck/render_markdown");
    expect(wired).toBe(
      gridIndex(`[
    "bridge/render_markdown",
    "sickbay/render_markdown",
    "holodeck/render_markdown",
]`),
    );
    expect(unwireFromFanIns(wired, "holodeck/render_markdown")).toBe(before);
    expect(unwireFromFanIns(before, "bridge/render_markdown", "grid_index")).toBe(
      gridIndex(`[
    "sickbay/render_markdown",
]`),
    );
  });

  it("keeps an array without a trailing comma without one", () => {
    const before = gridIndex(`[
  "bridge/render_markdown",
  "sickbay/render_markdown"
]`);
    const wired = wireIntoFanIns(before, "holodeck/render_markdown");
    expect(wired).toBe(
      gridIndex(`[
  "bridge/render_markdown",
  "sickbay/render_markdown",
  "holodeck/render_markdown"
]`),
    );
    expect(unwireFromFanIns(wired, "holodeck/render_markdown")).toBe(before);
  });

  /// The old comma split read a comment as part of the id after it, and
  /// a `]` inside one ended the array early.
  it("keeps the comments and blank lines inside an array", () => {
    const before = gridIndex(`[ # every render step
  # the crew's own records
  "bridge/render_markdown", # logs [stardate order]

  "sickbay/render_markdown",
  # "holodeck/render_markdown",
]`);
    const wired = wireIntoFanIns(before, "cargo_bay/render_markdown");
    expect(wired).toBe(
      gridIndex(`[ # every render step
  # the crew's own records
  "bridge/render_markdown", # logs [stardate order]

  "sickbay/render_markdown",
  # "holodeck/render_markdown",
  "cargo_bay/render_markdown",
]`),
    );
    expect(unwireFromFanIns(wired, "cargo_bay/render_markdown")).toBe(before);
    expect(unwireFromFanIns(before, "bridge/render_markdown")).toBe(
      gridIndex(`[ # every render step
  # the crew's own records

  "sickbay/render_markdown",
  # "holodeck/render_markdown",
]`),
    );
    expect(
      listSteps(unwireFromFanIns(before, "sickbay/render_markdown")).find(
        (s) => s.id === "unified_grid_and_search_index/grid_index",
      )!.inputs,
    ).toEqual(["bridge/render_markdown"]);
  });

  it("keeps a one-line array on one line", () => {
    const before = gridIndex(`["bridge/render_markdown", "sickbay/render_markdown"]`);
    expect(wireIntoFanIns(before, "holodeck/render_markdown")).toBe(
      gridIndex(
        `["bridge/render_markdown", "sickbay/render_markdown", "holodeck/render_markdown"]`,
      ),
    );
    expect(unwireFromFanIns(before, "bridge/render_markdown", "grid_index")).toBe(
      gridIndex(`["sickbay/render_markdown"]`),
    );
  });

  /// An id is matched by the string it spells, not by how it is quoted.
  it("unwires an id written as a literal string", () => {
    const before = gridIndex(`['bridge/render_markdown', "sickbay/render_markdown"]`);
    expect(unwireFromFanIns(before, "bridge/render_markdown", "grid_index")).toBe(
      gridIndex(`["sickbay/render_markdown"]`),
    );
    expect(wireIntoFanIns(before, "bridge/render_markdown")).toBe(before);
  });

  /// An array the edit cannot lay out is refused rather than written.
  it("refuses to edit an array holding something other than strings", () => {
    const nested = gridIndex(`["bridge/render_markdown", ["sickbay/render_markdown"]]`);
    expect(() => wireIntoFanIns(nested, "holodeck/render_markdown")).toThrow(
      /in an array of strings/,
    );
  });

  /// A multi-line array with nothing in it yet takes the indentation of
  /// its `]`, one step in.
  it("wires into an empty multi-line array", () => {
    expect(wireIntoFanIns(gridIndex(`[\n]`), "bridge/render_markdown")).toBe(
      gridIndex(`[\n  "bridge/render_markdown",\n]`),
    );
  });
});

describe("what Check account is sent", () => {
  /// The form's `<input type=number>` hands back a string, and the
  /// backend's `Option<i64>` will not take `"30"`.
  it("sends numbers as numbers", () => {
    const values = seedFieldValues(SLACK);
    values["api.refresh_window_days"] = "30";
    const params = paramsObject(SLACK, values, "download") as {
      api: { refresh_window_days: unknown };
    };
    expect(params.api.refresh_window_days).toBe(30);
  });
});

describe("what Check account authenticates as", () => {
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

describe("the ChatGPT descriptor", () => {
  /// ChatGPT's form is Claude's with the Claude-only knobs (Projects,
  /// Cowork) left out:
  /// same connection block, same probe, same scoping fields and the same
  /// API settings, writing the same `api` table `datalib-step` dispatches
  /// on; only ChatGPT's listing has pages to cap.
  it("mirrors Claude's connection block", () => {
    expect(CHATGPT.wizard).toBe(true);
    expect(CHATGPT.canProbe).toBe(true);
    expect(CHATGPT.credentialService).toBe("chatgpt");
    // The registration has to be the one the hand-run recipe in
    // docs/user/getting_your_data.md gives, or the two produce services
    // that behave differently under the same name.
    expect(CHATGPT.credentialRegister).toEqual({
      base_api_url: "https://chatgpt.com/",
      login_url: "https://chatgpt.com/auth/login",
      login_flow: "token-capture",
      login_flow_params: {
        tokenUrl: "https://chatgpt.com/api/auth/session",
        tokenField: "accessToken",
      },
    });
    const targets = (CHATGPT.fields ?? []).map((f) => f.target);
    expect(targets).toEqual([
      "latchkey_settings.account",
      "export.path",
      "api.since",
      "api.conv_uuids",
      "api.max_conversations_per_run",
      "api.refetch_newest_conversations",
      "api.seconds_between_requests",
      "api.max_listing_pages",
      "when_deleted_upstream",
    ]);
    const claudeApi = (CLAUDE.fields ?? [])
      .map((f) => f.target)
      .filter((t) => t.startsWith("api.") && !t.startsWith("api.project") && t !== "api.cowork");
    expect(claudeApi.filter((t) => !targets.includes(t))).toEqual([]);
  });

  it("writes an api table even with nothing filled in", () => {
    const params = paramsObject(CHATGPT, seedFieldValues(CHATGPT), "download");
    expect(params).toEqual({ api: {}, when_deleted_upstream: "delete_local_copy" });
  });

  it("keeps a source it wrote editable", () => {
    const values = seedFieldValues(CHATGPT);
    values["api.since"] = "2026-01-01";
    values["api.conv_uuids"] = ["https://chatgpt.com/c/abc", "def"];
    const { stepsBody } = buildSource({
      entry: CHATGPT,
      group: "chatgpt",
      name: "",
      values,
      withGroup: false,
    });
    const [ingest] = listSteps(stepsBody);
    expect(paramsAreRepresentable(ingest, CHATGPT)).toEqual({ ok: true });
  });

  /// An export beside the API is how a large account is copied without
  /// the rate limit, so it is on the API form: answered, it writes an
  /// `export` table beside `api`, and the form reopens on that answer.
  it("writes an export beside the api, and reopens on that answer", () => {
    const values = seedFieldValues(CHATGPT);
    values["export.path"] = "~/Downloads/chatgpt-export.zip";
    expect(paramsObject(CHATGPT, values, "download")).toEqual({
      api: {},
      export: { path: "~/Downloads/chatgpt-export.zip" },
      when_deleted_upstream: "delete_local_copy",
    });
    const { stepsBody } = buildSource({
      entry: CHATGPT,
      group: "chatgpt",
      name: "",
      values,
      withGroup: false,
    });
    const [ingest] = listSteps(stepsBody);
    expect(paramsAreRepresentable(ingest, CHATGPT)).toEqual({ ok: true });

    const section = CHATGPT.sections!.find((s) => s.heading === "Start from an export?")!;
    const answers = section.answers!.map((answer) => ({
      answer,
      fields: (answer.fields ?? []).map((t) => CHATGPT.fields!.find((f) => f.target === t)!),
    }));
    expect(chosenAnswer(answers, values)).toBe(1);
    expect(chosenAnswer(answers, seedFieldValues(CHATGPT))).toBe(0);
  });

  /// A source with both tables is the API seeded from an export, not an
  /// export source: it was labelled "ChatGPT (export)" and opened the
  /// export-only form, which could not show its `api` settings.
  it("is the entry for a step with both tables", () => {
    const both = { api: { since: "2026-05-01" }, export: { path: "x.zip" } };
    expect(catalogForStep("chatgpt", both)?.label).toBe("ChatGPT");
    expect(catalogForStep("chatgpt", { api: {} })?.label).toBe("ChatGPT");
    expect(catalogForStep("chatgpt", { export: { path: "x.zip" } })?.label).toBe(
      "ChatGPT (export)",
    );
  });
});

describe("a source's search steps", () => {
  const MAPPED = `${PAIR}
[[steps]]
group = "unified_grid_and_search_index"
function = "embedding_map"
inputs = ["unified_grid_and_search_index/qmd_aggregator"]
`;
  const inputsOf = (text: string, id: string) => listSteps(text).find((s) => s.id === id)?.inputs;

  /// The `grid_index` step puts the source in the keyword search; its embed reads
  /// the `grid_index` step, and the aggregator reads the embed. Nothing of the
  /// source's reads the aggregator: a step downstream of a fan-in that
  /// waits on every source would finish only when the slowest does.
  it("adds the embed, reading the `grid_index` step, and wires it into the aggregator", () => {
    const next = setSearchSteps(MAPPED, "slack", "keyword_and_embed");
    expect(inputsOf(next, "slack/embed")).toEqual(["unified_grid_and_search_index/grid_index"]);
    expect(inputsOf(next, "unified_grid_and_search_index/qmd_aggregator")).toEqual(["slack/embed"]);
    expect(inputsOf(next, "unified_grid_and_search_index/embedding_map")).toEqual([
      "unified_grid_and_search_index/qmd_aggregator",
    ]);
    expect(next).not.toContain("not_searched");
    expect(setSearchSteps(next, "slack", "keyword_and_embed")).toBe(next);
    expect(searchIndexingOf(listSteps(next), "slack")).toBe("keyword_and_embed");
  });

  /// Keyword search without the slow embeddings: the embed step and the
  /// aggregator's edge to it go, and the source stays searched by its
  /// words. Ticking embeddings back on restores both.
  it("keeps the keyword search when only the embeddings go", () => {
    const both = setSearchSteps(MAPPED, "slack", "keyword_and_embed");
    const keyword = setSearchSteps(both, "slack", "keyword");
    expect(inputsOf(keyword, "slack/embed")).toBeUndefined();
    expect(inputsOf(keyword, "unified_grid_and_search_index/qmd_aggregator")).toEqual([]);
    expect(keyword).not.toContain("not_searched");
    expect(searchIndexingOf(listSteps(keyword), "slack")).toBe("keyword");
    expect(setSearchSteps(keyword, "slack", "keyword")).toBe(keyword);
    const back = setSearchSteps(keyword, "slack", "keyword_and_embed");
    expect(inputsOf(back, "slack/embed")).toEqual(["unified_grid_and_search_index/grid_index"]);
    expect(inputsOf(back, "unified_grid_and_search_index/qmd_aggregator")).toEqual(["slack/embed"]);
  });

  /// A step reading the embeddings would name an input that no longer
  /// exists, which the loader drops it for, so it goes with them.
  it("takes a step that reads the embeddings with them", () => {
    const reader = `${setSearchSteps(MAPPED, "slack", "keyword_and_embed")}
[[steps]]
group = "slack"
function = "cluster"
command = "my-clusterer"
inputs = ["slack/embed"]
`;
    const keyword = setSearchSteps(reader, "slack", "keyword");
    expect(inputsOf(keyword, "slack/cluster")).toBeUndefined();
    expect(inputsOf(keyword, "slack/render_markdown")).toBeDefined();
  });

  /// Turning search off for a source lists it in the `grid_index` step's
  /// `not_searched` and takes its embed and the aggregator's edge to it;
  /// the grid keeps the source.
  it("lists the source as not searched and takes its embed back out", () => {
    const off = setSearchSteps(
      setSearchSteps(MAPPED, "slack", "keyword_and_embed"),
      "slack",
      "none",
    );
    expect(
      listSteps(off).find((s) => s.id === "unified_grid_and_search_index/grid_index")?.params
        ?.not_searched,
    ).toEqual(["slack"]);
    expect(inputsOf(off, "slack/embed")).toBeUndefined();
    expect(inputsOf(off, "unified_grid_and_search_index/qmd_aggregator")).toEqual([]);
    expect(inputsOf(off, "unified_grid_and_search_index/embedding_map")).toEqual([
      "unified_grid_and_search_index/qmd_aggregator",
    ]);
    expect(inputsOf(off, "unified_grid_and_search_index/grid_index")).toEqual([
      "slack/render_markdown",
    ]);
    expect(searchIndexingOf(listSteps(off), "slack")).toBe("none");
  });

  /// Without the aggregator search by meaning is off, and nothing would
  /// sweep what an embed writes, so none is written.
  it("writes no embed where the config has no aggregator", () => {
    const noSearch = removeSteps(
      PAIR,
      listSteps(PAIR).filter((s) => s.id === "unified_grid_and_search_index/qmd_aggregator"),
    );
    expect(
      inputsOf(setSearchSteps(noSearch, "slack", "keyword_and_embed"), "slack/embed"),
    ).toBeUndefined();
  });

  /// A render is wired into the grid index and never into the aggregator
  /// or the map.
  it("wires a render into the grid index only", () => {
    const wired = wireIntoFanIns(MAPPED, "email/render_markdown");
    expect(inputsOf(wired, "unified_grid_and_search_index/grid_index")).toContain(
      "email/render_markdown",
    );
    expect(inputsOf(wired, "unified_grid_and_search_index/qmd_aggregator")).toEqual([]);
    expect(inputsOf(wired, "unified_grid_and_search_index/embedding_map")).toEqual([
      "unified_grid_and_search_index/qmd_aggregator",
    ]);
  });

  /// Removing a source's ingest takes everything that reads it, however
  /// far down. A fan-in only loses the edge.
  it("finds every step reading a removed one, but no fan-in", () => {
    const all = listSteps(setSearchSteps(MAPPED, "slack", "keyword_and_embed"));
    expect(readersOf(["slack/ingest"], all).map((s) => s.id)).toEqual(["slack/render_markdown"]);
  });

  /// Removing the aggregator turns search by meaning off, so every
  /// source's embed goes with it — though none of them reads it.
  it("takes every source's embed with the aggregator", () => {
    const all = listSteps(setSearchSteps(MAPPED, "slack", "keyword_and_embed"));
    expect(readersOf(["unified_grid_and_search_index/qmd_aggregator"], all)).toEqual([]);
    expect(
      removedWith(["unified_grid_and_search_index/qmd_aggregator"], all).map((s) => s.id),
    ).toEqual(["slack/embed"]);
  });

  /// A source's embed reads the `grid_index` step, not its render, but it embeds
  /// that render's documents: it goes when the render does.
  it("takes a source's embed with its render", () => {
    const all = listSteps(setSearchSteps(MAPPED, "slack", "keyword_and_embed"));
    expect(removedWith(["slack/ingest"], all).map((s) => s.id)).toEqual([
      "slack/render_markdown",
      "slack/embed",
    ]);
  });
});

describe("where the wizard writes", () => {
  /// Sources first, each with its steps, then the index they all feed:
  /// every step below the steps it reads.
  const FLOWING = `data_root = "~/datalib"

# ── slack ─────────────────────────────────────────────────────────────
[[groups]]
id = "slack"
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

# ── notes ─────────────────────────────────────────────────────────────
[[groups]]
id = "notes"
type = "slack"

[[steps]]
group = "notes"
function = "ingest"
[steps.params.api]
channels = ["notes"]

[[steps]]
group = "notes"
function = "render_markdown"
inputs = ["notes/ingest"]

# ── the unified grid and search index ─────────────────────────────────
[[groups]]
id = "unified_grid_and_search_index"

[[steps]]
group = "unified_grid_and_search_index"
function = "grid_index"
inputs = ["slack/render_markdown", "notes/render_markdown"]

[[steps]]
group = "unified_grid_and_search_index"
function = "qmd_aggregator"
inputs = []

[[applets]]
group = "unified_grid_and_search_index"
id = "unified_grid_and_search_index"
command = "datalib-applet unified_grid_and_search_index"
`;

  /// Every input a step names that is in the file comes above it.
  function flowsDown(text: string): string[] {
    const ids = listSteps(text).map((s) => s.id);
    return listSteps(text).flatMap((s) =>
      s.inputs
        .filter((i) => ids.includes(i) && ids.indexOf(i) > ids.indexOf(s.id))
        .map((i) => `${s.id} reads ${i}, below it`),
    );
  }

  /// What the Sources card does on "+ Data Source".
  function addSource(text: string, group: string): string {
    const built = buildSource({ entry: SLACK, group, name: "", values: {}, withGroup: true });
    let next = insertEntries(text, `${built.groupBody}\n\n${built.stepsBody}`);
    next = wireIntoFanIns(next, built.renderId!);
    return setSearchSteps(next, group, "keyword_and_embed");
  }

  it("adds a source after the last one, above the index, with its steps together", () => {
    const next = addSource(FLOWING, "extra");
    expect(listGroups(next).map((g) => g.id)).toEqual([
      "slack",
      "notes",
      "extra",
      "unified_grid_and_search_index",
    ]);
    expect(listSteps(next).map((s) => s.id)).toEqual([
      "slack/ingest",
      "slack/render_markdown",
      "notes/ingest",
      "notes/render_markdown",
      "extra/ingest",
      "extra/render_markdown",
      "unified_grid_and_search_index/grid_index",
      "extra/embed",
      "unified_grid_and_search_index/qmd_aggregator",
      "unified_grid_and_search_index",
    ]);
    expect(flowsDown(next)).toEqual([]);
    expect(next.startsWith('data_root = "~/datalib"\n')).toBe(true);
    // Its divider stays above its group, and the index's above the index.
    // Its embed reads the `grid_index` step, so it sits below it, above the
    // aggregator that reads the embed.
    expect(next.indexOf("── extra")).toBeLessThan(next.indexOf('id = "extra"'));
    expect(next.indexOf('function = "embed"')).toBeGreaterThan(
      next.indexOf('function = "grid_index"'),
    );
    expect(next.indexOf('function = "embed"')).toBeLessThan(
      next.indexOf('function = "qmd_aggregator"'),
    );
  });

  /// The scaffold a new root starts from is the index alone; the first
  /// source goes above it, not after it.
  it("puts the first source above the index a new root starts with", () => {
    const scaffold = FLOWING.slice(
      FLOWING.indexOf("# ── the unified grid and search index"),
    ).replace('inputs = ["slack/render_markdown", "notes/render_markdown"]', "inputs = []");
    const next = addSource(scaffold, "extra");
    expect(listGroups(next).map((g) => g.id)).toEqual(["extra", "unified_grid_and_search_index"]);
    expect(flowsDown(next)).toEqual([]);
    expect(next.startsWith("# ── extra")).toBe(true);
  });

  /// Search by meaning turned on for a source that is not the last: its
  /// embed reads the `grid_index` step, so it goes below that and above the
  /// aggregator that reads it, whichever source it belongs to.
  it("puts a source's embed between the `grid_index` step and the aggregator", () => {
    const both = setSearchSteps(FLOWING, "slack", "keyword_and_embed");
    expect(listSteps(both).map((s) => s.id)).toEqual([
      "slack/ingest",
      "slack/render_markdown",
      "notes/ingest",
      "notes/render_markdown",
      "unified_grid_and_search_index/grid_index",
      "slack/embed",
      "unified_grid_and_search_index/qmd_aggregator",
      "unified_grid_and_search_index",
    ]);
    expect(flowsDown(both)).toEqual([]);
  });

  /// An edit rewrites a source where it stands: the table does not
  /// reshuffle because someone changed a channel list.
  it("rewrites an edited source in place", () => {
    const after = patchSource(FLOWING, {
      entry: SLACK,
      group: "slack",
      values: { "api.channels": ["random"] },
    });
    expect(listSteps(after).map((s) => s.id)).toEqual(listSteps(FLOWING).map((s) => s.id));
    expect(after).toContain('channels = ["random"]');
    expect(after).not.toContain('channels = ["general"]');
    expect(after.indexOf('channels = ["random"]')).toBeLessThan(after.indexOf("── notes"));
  });

  /// A comparison reads its source's download, so it goes below that, and
  /// above the index that reads it.
  it("puts a comparison after the sources, above the index", () => {
    const built = buildDiffSource({
      id: "slack-diff",
      name: "",
      source: "slack",
      from: "aaa",
      to: "bbb",
      maxDocuments: 50,
    });
    const next = insertEntries(FLOWING, `${built.groupBody}\n\n${built.stepsBody}`);
    expect(listGroups(next).map((g) => g.id)).toEqual([
      "slack",
      "notes",
      "slack-diff",
      "unified_grid_and_search_index",
    ]);
    expect(flowsDown(wireIntoFanIns(next, built.renderId))).toEqual([]);
  });

  /// A file written with the aggregator above the `grid_index` step has no place
  /// below the one and above the other; the embed goes at the end, as
  /// entries always did there, and the file still loads.
  it("falls back to the end of a file already out of order", () => {
    const gridIndexStep = `[[steps]]
group = "unified_grid_and_search_index"
function = "grid_index"
inputs = ["slack/render_markdown"]

`;
    const swapped = `${PAIR.replace(gridIndexStep, "").replace("# ── slack", `${gridIndexStep}# ── slack`)}`;
    expect(swapped.indexOf("qmd_aggregator")).toBeLessThan(swapped.indexOf("grid_index"));
    const next = setSearchSteps(swapped, "slack", "keyword_and_embed");
    expect(
      listSteps(next)
        .map((s) => s.id)
        .at(-1),
    ).toBe("slack/embed");
  });
});

describe("the latchkey account field", () => {
  /// Picking a stored login, or naming a new one, is the same for every
  /// service — Slack had none at all, and Claude's was hidden.
  it("is there, once, on every source that signs in through latchkey", () => {
    for (const entry of CATALOG.filter((e) => e.credentialService)) {
      const accounts = fieldsFor(entry, "download").filter((f) => f.kind === "text" && f.latchkey);
      expect(
        accounts.map((f) => f.target),
        entry.label,
      ).toEqual(["latchkey_settings.account"]);
    }
  });

  it("is worded for the source when it declares none", () => {
    const [account] = fieldsFor(SLACK, "download");
    expect(account).toMatchObject({ label: "Slack account", target: "latchkey_settings.account" });
  });

  it("never reaches a render step", () => {
    expect(fieldsFor(SLACK, "render").some((f) => f.kind === "text" && f.latchkey)).toBe(false);
  });
});

describe("moveGroup", () => {
  const CONFIG = `data_root = "."

# ── slack ──
[[groups]]
id = "slack"
type = "slack"

[[steps]]
group = "slack"
function = "ingest"

[steps.params.api]
workspace = "enterprise"

[[steps]]
group = "slack"
function = "render_markdown"
inputs = ["slack/ingest"]

# ── claude ──
[[groups]]
id = "claude"
type = "claude"

[[steps]]
group = "claude"
function = "ingest"

[[groups]]
id = "unified_grid_and_search_index"

[[steps]]
group = "unified_grid_and_search_index"
function = "grid_index"
inputs = ["slack/render_markdown"]

# A step filed under slack, written apart from it.
[[steps]]
group = "slack"
function = "keyword_index"
inputs = ["slack/render_markdown"]

[[applets]]
group = "slack"
id = "slack_view"
command = "datalib-applet slack_view"
`;

  const groupOrder = (text: string) => listGroups(text).map((g) => g.id);
  const entryOrder = (text: string) =>
    [...listGroups(text).map((g) => ({ id: `group ${g.id}`, start: g.start }))]
      .concat(listSteps(text).map((s) => ({ id: s.id, start: s.start })))
      .sort((a, b) => a.start - b.start)
      .map((e) => e.id);
  /// Every entry, as the loader would see it, whatever order it is in.
  const entries = (text: string) => ({
    groups: listGroups(text)
      .map(({ id, name, type }) => ({ id, name, type }))
      .sort((a, b) => a.id.localeCompare(b.id)),
    steps: listSteps(text)
      .map(({ id, kind, inputs, params }) => ({ id, kind, inputs, params }))
      .sort((a, b) => a.id.localeCompare(b.id)),
  });

  it("puts a group, with every step under it, above another, and moves nothing else", () => {
    const next = moveGroup(CONFIG, "claude", "slack");
    expect(groupOrder(next)).toEqual(["claude", "slack", "unified_grid_and_search_index"]);
    expect(entryOrder(next)).toEqual([
      "group claude",
      "claude/ingest",
      "group slack",
      "slack/ingest",
      "slack/render_markdown",
      "group unified_grid_and_search_index",
      "unified_grid_and_search_index/grid_index",
      "slack/keyword_index",
      "slack_view",
    ]);
    expect(entries(next)).toEqual(entries(CONFIG));
    expect(next.startsWith('data_root = "."\n\n# ── claude ──\n[[groups]]')).toBe(true);
    expect(next).toContain("# A step filed under slack, written apart from it.\n[[steps]]");
  });

  it("puts a group below the last one when there is nothing to put it above", () => {
    const next = moveGroup(CONFIG, "slack", null);
    expect(groupOrder(next)).toEqual(["claude", "unified_grid_and_search_index", "slack"]);
    expect(entryOrder(next)).toEqual([
      "group claude",
      "claude/ingest",
      "group unified_grid_and_search_index",
      "unified_grid_and_search_index/grid_index",
      "group slack",
      "slack/ingest",
      "slack/render_markdown",
      "slack/keyword_index",
      "slack_view",
    ]);
    expect(entries(next)).toEqual(entries(CONFIG));
    expect(next).toContain('[steps.params.api]\nworkspace = "enterprise"');
  });

  it("gathers a group's steps and applets when moving it down past their neighbours", () => {
    const next = moveGroup(CONFIG, "slack", "unified_grid_and_search_index");
    expect(groupOrder(next)).toEqual(["claude", "slack", "unified_grid_and_search_index"]);
    expect(entryOrder(next)).toEqual([
      "group claude",
      "claude/ingest",
      "group slack",
      "slack/ingest",
      "slack/render_markdown",
      "slack/keyword_index",
      "slack_view",
      "group unified_grid_and_search_index",
      "unified_grid_and_search_index/grid_index",
    ]);
    expect(entries(next)).toEqual(entries(CONFIG));
  });

  it("leaves the text alone for a group above itself or one the config lacks", () => {
    expect(moveGroup(CONFIG, "slack", "slack")).toBe(CONFIG);
    expect(moveGroup(CONFIG, "nope", "slack")).toBe(CONFIG);
    expect(moveGroup(CONFIG, "slack", "nope")).toBe(CONFIG);
  });

  it("refuses a group above one it reads, or below one that reads it", () => {
    expect(moveAgainstDataFlow(CONFIG, "unified_grid_and_search_index", "slack")).toEqual({
      reads: "slack",
    });
    expect(moveAgainstDataFlow(CONFIG, "unified_grid_and_search_index", "claude")).toBeNull();
    expect(moveAgainstDataFlow(CONFIG, "slack", null)).toEqual({
      readBy: "unified_grid_and_search_index",
    });
    expect(moveAgainstDataFlow(CONFIG, "claude", "slack")).toBeNull();
    expect(moveAgainstDataFlow(CONFIG, "claude", null)).toBeNull();
    expect(moveAgainstDataFlow(CONFIG, "slack", "unified_grid_and_search_index")).toBeNull();
  });

  /// A source's embed reads the index group, which reads the source; that
  /// edge alone must not pin every source below the index.
  it("lets a source with an embed move among the sources", () => {
    const embedded = `${CONFIG}
[[steps]]
group = "slack"
function = "embed"
command = "datalib-step"
inputs = ["unified_grid_and_search_index/grid_index"]
`;
    expect(moveAgainstDataFlow(embedded, "slack", "claude")).toBeNull();
    expect(moveAgainstDataFlow(embedded, "slack", null)).toEqual({
      readBy: "unified_grid_and_search_index",
    });
  });
});

describe("a source's time-series step", () => {
  const WATCH = `${PAIR}
# ── garmin ──
[[groups]]
id = "riker"
type = "garmin"

[[steps]]
group = "riker"
function = "ingest"
`;
  const inputsOf = (text: string, id: string) => listSteps(text).find((s) => s.id === id)?.inputs;

  /// A time-series source added without its step and a place in the
  /// index renders documents happily and never reaches the time-series
  /// index, with nothing on screen to say why.
  it("adds the step, the time-series index and the edge between them, once", () => {
    const added = setTimeSeriesStep(WATCH, "riker", "garmin");
    expect(inputsOf(added, "riker/render_time_series")).toEqual(["riker/ingest"]);
    expect(inputsOf(added, "unified_time_series_index/time_series_index")).toEqual([
      "riker/render_time_series",
    ]);
    expect(inputsOf(added, "unified_grid_and_search_index/grid_index")).toContain(
      "riker/render_time_series",
    );
    expect(listGroups(added).map((g) => g.id)).toContain("unified_time_series_index");
    expect(
      listSteps(added).some((s) => s.kind === "applet" && s.id === "unified_time_series_index"),
    ).toBe(true);
    expect(setTimeSeriesStep(added, "riker", "garmin")).toBe(added);
    expect(added).toContain("── slack");
  });

  it("joins an index that is already there", () => {
    const one = setTimeSeriesStep(WATCH, "riker", "garmin");
    const two = setTimeSeriesStep(
      `${one}\n[[groups]]\nid = "troi"\ntype = "garmin"\n\n[[steps]]\ngroup = "troi"\nfunction = "ingest"\n`,
      "troi",
      "garmin",
    );
    expect(inputsOf(two, "unified_time_series_index/time_series_index")).toEqual([
      "riker/render_time_series",
      "troi/render_time_series",
    ]);
    expect(listGroups(two).filter((g) => g.id === "unified_time_series_index")).toHaveLength(1);
  });

  it("leaves a source with no time series alone", () => {
    expect(setTimeSeriesStep(PAIR, "slack", "slack")).toBe(PAIR);
  });

  /// The index it adds reads every source's time series by pattern, so it
  /// stops reading this one when the step goes, with no edge to take out.
  it("is unwired from the time-series index when the source goes", () => {
    const added = setTimeSeriesStep(WATCH, "riker", "garmin");
    expect(added).toContain('inputs = ["*/render_time_series"]');
    const step = listSteps(added).find((s) => s.id === "riker/render_time_series")!;
    const gone = unwireFromFanIns(removeSteps(added, [step]), step.id);
    expect(inputsOf(gone, "unified_time_series_index/time_series_index")).toEqual([]);
    expect(inputsOf(gone, "unified_grid_and_search_index/grid_index")).not.toContain(
      "riker/render_time_series",
    );
  });
});

describe("a calendar source's occurrences step", () => {
  const CAL = `[[groups]]
id = "bridge"
type = "calendar"

[[steps]]
group = "bridge"
function = "ingest"
`;

  it("adds the step reading the source's download, once", () => {
    const added = setOccurrencesStep(CAL, "bridge", "calendar");
    const step = listSteps(added).find((s) => s.id === "bridge/render_occurrences");
    expect(step?.inputs).toEqual(["bridge/ingest"]);
    expect(step?.phase).toBe("render");
    expect(setOccurrencesStep(added, "bridge", "calendar")).toBe(added);
  });

  it("leaves a source of another type alone", () => {
    expect(setOccurrencesStep(PAIR, "slack", "slack")).toBe(PAIR);
  });

  /// Its problem rows reach the problems list only through `grid_index`.
  it("is read by grid_index for its problem rows", () => {
    const added = setOccurrencesStep(`${PAIR}\n${CAL}`, "bridge", "calendar");
    const grid = listSteps(added).find((s) => s.id === "unified_grid_and_search_index/grid_index");
    expect(grid?.inputs).toContain("bridge/render_occurrences");
  });

  /// The wizard edits a source's render step; written first, the
  /// occurrences step must not be taken for it.
  it("is not the render step the wizard edits", () => {
    const first = setOccurrencesStep(CAL, "bridge", "calendar");
    const both = `${first}\n[[steps]]\ngroup = "bridge"\nfunction = "render_markdown"\ninputs = ["bridge/ingest"]\n`;
    expect(sourceStepsOf("bridge", listSteps(both)).render?.function).toBe("render_markdown");
  });
});

describe("a fan-in that reads by pattern", () => {
  const PATTERNED = `[[groups]]
id = "picard"
type = "email"

[[steps]]
group = "picard"
function = "render_markdown"
command = "datalib-step"

[[groups]]
id = "unified_grid_and_search_index"

[[steps]]
group = "unified_grid_and_search_index"
function = "grid_index"
command = "datalib-step"
inputs = ["*/render_markdown"]
`;

  /// The pattern already names every render, so wiring a new one in
  /// leaves the text as it is rather than naming the step beside it.
  it("is left alone when a source is wired in", () => {
    expect(wireIntoFanIns(PATTERNED, "picard/render_markdown")).toBe(PATTERNED);
  });

  /// What the wizard reads off the text is what the loader reads: the
  /// ids the pattern names.
  it("reads the steps the pattern names", () => {
    const grid = listSteps(PATTERNED).find(
      (s) => s.id === "unified_grid_and_search_index/grid_index",
    );
    expect(grid?.inputs).toEqual(["picard/render_markdown"]);
  });
});
