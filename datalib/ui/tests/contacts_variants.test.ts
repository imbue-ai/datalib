// Google, Fastmail, CardDAV and .vcf are one `contacts` step type with four
// descriptors, told apart by which method table the step carries.
// Written the wrong way, a step names no method and `datalib-step`
// refuses it; read back the wrong way, it loses its form.
import { describe, expect, it } from "vitest";
import { CATALOG, catalogForStep, entryKey } from "../src/config/catalog";
import { buildStep, listSteps, seedFieldValues } from "../src/config/sourceSteps";
import methods from "../src/config/ingestMethods.json";

const CONTACTS = CATALOG.filter((e) => e.type === "contacts");
const byKey = (k: string) => CONTACTS.find((e) => e.variantKey === k)!;

describe("the catalog's contacts variants", () => {
  it("has a form for every method the backend declares", () => {
    const declared = (methods as Record<string, { path: string }[]>).contacts.map((m) => m.path);
    expect(CONTACTS.map((e) => e.variantKey).sort()).toEqual([...declared].sort());
    expect(CONTACTS.every((e) => e.wizard)).toBe(true);
    expect(CONTACTS.map(entryKey)).toEqual([
      "contacts:google",
      "contacts:fastmail",
      "contacts:carddav",
      "contacts:vcf",
    ]);
  });

  it("offers a picker of the account's address books wherever there is a server", () => {
    for (const key of ["fastmail", "carddav"]) {
      const entry = byKey(key);
      const field = entry.fields!.find((f) => f.target === `${key}.addressbooks`)!;
      expect(entry.canProbe, key).toBe(true);
      expect(field.kind === "string_list" && field.probe, key).toBe("addressbooks");
    }
    expect(byKey("vcf").canProbe).toBeFalsy();
  });

  it("signs in to Google with a service of its own, and picks labels", () => {
    const google = byKey("google");
    expect(google.credentialService).toBe("google-people");
    expect(google.canProbe).toBe(true);
    expect(google.fields!.map((f) => f.target)).toEqual([
      "latchkey_settings.account",
      "google.labels",
    ]);
    const labels = google.fields!.find((f) => f.target === "google.labels")!;
    expect(labels.kind === "string_list" && labels.probe).toBe("labels");
  });

  it("logs in to Fastmail with the app password Fastmail Calendar uses", () => {
    const calendar = CATALOG.find((e) => e.type === "calendar" && e.variantKey === "fastmail")!;
    expect(byKey("fastmail").credentialService).toBe("fastmail-dav");
    expect(byKey("fastmail").credentialService).toBe(calendar.credentialService);
  });
});

describe("writing and reading a step", () => {
  it("writes the method table even when nothing is filled in", () => {
    for (const key of ["google", "fastmail"]) {
      const toml = buildStep({
        entry: byKey(key),
        group: key,
        phase: "download",
        values: seedFieldValues(byKey(key)),
      });
      expect(toml).toContain(`${key} = {}`);
    }
  });

  it("writes a CardDAV server and a .vcf folder under their own tables", () => {
    const write = (key: string, values: Record<string, unknown>) =>
      buildStep({
        entry: byKey(key),
        group: key,
        phase: "download",
        values: { ...seedFieldValues(byKey(key)), ...values },
      });
    const carddav = write("carddav", {
      "carddav.server_url": "https://contacts.icloud.com/",
      "carddav.addressbooks": ["Enterprise crew"],
    });
    expect(carddav).toContain("[steps.params.carddav]");
    expect(carddav).toContain('server_url = "https://contacts.icloud.com/"');
    expect(carddav).toContain('addressbooks = ["Enterprise crew"]');
    const vcf = write("vcf", { "vcf.path": "~/backups/crew_vcf" });
    expect(vcf).toContain("[steps.params.vcf]");
    expect(vcf).toContain('path = "~/backups/crew_vcf"');
    const google = write("google", { "google.labels": ["Bridge crew"] });
    expect(google).toContain("[steps.params.google]");
    expect(google).toContain('labels = ["Bridge crew"]');
  });

  it("finds each method's form from the table it carries", () => {
    const steps = listSteps(`data_root = "~/datalib"
${["g", "f", "c", "v"].map((id) => `\n[[groups]]\nid = "${id}"\ntype = "contacts"\n`).join("")}
[[steps]]
id = "g/ingest"
[steps.params.google]

[[steps]]
id = "f/ingest"
[steps.params.fastmail]
addressbooks = ["Crew"]

[[steps]]
id = "c/ingest"
[steps.params.carddav]
server_url = "https://contacts.icloud.com/"

[[steps]]
id = "v/ingest"
[steps.params.vcf]
path = "~/backups/contacts"
`);
    for (const [id, label] of [
      ["g/ingest", "Google Contacts"],
      ["f/ingest", "Fastmail Contacts"],
      ["c/ingest", "Contacts (CardDAV)"],
      ["v/ingest", "Contacts (.vcf files)"],
    ]) {
      const step = steps.find((s) => s.id === id)!;
      expect(catalogForStep("contacts", step.params)!.label, id).toBe(label);
    }
  });
});
