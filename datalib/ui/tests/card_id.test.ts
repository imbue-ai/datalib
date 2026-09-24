import { describe, expect, it } from "vitest";
import { cardType, newCardId, uuidv7 } from "@/cards/cardId";

describe("uuidv7", () => {
  it("carries its millisecond stamp, version 7 and the RFC variant", () => {
    const at = Date.UTC(2026, 8, 24, 12, 0, 0, 123);
    const id = uuidv7(at, new Uint8Array(10).fill(0xff));
    expect(id).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    expect(parseInt(id.replace(/-/g, "").slice(0, 12), 16)).toBe(at);
  });

  it("sorts by when the card opened", () => {
    const zero = new Uint8Array(10);
    const ones = new Uint8Array(10).fill(0xff);
    expect(uuidv7(1_000, ones) < uuidv7(1_001, zero)).toBe(true);
  });

  it("mints a fresh one each time", () => {
    expect(newCardId()).not.toBe(newCardId());
  });
});

describe("cardType", () => {
  it("is the factory or component the source calls", () => {
    expect(cardType('gridView({ q: "source_id:slack" })')).toBe("gridView");
    expect(cardType("  logView()")).toBe("logView");
    expect(cardType("comp . user . tetris()")).toBe("comp.user.tetris");
  });

  it("names what is not a call", () => {
    expect(cardType("")).toBe("blank");
    expect(cardType("(root) => () => {}")).toBe("custom");
  });
});
