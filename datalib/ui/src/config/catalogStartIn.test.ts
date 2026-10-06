// A path field's `startIn` is shown inside its help text with a copy
// button, and the picker opens there; both need it to be a home path
// the help actually names.

import { describe, expect, it } from "vitest";

import { CATALOG } from "./catalog";

const startFields = CATALOG.flatMap((entry) =>
  (entry.fields ?? []).flatMap((f) =>
    f.kind === "path" && f.startIn ? [{ key: `${entry.type}:${f.target}`, f }] : [],
  ),
);

describe("path field startIn", () => {
  it("is set on the Messages folder", () => {
    expect(startFields.map((s) => s.key)).toContain("apple_messages:messages.path");
  });

  it.each(startFields)("$key names it in its help, as a ~/ path", ({ f }) => {
    if (f.kind !== "path") throw new Error("unreachable");
    expect(f.startIn).toMatch(/^~\//);
    expect(f.help ?? "").toContain(f.startIn);
  });
});
