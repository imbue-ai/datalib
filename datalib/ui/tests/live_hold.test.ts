import { describe, expect, it, vi } from "vitest";
import { chainOfFrameBeingHandled, holdWhileOffScreen, type RootEvent } from "../src/live";

const rows: RootEvent = { kind: "table_changed", table: "manage.rows" };
const dag: RootEvent = { kind: "table_changed", table: "dag" };

describe("holdWhileOffScreen", () => {
  it("passes everything through while on screen", () => {
    const root = vi.fn();
    const { handlers } = holdWhileOffScreen({ root });
    handlers.root!(rows);
    expect(root).toHaveBeenCalledWith(rows);
  });

  /// A Manage card in a hidden tab refetched its rows on every frame
  /// while a sync ran: twice the requests, and twice the log lines.
  it("holds frames off screen and delivers each once on the way back", () => {
    const root = vi.fn();
    const { handlers, setOnScreen } = holdWhileOffScreen({ root });
    setOnScreen(false);
    handlers.root!(rows);
    handlers.root!({ ...rows, chain: 3 });
    handlers.root!(dag);
    handlers.root!(rows);
    expect(root).not.toHaveBeenCalled();

    setOnScreen(true);
    expect(root.mock.calls.map((c) => c[0])).toEqual([rows, dag]);
  });

  it("marks what a released frame fetches as a live refetch", () => {
    const seen: (number | undefined)[] = [];
    const { handlers, setOnScreen } = holdWhileOffScreen({
      root: () => seen.push(chainOfFrameBeingHandled()),
    });
    setOnScreen(false);
    handlers.root!({ ...rows, chain: 7 });
    setOnScreen(true);
    expect(seen).toEqual([0]);
    expect(chainOfFrameBeingHandled()).toBeUndefined();
  });

  it("lets one resync stand for everything held", () => {
    const root = vi.fn();
    const resync = vi.fn();
    const { handlers, setOnScreen } = holdWhileOffScreen({ root, resync });
    setOnScreen(false);
    handlers.root!(rows);
    handlers.resync!();
    handlers.resync!();
    setOnScreen(true);
    expect(resync).toHaveBeenCalledTimes(1);
    expect(root).not.toHaveBeenCalled();

    setOnScreen(true);
    expect(resync).toHaveBeenCalledTimes(1);
  });
});
