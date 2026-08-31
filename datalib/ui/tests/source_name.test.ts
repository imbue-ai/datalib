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
  buildStepPair,
  listConfiguredSources,
  removeSource,
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

/// The single source in a config, asserted to be the only entry.
const one = (text: string) => {
  const entries = listConfiguredSources(text);
  expect(entries).toHaveLength(1);
  return entries[0];
};

const withName = (text: string, step: string, name: string) =>
  text.replace(`id = "${step}"`, `id = "${step}"\nname = "${name}"`);

describe("reading a name", () => {
  it("falls back to the id when no step declares one", () => {
    const source = one(UNNAMED);
    expect(source.id).toBe("slack");
    expect(source.name).toBe("slack");
  });

  it("takes the name off whichever step carries it", () => {
    const source = one(withName(UNNAMED, "slack/raw", "Work Slack"));
    expect(source.id).toBe("slack");
    expect(source.name).toBe("Work Slack");
  });

  it("takes the first when both steps carry one", () => {
    const both = withName(withName(UNNAMED, "slack/raw", "Work Slack"), "slack/rendered_md", "Ignored");
    expect(one(both).name).toBe("Work Slack");
  });

  it("ignores a blank name rather than showing an empty cell", () => {
    expect(one(withName(UNNAMED, "slack/raw", "   ")).name).toBe("slack");
  });
});

describe("names on the other kinds of entry", () => {
  // The Pipeline table lists sources, the shared index steps, and
  // applets. A name is a property of a *step*, so the fan-ins carry one
  // too; applets cannot — `AppletEntry` is deny_unknown_fields with no
  // `name` key — and fall back to their id.
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

  it("names a shared index step, and leaves an unnamed one alone", () => {
    const byId = new Map(listConfiguredSources(OTHER).map((e) => [e.id, e]));
    expect(byId.get("unified_index/grid")?.kind).toBe("step");
    expect(byId.get("unified_index/grid")?.name).toBe("Search index");
    expect(byId.get("unified_index/qmd")?.name).toBe("unified_index/qmd");
  });

  it("shows an applet by its id", () => {
    const applet = listConfiguredSources(OTHER).find((e) => e.kind === "applet");
    expect(applet?.id).toBe("unified_index");
    expect(applet?.name).toBe("unified_index");
  });
});

describe("writing a name", () => {
  /// Round-trip through the same splice the Edit button performs, so
  /// what's asserted is what the config file would actually hold.
  const save = (text: string, id: string, name: string) => {
    const existing = listConfiguredSources(text).find((s) => s.id === id);
    const body = buildStepPair(SLACK, id, name, { "sync.media": true });
    return existing ? appendSource(removeSource(text, existing), body) : appendSource(text, body);
  };

  it("round-trips through the config text", () => {
    const next = save(UNNAMED, "slack", "Work Slack");
    expect(next).toContain('name = "Work Slack"');
    expect(one(next).name).toBe("Work Slack");
    // The id is untouched: still the directory and the step-id stem.
    expect(one(next).id).toBe("slack");
    expect(next).toContain('id = "slack/raw"');
  });

  it("writes no key at all when there is nothing to say", () => {
    expect(save(UNNAMED, "slack", "")).not.toContain("name =");
    // A name that only respells the id is not a name.
    expect(save(UNNAMED, "slack", "slack")).not.toContain("name =");
    expect(save(UNNAMED, "slack", "  slack  ")).not.toContain("name =");
  });

  it("clearing a name removes the key", () => {
    const cleared = save(save(UNNAMED, "slack", "Work Slack"), "slack", "");
    expect(cleared).not.toContain("name =");
    expect(one(cleared).name).toBe("slack");
  });

  it("survives quotes, backslashes and a pasted newline", () => {
    const nasty = 'Thad\'s "work" \\ slack\nsecond line\ttabbed';
    // The real assertion is that the file still parses — an unescaped
    // newline inside a TOML basic string would take the whole config
    // down, not just this key.
    expect(one(save(UNNAMED, "slack", nasty)).name).toBe(nasty);
  });

  it("lands on the download step, before its params tables", () => {
    const next = save(UNNAMED, "slack", "Work Slack");
    expect(next.indexOf("name =")).toBeLessThan(next.indexOf("[steps.params"));
    expect(next.indexOf("name =")).toBeLessThan(next.indexOf('id = "slack/rendered_md"'));
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
