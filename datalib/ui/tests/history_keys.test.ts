import { describe, expect, it } from "vitest";
import { historyStep } from "@/historyKeys";

const press = (
  key: string,
  mods: Partial<{ metaKey: boolean; altKey: boolean; ctrlKey: boolean }>,
) => ({
  key,
  metaKey: false,
  altKey: false,
  ctrlKey: false,
  ...mods,
});

describe("historyStep", () => {
  it("reads Cmd+[ and Cmd+] as back and forward", () => {
    expect(historyStep(press("[", { metaKey: true }))).toBe(-1);
    expect(historyStep(press("]", { metaKey: true }))).toBe(1);
  });
  it("reads Alt+arrows as back and forward", () => {
    expect(historyStep(press("ArrowLeft", { altKey: true }))).toBe(-1);
    expect(historyStep(press("ArrowRight", { altKey: true }))).toBe(1);
  });
  it("leaves every other chord alone", () => {
    expect(historyStep(press("[", {}))).toBe(0);
    expect(historyStep(press("ArrowLeft", { metaKey: true }))).toBe(0);
    expect(historyStep(press("[", { metaKey: true, altKey: true }))).toBe(0);
    expect(historyStep(press("ArrowLeft", { ctrlKey: true, altKey: true }))).toBe(0);
  });
});
