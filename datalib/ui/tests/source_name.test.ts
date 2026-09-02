// The two halves of an entry's identity: `id` and `name`.
//
// `id` is the directory under the data root, the stem of the step ids,
// and the prefix inside every `qmd_path` the grid index recorded — so
// it is fixed once written. `name` is the part a person rewrites at
// will. These tests pin the properties that make offering that safe:
//
//   1. a name round-trips through the config text,
//   2. an entry without one reads and writes exactly as before,
//   3. a name full of TOML metacharacters still parses back,
//   4. slugify + suggestId turn a typed name into a usable id.
import { describe, expect, it } from "vitest";
import {
  appendSource,
  buildStep,
  listSteps,
  removeSteps,
  slugify,
  suggestId,
} from "../src/config/sourceSteps";
import { catalogFor, type CatalogEntry } from "../src/config/catalog";

const SLACK = catalogFor("slack_api") as CatalogEntry;

const UNNAMED = `data_root = "/tmp/data"

[[steps]]
id = "slack/raw"
command = "datalib-step download slack_api"
[steps.params]
sync = {}

[[steps]]
id = "slack/rendered_md"
command = "datalib-step render slack_api"
inputs = ["slack/raw"]
`;

/// The fetch step in a config, which is where a source's name lands.
const fetchStep = (text: string) => {
  const step = listSteps(text).find((e) => e.id === "slack/raw");
  expect(step, `no slack/raw in:\n${text}`).toBeTruthy();
  return step!;
};

const withName = (text: string, step: string, name: string) =>
  text.replace(`id = "${step}"`, `id = "${step}"\nname = "${name}"`);

describe("reading a name", () => {
  it("falls back to the id when the step declares none", () => {
    const step = fetchStep(UNNAMED);
    expect(step.id).toBe("slack/raw");
    expect(step.name).toBe("slack/raw");
  });

  it("takes the name the step carries", () => {
    const step = fetchStep(withName(UNNAMED, "slack/raw", "Work Slack"));
    expect(step.id).toBe("slack/raw");
    expect(step.name).toBe("Work Slack");
  });

  /// Each step is named independently now — there is no source to
  /// inherit from, and two siblings can drift apart on purpose.
  it("names each step separately", () => {
    const both = withName(
      withName(UNNAMED, "slack/raw", "Work Slack"),
      "slack/rendered_md",
      "Work Slack markdown",
    );
    const by = new Map(listSteps(both).map((e) => [e.id, e.name]));
    expect(by.get("slack/raw")).toBe("Work Slack");
    expect(by.get("slack/rendered_md")).toBe("Work Slack markdown");
  });

  it("ignores a blank name rather than showing an empty cell", () => {
    expect(fetchStep(withName(UNNAMED, "slack/raw", "   ")).name).toBe("slack/raw");
  });
});

describe("names on the other kinds of entry", () => {
  // The Pipeline table lists sources, the shared index steps, and
  // applets. A name is a property of a *step*, so the fan-ins carry one
  // too; applets cannot — `AppletEntry` is deny_unknown_fields with no
  // `name` key.
  //
  // Neither falls back to its bare id, though. The three entries every
  // config ships get a readable default label from `DEFAULT_NAMES`,
  // which is UI-side precisely because the applet has nowhere in the
  // config to put one. A `name =` someone did write still wins.
  const OTHER = `data_root = "/tmp/data"

[[steps]]
id = "unified_index/grid"
name = "Search index"
command = "datalib-step grid_index"
inputs = ["slack/rendered_md"]

[[steps]]
id = "unified_index/qmd"
command = "datalib-step qmd_index"
inputs = ["slack/rendered_md"]

[[applets]]
id = "unified_index"
command = "datalib-applet unified_index"
`;

  it("lets a written name beat the shared step's default label", () => {
    const byId = new Map(listSteps(OTHER).map((e) => [e.id, e]));
    expect(byId.get("unified_index/grid")?.kind).toBe("step");
    expect(byId.get("unified_index/grid")?.name).toBe("Search index");
  });

  it("gives an unnamed shared step its default label", () => {
    const byId = new Map(listSteps(OTHER).map((e) => [e.id, e]));
    expect(byId.get("unified_index/qmd")?.name).toBe("Unified Index (QMD)");
  });

  it("labels the applet too, which has no config key to name it", () => {
    const applet = listSteps(OTHER).find((e) => e.kind === "applet");
    expect(applet?.id).toBe("unified_index");
    expect(applet?.name).toBe("Unified Index (Applet)");
  });

  it("still shows an entry the defaults don't know by its id", () => {
    const custom = `[[steps]]
id = "notes/raw"
command = "datalib-step download fsindex"

[[applets]]
id = "slack"
command = "datalib-applet slack"
`;
    const byId = new Map(listSteps(custom).map((e) => [e.id, e]));
    expect(byId.get("notes/raw")?.name).toBe("notes/raw");
    expect(byId.get("slack")?.name).toBe("slack");
  });
});

describe("writing a name", () => {
  /// Round-trip through the same splice the Edit button performs, so
  /// what's asserted is what the config file would actually hold.
  const save = (text: string, name: string) => {
    const existing = listSteps(text).find((s) => s.id === "slack/raw");
    const body = buildStep({
      entry: SLACK,
      id: "slack/raw",
      name,
      phase: "download",
      values: { "sync.media": true },
    });
    return existing ? appendSource(removeSteps(text, [existing]), body) : appendSource(text, body);
  };

  it("round-trips through the config text", () => {
    const next = save(UNNAMED, "Work Slack");
    expect(next).toContain('name = "Work Slack"');
    expect(fetchStep(next).name).toBe("Work Slack");
    // The id is untouched: still the tree the step writes.
    expect(fetchStep(next).id).toBe("slack/raw");
  });

  it("writes no key at all when there is nothing to say", () => {
    expect(save(UNNAMED, "")).not.toContain("name =");
    // A name that only respells the id is not a name.
    expect(save(UNNAMED, "slack/raw")).not.toContain("name =");
    expect(save(UNNAMED, "  slack/raw  ")).not.toContain("name =");
  });

  it("clearing a name removes the key", () => {
    const cleared = save(save(UNNAMED, "Work Slack"), "");
    expect(cleared).not.toContain("name =");
    expect(fetchStep(cleared).name).toBe("slack/raw");
  });

  it("survives quotes, backslashes and a pasted newline", () => {
    const nasty = 'Thad\'s "work" \\ slack\nsecond line\ttabbed';
    // The real assertion is that the file still parses — an unescaped
    // newline inside a TOML basic string would take the whole config
    // down, not just this key.
    expect(fetchStep(save(UNNAMED, nasty)).name).toBe(nasty);
  });

  it("lands before the step's params tables", () => {
    const next = save(UNNAMED, "Work Slack");
    expect(next.indexOf("name =")).toBeLessThan(next.indexOf("[steps.params"));
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
