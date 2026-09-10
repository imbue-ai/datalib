// The two halves of a source's identity: the group's `id` and `name`.
import { describe, expect, it } from "vitest";
import {
  appendSource,
  buildGroup,
  buildStep,
  listSteps,
  renameGroup,
  slugify,
  suggestId,
} from "../src/config/sourceSteps";
import { catalogFor, type CatalogEntry } from "../src/config/catalog";

const SLACK = catalogFor("slack_api") as CatalogEntry;

const UNNAMED = `data_root = "/tmp/data"

[[groups]]
id = "slack"
type = "slack_api"

[[steps]]
group = "slack"
function = "ingest"
[steps.params]
sync = {}

[[steps]]
group = "slack"
function = "render_markdown"
inputs = ["slack/ingest"]
`;

/// The fetch step in a config, which is where a source's name shows.
const fetchStep = (text: string) => {
  const step = listSteps(text).find((e) => e.id === "slack/ingest");
  expect(step, `no slack/ingest in:\n${text}`).toBeTruthy();
  return step!;
};

describe("reading a name", () => {
  it("falls back to the id when the group declares none", () => {
    const step = fetchStep(UNNAMED);
    expect(step.id).toBe("slack/ingest");
    expect(step.name).toBe("slack/ingest");
  });

  it("takes the group's name", () => {
    const named = renameGroup(UNNAMED, "slack", "Work Slack");
    expect(fetchStep(named).id).toBe("slack/ingest");
    expect(fetchStep(named).name).toBe("Work Slack");
    // The render step is the same source, said again.
    expect(listSteps(named).find((e) => e.id === "slack/render_markdown")!.name).toBe(
      "Work Slack (render markdown)",
    );
  });

  /// A `name` written on a step is still honored — a hand-editor may
  /// put one there — and the loader tells them it is not shown.
  it("lets a step's own name beat the group's", () => {
    const text = renameGroup(UNNAMED, "slack", "Work Slack").replace(
      'function = "render_markdown"',
      'function = "render_markdown"\nname = "The markdown"',
    );
    const by = new Map(listSteps(text).map((e) => [e.id, e.name]));
    expect(by.get("slack/ingest")).toBe("Work Slack");
    expect(by.get("slack/render_markdown")).toBe("The markdown");
  });

  it("ignores a blank name rather than showing an empty cell", () => {
    const blank = UNNAMED.replace('id = "slack"', 'id = "slack"\nname = "   "');
    expect(fetchStep(blank).name).toBe("slack/ingest");
  });
});

describe("names on the other kinds of entry", () => {
  // The Pipeline table lists sources, the shared index steps, and
  // applets. The index group can be named like any other; applets
  // cannot — `AppletEntry` is deny_unknown_fields with no `name` key.
  const OTHER = `data_root = "/tmp/data"

[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid_index"
name = "Search index"
inputs = ["slack/render_markdown"]

[[steps]]
group = "unified_index"
function = "qmd_index"
inputs = ["slack/render_markdown"]

[[applets]]
group = "unified_index"
id = "unified_index"
command = "datalib-applet unified_index"
`;

  it("lets a written name beat the shared step's default label", () => {
    const byId = new Map(listSteps(OTHER).map((e) => [e.id, e]));
    expect(byId.get("unified_index/grid_index")?.kind).toBe("step");
    expect(byId.get("unified_index/grid_index")?.name).toBe("Search index");
  });

  it("gives an unnamed shared step its default label", () => {
    const byId = new Map(listSteps(OTHER).map((e) => [e.id, e]));
    expect(byId.get("unified_index/qmd_index")?.name).toBe("Unified Index (QMD)");
  });

  it("labels the applet too, which has no config key to name it", () => {
    const applet = listSteps(OTHER).find((e) => e.kind === "applet");
    expect(applet?.id).toBe("unified_index");
    expect(applet?.name).toBe("Unified Index (Applet)");
  });

  it("still shows an entry the defaults don't know by its id", () => {
    const custom = `[[groups]]
id = "notes"
type = "fsindex"

[[steps]]
group = "notes"
function = "ingest"

[[applets]]
id = "slack"
command = "datalib-applet slack"
`;
    const byId = new Map(listSteps(custom).map((e) => [e.id, e]));
    expect(byId.get("notes/ingest")?.name).toBe("notes/ingest");
    expect(byId.get("slack")?.name).toBe("slack");
  });
});

describe("writing a name", () => {
  /// Round-trip through the same splice the Edit button performs, so
  /// what's asserted is what the config file would actually hold.
  const save = (text: string, name: string) => renameGroup(text, "slack", name);

  it("round-trips through the config text", () => {
    const next = save(UNNAMED, "Work Slack");
    expect(next).toContain('name = "Work Slack"');
    expect(fetchStep(next).name).toBe("Work Slack");
    // The id is untouched: still the tree the step writes.
    expect(fetchStep(next).id).toBe("slack/ingest");
  });

  it("writes no key at all when there is nothing to say", () => {
    expect(save(UNNAMED, "")).not.toContain("name =");
    // A name that only respells the id is not a name.
    expect(save(UNNAMED, "slack")).not.toContain("name =");
    expect(save(UNNAMED, "  slack  ")).not.toContain("name =");
  });

  it("clearing a name removes the key", () => {
    const cleared = save(save(UNNAMED, "Work Slack"), "");
    expect(cleared).not.toContain("name =");
    expect(fetchStep(cleared).name).toBe("slack/ingest");
  });

  it("survives quotes, backslashes and a pasted newline", () => {
    const nasty = 'Thad\'s "work" \\ slack\nsecond line\ttabbed';
    // The real assertion is that the file still parses — an unescaped
    // newline inside a TOML basic string would take the whole config
    // down, not just this key.
    expect(fetchStep(save(UNNAMED, nasty)).name).toBe(nasty);
  });

  /// A new source is written the way the wizard writes it: the group
  /// carries the name, the steps carry none.
  it("lands on the group when a source is created", () => {
    const body = `${buildGroup({ id: "slack-2", name: "Second Slack", type: "slack_api" })}\n\n${buildStep(
      { entry: SLACK, group: "slack-2", phase: "download", values: { "sync.media": true } },
    )}`;
    const next = appendSource(UNNAMED, body);
    expect(next.indexOf('name = "Second Slack"')).toBeLessThan(next.indexOf('group = "slack-2"'));
    expect(listSteps(next).find((s) => s.id === "slack-2/ingest")!.name).toBe("Second Slack");
  });
});

describe("deriving an id from a name", () => {
  it("keeps word order and lowercases", () => {
    // Not `slack-work`: slugifying is not reordering.
    expect(slugify("Work Slack")).toBe("work-slack");
    expect(slugify("Thad's PDFs")).toBe("thad-s-pdfs");
  });

  it("folds accents and collapses runs of punctuation", () => {
    expect(slugify("Café  —  Notes!!")).toBe("cafe-notes");
    expect(slugify("  spaced  out  ")).toBe("spaced-out");
  });

  it("returns empty when nothing survives, for the caller to fall back", () => {
    // A non-Latin script or pure punctuation leaves nothing usable;
    // `suggestId` puts the catalog's default in rather than inventing.
    expect(slugify("日本語")).toBe("");
    expect(slugify("!!!")).toBe("");
    expect(suggestId(new Set(), slugify("日本語"), "slack")).toBe("slack");
  });

  it("caps the length, since this lands inside paths carrying UUIDs", () => {
    expect(slugify("a".repeat(80))).toHaveLength(40);
    // And never leaves a trailing separator after the cut.
    expect(slugify(`${"a".repeat(39)} bcd`)).toBe("a".repeat(39));
  });

  it("suffixes rather than colliding", () => {
    const taken = new Set(["slack", "slack-2"]);
    expect(suggestId(taken, "slack", "slack")).toBe("slack-3");
    expect(suggestId(taken, "work-slack", "slack")).toBe("work-slack");
  });

  it("suffixes a reserved id too", () => {
    // `system` and `unified_index` are directories the pipeline owns;
    // the loader would refuse them, so never propose one.
    expect(suggestId(new Set(), "system", "slack")).toBe("system-2");
    expect(suggestId(new Set(), "unified_index", "slack")).toBe("unified_index-2");
  });
});
