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
    const out = pipelineOrder([mk("s/rendered_md", ["s/raw"]), mk("s/raw", [])]);
    expect(out.map((c) => c.id)).toEqual(["s/raw", "s/rendered_md"]);
  });

  it("keeps config order between steps that do not read each other", () => {
    const out = pipelineOrder([
      mk("u/grid", ["a/rendered_md"]),
      mk("u/qmd", ["a/rendered_md"]),
    ]);
    expect(out.map((c) => c.id)).toEqual(["u/grid", "u/qmd"]);
  });

  it("trails the applets, which are never scheduled", () => {
    const out = pipelineOrder([
      mk("u", [], "applet"),
      mk("u/qmd", []),
      mk("u/grid", []),
    ]);
    expect(out.map((c) => c.id)).toEqual(["u/qmd", "u/grid", "u"]);
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
    const got = groupStatus([child("s/raw", "succeeded"), child("s/rendered_md", "running")]);
    expect(got?.status.key).toBe("running");
    expect(got?.from).toBe("s/rendered_md");
  });

  it("is failed when any child failed, even if a later one is up to date", () => {
    const got = groupStatus([
      child("s/raw", "failed"),
      child("s/rendered_md", "skipped_up_to_date"),
    ]);
    expect(got?.status.key).toBe("failed");
    expect(got?.from).toBe("s/raw");
  });

  it("otherwise reads the last step in pipeline order", () => {
    // The fetch succeeded and the render is still queued: the group
    // has not finished, and the last step is what says so.
    const got = groupStatus([child("s/raw", "succeeded"), child("s/rendered_md", "queued")]);
    expect(got?.status.key).toBe("queued");
    expect(got?.from).toBe("s/rendered_md");
  });

  it("reads the last step, not a trailing applet", () => {
    const got = groupStatus([
      child("u/grid", "succeeded"),
      child("u/qmd", "skipped_up_to_date"),
      child("u", "succeeded", "applet"),
    ]);
    expect(got?.from).toBe("u/qmd");
  });

  it("falls back to an applet when the group has only applets", () => {
    const got = groupStatus([child("view", "succeeded", "applet")]);
    expect(got?.from).toBe("view");
  });

  it("counts an applet that failed to start as a failure", () => {
    // The group row is the only place an applet's health shows while
    // the group is folded.
    const got = groupStatus([
      child("s/raw", "succeeded"),
      child("s/rendered_md", "succeeded"),
      child("s_view", "failed", "applet"),
    ]);
    expect(got?.status.key).toBe("failed");
    expect(got?.from).toBe("s_view");
  });

  it("names the child in the detail so the tooltip says where the word came from", () => {
    const withDetail: ChildStatus = {
      id: "s/raw",
      kind: "step",
      status: view("failed", null, "boom"),
    };
    expect(groupStatus([withDetail])?.status.detail).toBe("s/raw: boom");
    expect(groupStatus([child("s/raw", "succeeded")])?.status.detail).toBe("s/raw");
  });

  it("keeps the child's instant, which feeds Last synced", () => {
    const got = groupStatus([child("s/raw", "succeeded", "step", "2026-09-10T10:00:00+02:00")]);
    expect(got?.status.at).toBe("2026-09-10T10:00:00+02:00");
  });
});

describe("groupLastSynced", () => {
  it("is the fetch step's instant, even when the render ran later", () => {
    expect(
      groupLastSynced([
        { kind: "step", phase: "fetch", at: "2026-09-10T10:00:00+02:00" },
        { kind: "step", phase: "render", at: "2026-09-10T10:05:00+02:00" },
      ]),
    ).toBe("2026-09-10T10:00:00+02:00");
  });

  it("is null while the fetch step has never run, whatever the render says", () => {
    expect(
      groupLastSynced([
        { kind: "step", phase: "fetch", at: null },
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
      groupSeeds([mk("s/raw", []), mk("s/rendered_md", ["s/raw"]), mk("v", [], "applet")], () => false),
    ).toEqual(["s/raw"]);
  });

  it("leaves out a step the loader dropped", () => {
    expect(groupSeeds([mk("s/raw", [])], (c) => c.id === "s/raw")).toEqual([]);
  });

  it("is empty for a group whose steps all read something", () => {
    expect(groupSeeds([mk("u/grid", ["a/rendered_md"])], () => false)).toEqual([]);
  });
});
