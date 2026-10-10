import { describe, expect, it } from "vitest";
import type { DiskFree } from "@/api";
import { diskCrossing, diskTitle, lowDiskMessage } from "./diskFree";

function disk(available: number, low: boolean): DiskFree {
  return {
    available_bytes: available,
    total_bytes: 500e9,
    pause_below_bytes: 10e9,
    resume_at_bytes: 15e9,
    low,
    history: [],
    window_secs: 300,
  };
}

describe("diskCrossing", () => {
  it("raises the toast on a page that opens with the steps already held", () => {
    expect(diskCrossing(null, disk(3e9, true))).toBe("low");
  });

  it("raises it once, and clears it once the steps are let go", () => {
    expect(diskCrossing(false, disk(3e9, true))).toBe("low");
    expect(diskCrossing(true, disk(12e9, true))).toBeNull();
    expect(diskCrossing(true, disk(16e9, false))).toBe("cleared");
    expect(diskCrossing(false, disk(16e9, false))).toBeNull();
  });

  it("says nothing on a first answer with room", () => {
    expect(diskCrossing(null, disk(42e9, false))).toBeNull();
  });
});

describe("the words", () => {
  it("say how much is free and how much it takes to carry on", () => {
    const msg = lowDiskMessage(disk(3.2e9, true));
    expect(msg).toContain("Only 3.2 GB free");
    expect(msg).toContain("once it has 15 GB free");
  });

  it("name both lines in the hover", () => {
    const roomy = diskTitle(disk(42e9, false), "");
    expect(roomy).toContain("42 GB free of 500 GB");
    expect(roomy).toContain("pause under 10 GB free and carry on from 15 GB");
    expect(diskTitle(disk(12e9, true), "")).toContain("paused until 15 GB is free");
  });
});
