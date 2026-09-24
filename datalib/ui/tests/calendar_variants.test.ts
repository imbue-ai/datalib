// Google, Fastmail, CalDAV and .ics are one `calendar` step type with
// four descriptors, told apart by which method table the step carries.
// Written the wrong way, a step names no method and `datalib-step`
// refuses it; read back the wrong way, it loses its form.
import { describe, expect, it } from "vitest";
import { CATALOG, catalogForStep, entryKey } from "../src/config/catalog";
import { buildStep, entryForStep, listSteps, seedFieldValues } from "../src/config/sourceSteps";
import methods from "../src/config/ingestMethods.json";

const CALENDARS = CATALOG.filter((e) => e.type === "calendar");
const byKey = (k: string) => CALENDARS.find((e) => e.variantKey === k)!;

describe("the catalog's calendar variants", () => {
  it("has a form for every method the backend declares", () => {
    const declared = (methods as Record<string, { path: string }[]>).calendar.map((m) => m.path);
    expect(CALENDARS.map((e) => e.variantKey).sort()).toEqual([...declared].sort());
    expect(CALENDARS.every((e) => e.wizard)).toBe(true);
    expect(CALENDARS.map(entryKey)).toEqual([
      "calendar:google",
      "calendar:fastmail",
      "calendar:caldav",
      "calendar:ics",
    ]);
  });

  it("offers a picker of the account's calendars wherever there is an account", () => {
    for (const key of ["google", "fastmail", "caldav"]) {
      const entry = byKey(key);
      const field = entry.fields!.find((f) => f.target === `${key}.calendars`)!;
      expect(entry.canProbe, key).toBe(true);
      expect(field.kind === "string_list" && field.probe, key).toBe("calendars");
    }
    expect(byKey("ics").canProbe).toBeFalsy();
  });

  it("names the latchkey services latchkey actually ships", () => {
    expect(byKey("google").credentialService).toBe("google-calendar");
    expect(byKey("fastmail").credentialService).toBe("fastmail-dav");
  });
});

describe("writing a step", () => {
  const toml = (key: string, values: Record<string, unknown> = {}) =>
    buildStep({
      entry: byKey(key),
      group: key,
      phase: "download",
      values: { ...seedFieldValues(byKey(key)), ...values },
    });

  // Presence names the method, so an empty form still writes the table.
  it("writes the method table even when nothing is filled in", () => {
    expect(toml("google")).toContain("google = {}");
    expect(toml("fastmail")).toContain("fastmail = {}");
  });

  it("writes a CalDAV server and an .ics folder under their own tables", () => {
    const caldav = toml("caldav", {
      "caldav.server_url": "https://caldav.icloud.com/",
      "caldav.calendars": ["Home"],
    });
    expect(caldav).toContain("[steps.params.caldav]");
    expect(caldav).toContain('server_url = "https://caldav.icloud.com/"');
    expect(caldav).toContain('calendars = ["Home"]');
    const ics = toml("ics", { "ics.path": "~/Takeout/Calendar" });
    expect(ics).toContain("[steps.params.ics]");
    expect(ics).toContain('path = "~/Takeout/Calendar"');
  });
});

describe("reading a step back", () => {
  const STEPS = listSteps(`data_root = "~/datalib"
${["g", "f", "c", "i"].map((id) => `\n[[groups]]\nid = "${id}"\ntype = "calendar"\n`).join("")}
[[steps]]
group = "g"
function = "ingest"
[steps.params.google]

[[steps]]
group = "g"
function = "render_markdown"
inputs = ["g/ingest"]

[[steps]]
group = "f"
function = "ingest"
[steps.params.fastmail]
calendars = ["Work"]

[[steps]]
group = "c"
function = "ingest"
[steps.params.caldav]
server_url = "https://caldav.icloud.com/"

[[steps]]
group = "i"
function = "ingest"
[steps.params.ics]
path = "~/Takeout/Calendar"
`);
  const byId = (id: string) => STEPS.find((s) => s.id === id)!;

  it("finds each method's form from the table it carries", () => {
    for (const [id, label] of [
      ["g/ingest", "Google Calendar"],
      ["f/ingest", "Fastmail Calendar"],
      ["c/ingest", "CalDAV"],
      ["i/ingest", "Calendar files (.ics)"],
    ]) {
      expect(catalogForStep("calendar", byId(id).params)!.label, id).toBe(label);
    }
  });

  it("resolves a render step through the step it reads", () => {
    expect(entryForStep(byId("g/render_markdown"), STEPS)!.label).toBe("Google Calendar");
  });
});
