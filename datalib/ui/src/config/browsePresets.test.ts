// The Browse card's column presets: the invariants that make a preset
// safe to be generous, and the one silent-degradation trap.

import { describe, expect, it } from "vitest";
import { browseColumns, browseName, browsePresetTypes, browseQuery } from "./browsePresets";
import { catalogFor } from "./catalog";

describe("browseColumns", () => {
  /// A preset keyed by a type the catalog doesn't have is not an error
  /// — `browseColumns` just falls through to the generic set, so a
  /// misspelling (`sms-backup-restore` for `sms_backup_restore`) costs
  /// that source its tailored columns and says nothing.
  it("names only source types the catalog knows", () => {
    const unknown = browsePresetTypes().filter((t) => !catalogFor(t));
    expect(unknown).toEqual([]);
  });

  /// The text column is the reason the grid is worth looking at. No
  /// preset may drop it, and it reads last because it is the widest.
  it("always ends with the contents column", () => {
    for (const type of [...browsePresetTypes(), "some_type_we_never_heard_of"]) {
      expect(browseColumns(type)!.at(-1)).toBe("snippet");
    }
  });

  /// One source is rarely one kind of thing — Slack has threads and
  /// messages, PDFs have documents and pages — and `kind` is also what
  /// separates content rows from the storage rows render emits.
  it("always leads with the row's kind", () => {
    for (const type of browsePresetTypes()) {
      expect(browseColumns(type)![0]).toBe("kind");
    }
  });

  it("repeats no column", () => {
    for (const type of browsePresetTypes()) {
      const cols = browseColumns(type)!;
      expect(new Set(cols).size).toBe(cols.length);
    }
  });

  /// The unified projection keeps the grid's own defaults: there the
  /// Source column is the whole point, and a preset
  /// tuned for one source would hide them.
  it("has no preset for the index group", () => {
    expect(browseColumns(null)).toBeNull();
  });

  /// A type nobody wrote a preset for still gets a usable card rather
  /// than the unified default — the adaptive rule trims whichever of
  /// these the source leaves empty.
  it("falls back to a generic set for an unknown type", () => {
    expect(browseColumns("brand_new_provider")).toContain("author");
  });

  /// Per-source columns that are only ever set on the handful of
  /// storage rows would read as noise everywhere else. `pdf` is the one
  /// source where a row has a real size and page count of its own.
  it("shows size and item count only where they are per-row facts", () => {
    expect(browseColumns("pdf")).toContain("item_count");
    expect(browseColumns("slack")).not.toContain("item_count");
    expect(browseColumns("slack")).not.toContain("byte_size");
  });

  /// Columns that would be dead weight for a source never appear, which
  /// is the half the adaptive rule cannot do on its own: it hides what
  /// is empty, not what is meaningless.
  it("keeps irrelevant columns out per type", () => {
    expect(browseColumns("github")).not.toContain("channel");
    expect(browseColumns("whatsapp")).not.toContain("project");
    expect(browseColumns("claude")).toContain("org_name");
    expect(browseColumns("slack")).not.toContain("org_name");
  });
});

describe("browseQuery", () => {
  /// A group id is its directory under the data root, and `source_id:`
  /// matches the first segment of a row's `qmd_path` — the same string.
  it("filters on the group id", () => {
    expect(browseQuery("tiny-slack")).toBe("source_id:tiny-slack is:document");
  });

  /// A diff group's browse is every row that moved, the diff columns
  /// leading, and not one row per document.
  it("a diff group browses its changed rows", () => {
    expect(browseQuery("slack-diff", "diff")).toBe("source_id:slack-diff -change:unchanged");
    const cols = browseColumns("diff")!;
    expect(cols.slice(0, 2)).toEqual(["diff_status", "diff_changed_columns"]);
    expect(cols.at(-1)).toBe("snippet");
  });
});

describe("browseName", () => {
  it("names a Browse card for the source's documents, or a diff's changes", () => {
    expect(browseName("Slack")).toBe("Slack documents");
    expect(browseName("Slack", "slack")).toBe("Slack documents");
    expect(browseName("Weekly diff", "diff")).toBe("Weekly diff changes");
  });
});
