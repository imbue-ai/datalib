// Gmail and Fastmail are one step type with two descriptors.
//
// Everything here is about that split holding up in both directions:
// the wizard must *write* a step the backend recognizes as the right
// mode, and it must *read* an existing step back onto the descriptor
// that wrote it. Getting either wrong is quiet — a Gmail source written
// with no `gmail_api` table falls through to the mbox path and fails
// with "no live download mode"; a Fastmail step read back as the
// catch-all `email` entry loses its form and its Edit button.
//
// The expected strings here are the backend's, not this file's:
// `only_extract_labels`, `gmail_api.user_id` and `sync.hostname` come
// from `datalib/backend/etl/providers/email_config/src/lib.rs`, and
// `outlink_format` from `EmailRenderConfig`.
import { describe, expect, it } from "vitest";
import { CATALOG, catalogFor, catalogForStep, entryKey } from "../src/config/catalog";
import {
  buildStep,
  entryForStep,
  listSteps,
  paramsAreRepresentable,
  paramsObject,
  seedFieldValues,
} from "../src/config/sourceSteps";

const GMAIL = CATALOG.find((e) => e.type === "email" && e.variantKey === "gmail_api")!;
const FASTMAIL = CATALOG.find((e) => e.type === "email" && e.variantKey === "sync")!;

describe("the catalog's email variants", () => {
  it("gives each variant a key of its own", () => {
    expect(entryKey(GMAIL)).toBe("email:gmail_api");
    expect(entryKey(FASTMAIL)).toBe("email:sync");
    // The catch-all keeps the bare type, which is what every
    // single-descriptor entry uses.
    expect(entryKey(catalogFor("slack_api")!)).toBe("slack_api");
  });

  it("has no two entries sharing a key", () => {
    const keys = CATALOG.map(entryKey);
    expect(new Set(keys).size).toBe(keys.length);
  });

  it("orders the catch-all after the variants", () => {
    const emails = CATALOG.filter((e) => e.type === "email");
    expect(emails.at(-1)!.variantKey).toBeUndefined();
    expect(emails.slice(0, -1).every((e) => e.variantKey !== undefined)).toBe(true);
  });
});

describe("writing a step", () => {
  it("writes the table that selects Gmail's download mode", () => {
    const toml = buildStep({
      entry: GMAIL,
      id: "gmail/raw",
      name: "Work mail",
      phase: "download",
      values: seedFieldValues(GMAIL),
    });
    // Presence of `gmail_api` is what picks the mode; `user_id = "me"`
    // is the key that makes the table exist and is Gmail's own default.
    expect(toml).toContain("[steps.params.gmail_api]");
    expect(toml).toContain('user_id = "me"');
    expect(toml).toContain('command = "datalib-step download email"');
    expect(toml).not.toContain("sync");
  });

  it("writes Fastmail's JMAP hostname without asking for it", () => {
    const toml = buildStep({
      entry: FASTMAIL,
      id: "fastmail/raw",
      name: "",
      phase: "download",
      values: seedFieldValues(FASTMAIL),
    });
    expect(toml).toContain("[steps.params.sync]");
    expect(toml).toContain('hostname = "api.fastmail.com"');
    expect(toml).not.toContain("gmail_api");
  });

  it("writes the account and label filter a person actually chose", () => {
    const values = seedFieldValues(GMAIL);
    values["latchkey_settings.account"] = "thad@imbue.com";
    values["only_extract_labels"] = ["Inbox", "Work/Projects"];
    values["gmail_api.message_budget"] = "5000";
    const toml = buildStep({
      entry: GMAIL,
      id: "gmail/raw",
      name: "",
      phase: "download",
      values,
    });
    expect(toml).toContain('account = "thad@imbue.com"');
    expect(toml).toContain('only_extract_labels = ["Inbox", "Work/Projects"]');
    expect(toml).toContain("message_budget = 5000");
  });

  it("gives each variant's render step the right webmail outlink", () => {
    for (const [entry, outlink] of [
      [GMAIL, "gmail"],
      [FASTMAIL, "fastmail"],
    ] as const) {
      const toml = buildStep({
        entry,
        id: "mail/rendered_md",
        name: "",
        phase: "render",
        inputs: ["mail/raw"],
        values: seedFieldValues(entry),
      });
      expect(toml).toContain('command = "datalib-step render email"');
      expect(toml).toContain(`outlink_format = "${outlink}"`);
      // The download-mode params must not leak onto the render step:
      // `EmailRenderConfig` is deny_unknown_fields, so one would make
      // the step fail at load rather than at sync.
      expect(toml).not.toContain("gmail_api");
      expect(toml).not.toContain("hostname");
    }
  });

  /// Round trip through the parser, because the assertions above are
  /// about strings and the config is about tables.
  it("produces params the TOML parser reads back as the mode", () => {
    const toml = `data_root = "~/x"\n\n${buildStep({
      entry: FASTMAIL,
      id: "fastmail/raw",
      name: "",
      phase: "download",
      values: seedFieldValues(FASTMAIL),
    })}\n`;
    const [step] = listSteps(toml);
    expect(step.params).toMatchObject({ sync: { hostname: "api.fastmail.com" } });
  });
});

describe("reading a step back", () => {
  const CONFIG = `data_root = "~/datalib"

[[steps]]
id = "gmail/raw"
command = "datalib-step download email"
[steps.params.gmail_api]
user_id = "me"

[[steps]]
id = "gmail/rendered_md"
command = "datalib-step render email"
inputs = ["gmail/raw"]
[steps.params]
outlink_format = "gmail"

[[steps]]
id = "fastmail/raw"
command = "datalib-step download email"
[steps.params.sync]
hostname = "api.fastmail.com"

[[steps]]
id = "fastmail/rendered_md"
command = "datalib-step render email"
inputs = ["fastmail/raw"]
[steps.params]
outlink_format = "fastmail"

[[steps]]
id = "archive/raw"
command = "datalib-step download email"
[steps.params.common]
input_path = "~/takeout/mail.mbox"
`;
  const STEPS = listSteps(CONFIG);
  const byId = (id: string) => STEPS.find((s) => s.id === id)!;

  it("tells a Gmail fetch step from a Fastmail one by its params", () => {
    expect(catalogForStep("email", byId("gmail/raw").params)!.label).toBe("Gmail");
    expect(catalogForStep("email", byId("fastmail/raw").params)!.label).toBe("Fastmail");
  });

  it("falls back to the catch-all for a mode it has no form for", () => {
    const entry = catalogForStep("email", byId("archive/raw").params)!;
    expect(entry.variantKey).toBeUndefined();
    expect(entry.wizard).toBe(false);
  });

  /// A render step's own params say nothing about which service it
  /// renders — that is entirely a property of the step it reads. Without
  /// `entryForStep` reaching through `inputs`, every email render step
  /// would resolve to the formless catch-all.
  it("resolves a render step through the step it reads", () => {
    expect(entryForStep(byId("gmail/rendered_md"), STEPS)!.label).toBe("Gmail");
    expect(entryForStep(byId("fastmail/rendered_md"), STEPS)!.label).toBe("Fastmail");
  });

  it("leaves a fetch step resolved by its own params", () => {
    expect(entryForStep(byId("fastmail/raw"), STEPS)!.label).toBe("Fastmail");
  });

  /// A preset has no field, so without counting presets as known the
  /// grid would disable Edit on every source these descriptors write —
  /// the wizard refusing to reopen its own output.
  it("keeps a step it wrote editable", () => {
    for (const id of ["gmail/raw", "fastmail/raw", "gmail/rendered_md"]) {
      const step = byId(id);
      const entry = entryForStep(step, STEPS)!;
      expect(paramsAreRepresentable(step, entry), id).toEqual({ ok: true });
    }
  });

  /// The gate that keeps the wizard honest still bites: a knob no field
  /// models blocks Edit rather than being silently dropped on save.
  it("still refuses a step carrying something no field models", () => {
    const [step] = listSteps(`[[steps]]
id = "gmail/raw"
command = "datalib-step download email"
[steps.params.gmail_api]
user_id = "me"
quota_units_per_minute = 4000
`);
    const rep = paramsAreRepresentable(step, GMAIL);
    expect(rep.ok).toBe(false);
    expect(rep.ok === false && rep.unknown).toEqual(["gmail_api.quota_units_per_minute"]);
  });
});

describe("the params a probe is sent", () => {
  it("carries the credentials and the mode, as objects rather than TOML", () => {
    const values = seedFieldValues(GMAIL);
    values["latchkey_settings.account"] = "thad@imbue.com";
    expect(paramsObject(GMAIL, values, "download")).toEqual({
      latchkey_settings: { account: "thad@imbue.com" },
      gmail_api: { user_id: "me" },
    });
  });

  it("sends numbers as numbers", () => {
    // The form's `<input type=number>` hands back a string, and the
    // backend's `Option<usize>` will not take `"5000"`.
    const values = seedFieldValues(GMAIL);
    values["gmail_api.message_budget"] = "5000";
    const params = paramsObject(GMAIL, values, "download") as {
      gmail_api: { message_budget: unknown };
    };
    expect(params.gmail_api.message_budget).toBe(5000);
  });

  it("carries Fastmail's hostname, which is the whole of its mode", () => {
    expect(paramsObject(FASTMAIL, seedFieldValues(FASTMAIL), "download")).toEqual({
      sync: { hostname: "api.fastmail.com" },
    });
  });

  /// An empty account means latchkey's unnamed default, which is
  /// addressed by writing no account at all — not by writing "".
  it("omits an account nobody chose", () => {
    const params = paramsObject(GMAIL, seedFieldValues(GMAIL), "download");
    expect(params).not.toHaveProperty("latchkey_settings");
  });
});

describe("the label pickers", () => {
  const field = (entry: typeof GMAIL, target: string) =>
    entry.fields!.find((f) => f.target === target)!;

  it("asks for every label when downloading, and only mailboxes when rendering", () => {
    for (const entry of [GMAIL, FASTMAIL]) {
      const download = field(entry, "only_extract_labels");
      const render = field(entry, "only_render_labels");
      expect(download.kind === "string_list" && download.probe).toBe("labels");
      expect(render.kind === "string_list" && render.probe).toBe("mailboxes");
      // The render filter is a render-step param; putting it on the
      // download step would fail `deny_unknown_fields` at load.
      expect(render.phase).toBe("render");
      expect(download.phase).toBeUndefined();
    }
  });

  it("puts the account on the step that authenticates", () => {
    for (const entry of [GMAIL, FASTMAIL]) {
      const account = field(entry, "latchkey_settings.account");
      expect(account.kind === "text" && account.latchkey).toBe(true);
      expect(account.phase).toBeUndefined();
      expect(entry.credentialService).toBeTruthy();
      expect(entry.canProbe).toBe(true);
    }
  });

  it("names the latchkey services latchkey actually ships", () => {
    // `latchkey services list` — a wrong name here would show an empty
    // account list and a Connect button that fails.
    expect(GMAIL.credentialService).toBe("google-gmail");
    expect(FASTMAIL.credentialService).toBe("fastmail");
  });
});
