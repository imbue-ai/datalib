// The Manage screen's group row: the aggregation rules from
// docs/dev/plans/groups_and_functions.md, each pinned on its own.

import { describe, expect, it } from "vitest";
import {
  groupLastSynced,
  groupRowKey,
  groupSeeds,
  groupStatus,
  pipelineOrder,
  type ChildStatus,
} from "./groupRows";
import type { EntryKind } from "./sourceSteps";
import type { StatusView } from "./pipelineStatus";

const view = (key: string, at: string | null = null, detail: string | null = null): StatusView => ({
  key,
  label: key,
  at,
  detail,
});

const child = (id: string, key: string, kind: EntryKind = "step", at: string | null = null) => ({
  id,
  kind,
  status: view(key, at),
});

describe("groupRowKey", () => {
  it("cannot collide with an applet that shares the group's id", () => {
    // The scaffold files the `unified_index` applet under the
    // `unified_index` group; both are rows, and the grid keys rows by id.
    expect(groupRowKey("unified_index")).not.toBe("unified_index");
  });
});

describe("pipelineOrder", () => {
  const mk = (id: string, inputs: string[], kind: EntryKind = "step") => ({ id, kind, inputs });

  it("puts a step after the sibling it reads, whatever the config order", () => {
    const out = pipelineOrder([mk("s/render_markdown", ["s/ingest"]), mk("s/ingest", [])]);
    expect(out.map((c) => c.id)).toEqual(["s/ingest", "s/render_markdown"]);
  });

  it("keeps config order between steps that do not read each other", () => {
    const out = pipelineOrder([
      mk("u/grid_index", ["a/render_markdown"]),
      mk("u/qmd_index", ["a/render_markdown"]),
    ]);
    expect(out.map((c) => c.id)).toEqual(["u/grid_index", "u/qmd_index"]);
  });

  it("trails the applets, which are never scheduled", () => {
    const out = pipelineOrder([
      mk("u", [], "applet"),
      mk("u/qmd_index", []),
      mk("u/grid_index", []),
    ]);
    expect(out.map((c) => c.id)).toEqual(["u/qmd_index", "u/grid_index", "u"]);
  });

  it("does not spin on a cycle, which unsaved text can contain", () => {
    const out = pipelineOrder([mk("s/a", ["s/b"]), mk("s/b", ["s/a"])]);
    expect(out.map((c) => c.id)).toEqual(["s/a", "s/b"]);
  });
});

describe("groupStatus", () => {
  it("is empty for a group with nothing under it", () => {
    expect(groupStatus([])).toBeNull();
  });

  it("is running while any child runs, whichever it is", () => {
    const got = groupStatus([child("s/ingest", "succeeded"), child("s/render_markdown", "running")]);
    expect(got?.status.key).toBe("running");
    expect(got?.from).toBe("s/render_markdown");
  });

  it("is failed when any child failed, even if a later one is up to date", () => {
    const got = groupStatus([
      child("s/ingest", "failed"),
      child("s/render_markdown", "skipped_up_to_date"),
    ]);
    expect(got?.status.key).toBe("failed");
    expect(got?.from).toBe("s/ingest");
  });

  it("otherwise reads the last step in pipeline order", () => {
    // The fetch succeeded and the render is still queued: the group
    // has not finished, and the last step is what says so.
    const got = groupStatus([child("s/ingest", "succeeded"), child("s/render_markdown", "queued")]);
    expect(got?.status.key).toBe("queued");
    expect(got?.from).toBe("s/render_markdown");
  });

  it("reads the last step, not a trailing applet", () => {
    const got = groupStatus([
      child("u/grid_index", "succeeded"),
      child("u/qmd_index", "skipped_up_to_date"),
      child("u", "succeeded", "applet"),
    ]);
    expect(got?.from).toBe("u/qmd_index");
  });

  it("falls back to an applet when the group has only applets", () => {
    const got = groupStatus([child("view", "succeeded", "applet")]);
    expect(got?.from).toBe("view");
  });

  it("counts an applet that failed to start as a failure", () => {
    // The group row is the only place an applet's health shows while
    // the group is folded.
    const got = groupStatus([
      child("s/ingest", "succeeded"),
      child("s/render_markdown", "succeeded"),
      child("s_view", "failed", "applet"),
    ]);
    expect(got?.status.key).toBe("failed");
    expect(got?.from).toBe("s_view");
  });

  it("names the child in the detail so the tooltip says where the word came from", () => {
    const withDetail: ChildStatus = {
      id: "s/ingest",
      kind: "step",
      status: view("failed", null, "boom"),
    };
    expect(groupStatus([withDetail])?.status.detail).toBe("s/ingest: boom");
    expect(groupStatus([child("s/ingest", "succeeded")])?.status.detail).toBe("s/ingest");
  });

  it("keeps the child's instant, which feeds Last synced", () => {
    const got = groupStatus([child("s/ingest", "succeeded", "step", "2026-09-10T10:00:00+02:00")]);
    expect(got?.status.at).toBe("2026-09-10T10:00:00+02:00");
  });
});

describe("groupLastSynced", () => {
  it("is the fetch step's instant, even when the render ran later", () => {
    expect(
      groupLastSynced([
        { kind: "step", phase: "ingest", at: "2026-09-10T10:00:00+02:00" },
        { kind: "step", phase: "render", at: "2026-09-10T10:05:00+02:00" },
      ]),
    ).toBe("2026-09-10T10:00:00+02:00");
  });

  it("is null while the fetch step has never run, whatever the render says", () => {
    expect(
      groupLastSynced([
        { kind: "step", phase: "ingest", at: null },
        { kind: "step", phase: "render", at: "2026-09-10T10:05:00+02:00" },
      ]),
    ).toBeNull();
  });

  it("is the newest child's instant for a group with no fetch step", () => {
    // Stamps in different offsets: the comparison is on the instant.
    expect(
      groupLastSynced([
        { kind: "step", phase: "index", at: "2026-09-10T10:00:00+02:00" },
        { kind: "step", phase: "index", at: "2026-09-10T09:30:00+00:00" },
        { kind: "applet", phase: "other", at: null },
      ]),
    ).toBe("2026-09-10T09:30:00+00:00");
  });

  it("is null when nothing has run", () => {
    expect(groupLastSynced([{ kind: "step", phase: "index", at: null }])).toBeNull();
  });
});

describe("groupSeeds", () => {
  const mk = (id: string, inputs: string[], kind: EntryKind = "step") => ({ id, kind, inputs });

  it("is the group's steps with no inputs", () => {
    expect(
      groupSeeds([mk("s/ingest", []), mk("s/render_markdown", ["s/ingest"]), mk("v", [], "applet")], () => false),
    ).toEqual(["s/ingest"]);
  });

  it("leaves out a step the loader dropped", () => {
    expect(groupSeeds([mk("s/ingest", [])], (c) => c.id === "s/ingest")).toEqual([]);
  });

  it("is empty for a group whose steps all read something", () => {
    expect(groupSeeds([mk("u/grid_index", ["a/render_markdown"])], () => false)).toEqual([]);
  });
});
