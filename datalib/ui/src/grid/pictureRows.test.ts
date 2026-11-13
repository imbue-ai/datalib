import { describe, expect, it } from "vitest";
import { pictureRowHeight } from "./pictureRows";

const pictures = new Set(["image"]);

describe("pictureRowHeight", () => {
  /// Dragging the Picture column wider used to leave the row at its
  /// fixed height, so the picture stopped growing.
  it("makes a row as tall as its picture column is wide", () => {
    expect(pictureRowHeight([{ id: "image", width: 180 }], pictures, 52)).toBe(180);
  });

  it("never goes below the grid's own row height", () => {
    expect(pictureRowHeight([{ id: "image", width: 40 }], pictures, 52)).toBe(52);
  });

  it("ignores a hidden picture column and every other column", () => {
    const cols = [
      { id: "image", width: 300, hidden: true },
      { id: "snippet", width: 600 },
    ];
    expect(pictureRowHeight(cols, pictures, 52)).toBe(52);
  });
});
