import { describe, expect, it } from "vitest";
import {
  countsByKey,
  groupItems,
  groupKey,
  MORE,
  placeholderOf,
  unread,
  type GroupWindow,
  type ServerGroup,
} from "./serverGroups";
import { firstWindow } from "./pagedWindow";

type Row = { uuid: string; kind: string; author: string | null; source: { label: string } };

const row = (uuid: string, kind: string, author: string | null, label = "Bridge"): Row => ({
  uuid,
  kind,
  author,
  source: { label },
});
const group = (values: (string | null)[], count: number, sample: Row): ServerGroup<Row> => ({
  values,
  count,
  sample,
});

describe("groupItems", () => {
  const logs = group(["Log"], 3, row("log-3", "Log", "picard"));
  const notes = group(["Note"], 1, row("note-1", "Note", null));

  /// A group nobody has opened shows a placeholder, and only that: its
  /// rows are read once it is on screen.
  it("stands a placeholder in for every unread group", () => {
    const windows = new Map([
      [groupKey(logs.values), unread(logs, "c1")],
      [groupKey(notes.values), unread(notes, "c1")],
    ]);
    const items = groupItems([logs, notes], windows);
    expect(items.map((r) => r.uuid)).toEqual(['more:["Log"]', 'more:["Note"]']);
    // The placeholder carries the group's own values, so the grid files
    // it under the right group.
    expect(items[0]).toMatchObject({ kind: "Log", [MORE]: '["Log"]' });
  });

  /// Rows read so far come first, and the placeholder stays at the end
  /// until the group has been read to its end.
  it("puts the rows read ahead of the placeholder, and drops it at the end", () => {
    const partly: GroupWindow<Row> = firstWindow({
      rows: [row("log-3", "Log", "picard")],
      next: 1,
      total: 3,
      at: "c1",
    });
    const done: GroupWindow<Row> = firstWindow({
      rows: [row("note-1", "Note", null)],
      next: null,
      total: 1,
      at: "c1",
    });
    const windows = new Map([
      [groupKey(logs.values), partly],
      [groupKey(notes.values), done],
    ]);
    expect(groupItems([logs, notes], windows).map((r) => r.uuid)).toEqual([
      "log-3",
      'more:["Log"]',
      "note-1",
    ]);
  });

  it("gives each placeholder an id of its own", () => {
    expect(placeholderOf(logs).uuid).not.toBe(logs.sample.uuid);
  });
});

describe("countsByKey", () => {
  /// The grid draws a group per value its getters read, and nests them
  /// under their parents' keys: the counts are there at every level.
  it("counts every group at every level under the grid's own key", () => {
    const counts = countsByKey(
      [
        group(["Log", "picard"], 2, row("a", "Log", "picard")),
        group(["Log", "riker"], 1, row("b", "Log", "riker")),
        group(["Note", null], 4, row("c", "Note", null)),
      ],
      ["kind", "author"],
    );
    expect(Object.fromEntries(counts)).toEqual({
      Log: 3,
      "Log:|:picard": 2,
      "Log:|:riker": 1,
      Note: 4,
      "Note:|:null": 4,
    });
  });

  /// An identity column groups by its label: two sources the server
  /// counts apart, under one name, are one group on screen.
  it("adds up groups that read the same", () => {
    const label = (r: Row) => r.source.label;
    const counts = countsByKey(
      [
        group(["bridge-a"], 2, row("a", "Log", null, "Bridge")),
        group(["bridge-b"], 5, row("b", "Log", null, "Bridge")),
      ],
      [label],
    );
    expect(counts.get("Bridge")).toBe(7);
  });
});
