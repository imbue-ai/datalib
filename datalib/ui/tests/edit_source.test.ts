// Editing a source from the wizard writes only the keys its form owns and
// whose value changed (`patchSource`, over `tomlPatch.ts`); everything
// else a person wrote into the source's entries is kept as written.
import { describe, expect, it } from "vitest";
import { deleteIn, setIn } from "../src/config/tomlPatch";
import {
  buildSource,
  insertEntries,
  listSteps,
  patchSource,
  seedFieldValues,
  sourceStepsOf,
  untouchedSettings,
} from "../src/config/sourceSteps";
import { CATALOG, catalogFor } from "../src/config/catalog";

const SLACK = catalogFor("slack")!;

const STEP = { array: "steps", index: 0 };

describe("setIn", () => {
  it("replaces a value where it stands, keeping comments around it", () => {
    const text = `[[steps]]
group = "s"
# the channels we care about
[steps.params.api]
channels = ["a"] # trailing
media = false
`;
    expect(setIn(text, STEP, ["params", "api", "channels"], '["b"]')).toBe(
      text.replace('["a"]', '["b"]'),
    );
  });

  it("adds a key to the [header] table that holds its siblings", () => {
    const text = `[[steps]]
group = "s"
[steps.params.api]
channels = ["a"] # trailing

[[steps]]
group = "t"
`;
    expect(setIn(text, STEP, ["params", "api", "media"], "true")).toBe(
      text.replace("# trailing\n", "# trailing\nmedia = true\n"),
    );
  });

  it("adds a key inside an inline table", () => {
    const text = `[[steps]]\ngroup = "s"\n[steps.params]\napi = {}\nweb = { a = 1 }\n`;
    let next = setIn(text, STEP, ["params", "api", "media"], "true");
    expect(next).toContain("api = { media = true }");
    next = setIn(next, STEP, ["params", "web", "b"], "2");
    expect(next).toContain("web = { a = 1, b = 2 }");
  });

  it("adds a key beside the dotted keys that made its table", () => {
    const text = `[[steps]]\ngroup = "s"\n[steps.params]\napi.channels = ["a"]\nother = 1\n`;
    expect(setIn(text, STEP, ["params", "api", "media"], "true")).toBe(
      `[[steps]]\ngroup = "s"\n[steps.params]\napi.channels = ["a"]\napi.media = true\nother = 1\n`,
    );
  });

  it("starts a table of its own, inside the entry, when none exists", () => {
    const text = `[[steps]]\ngroup = "s"\nfunction = "ingest"\n\n[[steps]]\ngroup = "t"\n`;
    const next = setIn(text, STEP, ["params", "api", "media"], "true");
    expect(next).toBe(
      `[[steps]]\ngroup = "s"\nfunction = "ingest"\n\n[steps.params.api]\nmedia = true\n\n[[steps]]\ngroup = "t"\n`,
    );
    expect(listSteps(next)[1].params).toEqual({});
  });

  it("refuses an edit that cannot be made without changing something else", () => {
    const text = `[[steps]]\ngroup = "s"\n[steps.params]\napi = true\n`;
    expect(() => setIn(text, STEP, ["params", "api", "media"], "true")).toThrow();
  });
});

describe("deleteIn", () => {
  it("removes a key's whole line, trailing comment and all", () => {
    const text = `[[steps]]\n[steps.params.api]\n# about media\nmedia = true # yes\nchannels = []\n`;
    expect(deleteIn(text, STEP, ["params", "api", "media"])).toBe(
      `[[steps]]\n[steps.params.api]\n# about media\nchannels = []\n`,
    );
  });

  it("removes a key from an inline table, with its comma", () => {
    const text = `[[steps]]\n[steps.params]\napi = { a = 1, b = 2, c = 3 }\n`;
    expect(deleteIn(text, STEP, ["params", "api", "b"])).toContain("api = { a = 1, c = 3 }");
    expect(deleteIn(text, STEP, ["params", "api", "c"])).toContain("api = { a = 1, b = 2 }");
    const one = `[[steps]]\n[steps.params]\napi = { a = 1 }\n`;
    expect(deleteIn(one, STEP, ["params", "api", "a"])).toContain("api = {}");
  });

  it("leaves a key the entry does not hold alone", () => {
    const text = `[[steps]]\n[steps.params.api]\nmedia = true\n`;
    expect(deleteIn(text, STEP, ["params", "api", "nope"])).toBe(text);
  });
});

/** A Slack source with things the form has no field for. */
const HAND_EDITED = `data_root = "~/datalib"

# ── slack ────────────────────────────────────────────────
[[groups]]
id = "slack"
type = "slack"

[[steps]]
group = "slack"
function = "ingest"
code_version = "2"
env = { RUST_LOG = "debug" }
# Only the channels that matter.
[steps.params.api]
channels = ["general"] # keep this short
media = true
[steps.params.common]
blob_size_limit_bytes = 5_000_000
download_params = { maximum_sequential_failed_requests = 3 }

[[steps]]
group = "slack"
function = "render_markdown"
inputs = ["slack/ingest", "extra/ingest"]
`;

function seeded(text: string) {
  return seedFieldValues(SLACK, sourceStepsOf("slack", listSteps(text)));
}

describe("patchSource", () => {
  it("saving an unchanged form leaves the file exactly as it was", () => {
    expect(
      patchSource(HAND_EDITED, { entry: SLACK, group: "slack", values: seeded(HAND_EDITED) }),
    ).toBe(HAND_EDITED);
  });

  it("changes the one line whose value changed, and keeps everything else", () => {
    const values = { ...seeded(HAND_EDITED), "api.channels": ["general", "random"] };
    const after = patchSource(HAND_EDITED, { entry: SLACK, group: "slack", values });
    expect(after).toBe(
      HAND_EDITED.replace(
        'channels = ["general"] # keep this short',
        'channels = ["general", "random"] # keep this short',
      ),
    );
  });

  it("removes a key the form cleared, and nothing else", () => {
    const values = { ...seeded(HAND_EDITED), "api.media": false };
    const after = patchSource(HAND_EDITED, { entry: SLACK, group: "slack", values });
    // The size cap only applies while media is on, so it goes with it.
    expect(after).toContain("media = false");
    expect(after).not.toContain("blob_size_limit_bytes");
    expect(after).toContain("download_params = { maximum_sequential_failed_requests = 3 }");
    expect(after).toContain('env = { RUST_LOG = "debug" }');
    expect(after).toContain("# Only the channels that matter.");
  });

  it("removes a render step the form turned off, and keeps the download as written", () => {
    const after = patchSource(HAND_EDITED, {
      entry: SLACK,
      group: "slack",
      values: seeded(HAND_EDITED),
      renders: false,
    });
    expect(listSteps(after).map((s) => s.id)).toEqual(["slack/ingest"]);
    expect(after).toContain('code_version = "2"');
  });

  /// The account field every latchkey source gets without declaring it
  /// was not read back into the form, so an edit wrote the source without
  /// its account.
  it("keeps the latchkey account through an edit", () => {
    const text = HAND_EDITED.replace(
      "[steps.params.api]",
      '[steps.params.latchkey_settings]\naccount = "work"\n[steps.params.api]',
    );
    expect(seeded(text)["latchkey_settings.account"]).toBe("work");
    expect(patchSource(text, { entry: SLACK, group: "slack", values: seeded(text) })).toBe(text);
  });

  it("re-adds the method table when the last key under it goes", () => {
    const text = `[[groups]]\nid = "slack"\ntype = "slack"\n\n[[steps]]\ngroup = "slack"\nfunction = "ingest"\n[steps.params]\napi.channels = ["a"]\n`;
    const values = { ...seeded(text), "api.channels": [] };
    const after = patchSource(text, { entry: SLACK, group: "slack", values, renders: false });
    expect(listSteps(after)[0].params).toHaveProperty("api");
  });

  /// A download-only source written before such sources got a render
  /// step gains one on its next save, so its problems reach the index.
  it("gives a download-only source its missing render step", () => {
    const text = `[[groups]]\nid = "lin"\ntype = "linear"\n\n[[steps]]\ngroup = "lin"\nfunction = "ingest"\n[steps.params]\napi = {}\n`;
    const linear = catalogFor("linear")!;
    const after = patchSource(text, {
      entry: linear,
      group: "lin",
      values: seedFieldValues(linear, sourceStepsOf("lin", listSteps(text))),
      renders: false,
    });
    expect(listSteps(after).map((s) => s.id)).toEqual(["lin/ingest", "lin/render_markdown"]);
  });

  /// Every form, written by the wizard and reopened, saves back to the
  /// same bytes: an edit that changes nothing writes nothing.
  it("round-trips every wizard's own output unchanged", () => {
    for (const entry of CATALOG.filter((e) => e.wizard)) {
      const values = seedFieldValues(entry);
      const out = buildSource({ entry, group: "src", name: "", values, withGroup: true });
      const text = insertEntries(`data_root = "~/d"\n`, `${out.groupBody}\n\n${out.stepsBody}`);
      const reread = seedFieldValues(entry, sourceStepsOf("src", listSteps(text)));
      expect(patchSource(text, { entry, group: "src", values: reread }), entry.label).toBe(text);
    }
  });
});

describe("untouchedSettings", () => {
  it("names what the form has no field for, step by step", () => {
    expect(untouchedSettings(HAND_EDITED, "slack", SLACK)).toEqual([
      {
        step: "slack/ingest",
        keys: [
          "code_version",
          "env",
          "params.common.download_params.maximum_sequential_failed_requests",
        ],
      },
    ]);
  });
});
