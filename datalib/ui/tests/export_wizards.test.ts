// The forms for Google Takeout, LinkedIn, SMS Backup & Restore and
// `contacts` from .vcf files. What each must get right: write the table
// that names its ingest method, and read back the configs people have
// written by hand, which may still carry the retired
// `always_clear_before_ingest`.
import { describe, expect, it } from "vitest";
import { CATALOG } from "../src/config/catalog";
import type { CatalogEntry } from "../src/config/catalog";
import {
  buildStep,
  listSteps,
  paramsAreRepresentable,
  seedFieldValues,
} from "../src/config/sourceSteps";

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

/// `always_clear_before_ingest` went: no form offers it or writes it, and
/// a config that still carries it stays editable, since saving drops the
/// line the config check warns about.
describe("the retired snapshot switch", () => {
  const FORMS = [
    byType("linkedin"),
    byType("sms_backup_restore"),
    byType("contacts", "vcf"),
    byType("google_takeout"),
  ];

  it("is in no form and in nothing a form writes", () => {
    for (const entry of FORMS) {
      const targets = (entry.fields ?? []).map((f) => f.target);
      expect(targets, entry.label).not.toContain("common.always_clear_before_ingest");
      expect(toml(entry), entry.label).not.toContain("always_clear_before_ingest");
    }
  });

  it("does not block editing a config that still carries it", () => {
    const steps = ingestStep(
      "sms_backup_restore",
      `[steps.params.backup]
path = "~/backups/SMSBackupRestore"
[steps.params.common]
always_clear_before_ingest = true`,
    );
    expect(paramsAreRepresentable(steps[0], byType("sms_backup_restore"))).toEqual({ ok: true });
  });

  it("does not hide a key the form really cannot model", () => {
    const steps = ingestStep(
      "sms_backup_restore",
      `[steps.params.backup]
path = "~/backups/SMSBackupRestore"
[steps.params.common]
always_clear_before_ingest = true
blob_size_limit_bytes = 5`,
    );
    expect(paramsAreRepresentable(steps[0], byType("sms_backup_restore"))).toEqual({
      ok: false,
      unknown: ["common.blob_size_limit_bytes"],
    });
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
  /// here could be neither turned on nor kept on an edit. The provider
  /// defaults every feed off, so the form ticks each one and writes it.
  it("has a box for every feed the provider reads, each starting on", () => {
    const targets = TAKEOUT.fields!.map((f) => f.target);
    for (const feed of FEEDS) expect(targets, feed).toContain(`export.${feed}`);
    const seeded = seedFieldValues(TAKEOUT);
    for (const feed of FEEDS) expect(seeded[`export.${feed}`], feed).toBe(true);
    const added = toml(TAKEOUT, { "export.path": "~/backups/Takeout" });
    for (const feed of FEEDS) expect(added, feed).toContain(`${feed} = true`);
  });

  it("writes the feeds ticked, and the spam switch only under Voice", () => {
    const off = toml(TAKEOUT, { "export.path": "~/backups/Takeout", "export.google_voice": false });
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

  it("can edit a config that still turns the wipe on", () => {
    const steps = ingestStep(
      "google_takeout",
      `[steps.params.common]
always_clear_before_ingest = true

[steps.params.export]
path = "~/backups/Takeout"`,
    );
    expect(paramsAreRepresentable(steps[0], TAKEOUT)).toEqual({ ok: true });
  });

  it("can edit the config the examples show", () => {
    const steps = ingestStep(
      "google_takeout",
      `[steps.params.export]
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

  it("writes the export folder and no photo switch", () => {
    const body = toml(LINKEDIN, { "export.path": "~/backups/LinkedInDataExport" });
    expect(body).toContain("[steps.params.export]");
    expect(body).toContain('path = "~/backups/LinkedInDataExport"');
    expect(body).not.toContain("fetch_photos");
  });

  it("can edit a config that still has the retired switches on", () => {
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
path = "~/backups/SMSBackupRestore"`,
    );
    expect(paramsAreRepresentable(steps[0], SMS)).toEqual({ ok: true });
  });
});
