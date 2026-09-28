// The forms for Google Takeout, LinkedIn, SMS Backup & Restore and
// `contacts` from .vcf files. What each must get right: write the table
// that names its ingest method, and read back the configs people have
// written by hand, which all carry `always_clear_before_ingest`.
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

/// Each input here is a complete snapshot, so a new source drops what
/// the next export no longer holds unless someone unticks the box.
describe("the snapshot switch", () => {
  it("starts on for every file-backed form, and is written", () => {
    for (const entry of [
      byType("google_takeout"),
      byType("linkedin"),
      byType("sms_backup_restore"),
      byType("contacts", "vcf"),
    ]) {
      expect(seedFieldValues(entry)["common.always_clear_before_ingest"], entry.label).toBe(true);
      expect(toml(entry), entry.label).toContain("always_clear_before_ingest = true");
    }
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
