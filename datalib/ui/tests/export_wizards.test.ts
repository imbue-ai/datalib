// The forms for Google Takeout, LinkedIn, SMS Backup & Restore and the
// two ways into `contacts`. What each must get right: write the table
// that names its ingest method, and read back the configs people have
// written by hand, which all carry `always_clear_before_ingest`.
import { describe, expect, it } from "vitest";
import { CATALOG, catalogForStep, entryKey } from "../src/config/catalog";
import type { CatalogEntry } from "../src/config/catalog";
import {
  buildStep,
  entryForStep,
  listSteps,
  paramsAreRepresentable,
  seedFieldValues,
} from "../src/config/sourceSteps";
import methods from "../src/config/ingestMethods.json";

const byType = (type: string, variantKey?: string) =>
  CATALOG.find((e) => e.type === type && e.variantKey === variantKey)!;

const toml = (entry: CatalogEntry, values: Record<string, unknown> = {}) =>
  buildStep({
    entry,
    group: entry.defaultName,
    phase: "download",
    values: { ...seedFieldValues(entry), ...values },
  });

const ingestStep = (type: string, params: string) =>
  listSteps(`data_root = "~/datalib"

[[groups]]
id = "s"
type = "${type}"

[[steps]]
group = "s"
function = "ingest"
${params}

[[steps]]
group = "s"
function = "render_markdown"
inputs = ["s/ingest"]
`);

describe("the catalog's contacts variants", () => {
  const CONTACTS = CATALOG.filter((e) => e.type === "contacts");

  it("has a form for every method the backend declares", () => {
    const declared = (methods as Record<string, { path: string }[]>).contacts.map((m) => m.path);
    expect(CONTACTS.map((e) => e.variantKey).sort()).toEqual([...declared].sort());
    expect(CONTACTS.every((e) => e.wizard)).toBe(true);
    expect(CONTACTS.map(entryKey)).toEqual(["contacts:carddav", "contacts:vcf"]);
  });

  it("writes a CardDAV server and a .vcf folder under their own tables", () => {
    const carddav = toml(byType("contacts", "carddav"), {
      "carddav.server_url": "https://contacts.icloud.com/",
      "carddav.addressbooks": ["Enterprise crew"],
    });
    expect(carddav).toContain("[steps.params.carddav]");
    expect(carddav).toContain('server_url = "https://contacts.icloud.com/"');
    expect(carddav).toContain('addressbooks = ["Enterprise crew"]');
    const vcf = toml(byType("contacts", "vcf"), { "vcf.path": "~/backups/crew_vcf" });
    expect(vcf).toContain("[steps.params.vcf]");
    expect(vcf).toContain('path = "~/backups/crew_vcf"');
  });

  it("finds each method's form from the table it carries", () => {
    const carddav = ingestStep(
      "contacts",
      `[steps.params.carddav]\nserver_url = "https://contacts.icloud.com/"`,
    );
    expect(catalogForStep("contacts", carddav[0].params)!.label).toBe("CardDAV contacts");
    expect(entryForStep(carddav[1], carddav)!.label).toBe("CardDAV contacts");
    const vcf = ingestStep("contacts", `[steps.params.vcf]\npath = "~/backups/crew_vcf"`);
    expect(catalogForStep("contacts", vcf[0].params)!.label).toBe("Contact files (.vcf)");
  });
});

describe("Google Takeout", () => {
  const TAKEOUT = byType("google_takeout");
  const FEEDS = [
    "maps_reviews",
    "maps_saved_places",
    "maps_photos",
    "youtube_watch_history",
    "youtube_subscriptions",
    "google_chat",
    "gemini_apps",
    "google_voice",
    "google_voice_include_spam",
  ];

  /// `GoogleTakeoutSync` in google_takeout_config: a feed with no box
  /// here could be neither turned on nor kept on an edit.
  it("has a box for every feed the provider reads, each starting off", () => {
    const targets = TAKEOUT.fields!.map((f) => f.target);
    for (const feed of FEEDS) expect(targets, feed).toContain(`export.${feed}`);
    const seeded = seedFieldValues(TAKEOUT);
    for (const feed of FEEDS) expect(seeded[`export.${feed}`], feed).toBe(false);
  });

  it("writes the feeds ticked, and the spam switch only under Voice", () => {
    const off = toml(TAKEOUT, { "export.path": "~/backups/Takeout", "export.google_chat": true });
    expect(off).toContain("[steps.params.export]");
    expect(off).toContain("google_chat = true");
    expect(off).toContain("google_voice = false");
    expect(off).not.toContain("google_voice_include_spam");
    const on = toml(TAKEOUT, {
      "export.path": "~/backups/Takeout",
      "export.google_voice": true,
      "export.google_voice_include_spam": true,
    });
    expect(on).toContain("google_voice_include_spam = true");
  });

  it("can edit the config the examples show", () => {
    const steps = ingestStep(
      "google_takeout",
      `[steps.params.common]
always_clear_before_ingest = true

[steps.params.export]
path = "~/backups/Takeout"
google_chat = true
google_voice = true
google_voice_include_spam = false
maps_reviews = false
maps_saved_places = false
maps_photos = false
youtube_watch_history = false
youtube_subscriptions = false
gemini_apps = false`,
    );
    expect(paramsAreRepresentable(steps[0], TAKEOUT)).toEqual({ ok: true });
  });
});

describe("LinkedIn", () => {
  const LINKEDIN = byType("linkedin");

  it("writes the export folder and the photo switch", () => {
    const body = toml(LINKEDIN, {
      "export.path": "~/backups/LinkedInDataExport",
      "export.fetch_photos": true,
    });
    expect(body).toContain("[steps.params.export]");
    expect(body).toContain('path = "~/backups/LinkedInDataExport"');
    expect(body).toContain("fetch_photos = true");
  });

  it("can edit a config with the snapshot switch on", () => {
    const steps = ingestStep(
      "linkedin",
      `[steps.params.export]
path = "~/backups/LinkedInDataExport"
fetch_photos = true

[steps.params.common]
always_clear_before_ingest = true`,
    );
    expect(paramsAreRepresentable(steps[0], LINKEDIN)).toEqual({ ok: true });
  });
});

describe("SMS Backup & Restore", () => {
  const SMS = byType("sms_backup_restore");

  it("writes the backup folder under the table that names the method", () => {
    const body = toml(SMS, { "backup.path": "~/backups/SMSBackupRestore" });
    expect(body).toContain("[steps.params.backup]");
    expect(body).toContain('path = "~/backups/SMSBackupRestore"');
  });

  it("can edit the config the examples show", () => {
    const steps = ingestStep(
      "sms_backup_restore",
      `[steps.params.backup]
path = "~/backups/SMSBackupRestore"
[steps.params.common]
always_clear_before_ingest = true`,
    );
    expect(paramsAreRepresentable(steps[0], SMS)).toEqual({ ok: true });
  });
});
