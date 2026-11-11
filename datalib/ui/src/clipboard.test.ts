import { afterEach, describe, expect, it, vi } from "vitest";
import { copyRichToClipboard } from "./clipboard";

/** What a `copy` event's listeners put on the clipboard. */
function fireCopy(): Record<string, string> {
  const data: Record<string, string> = {};
  const ev = new Event("copy", { cancelable: true }) as Event & {
    clipboardData: Pick<DataTransfer, "setData">;
  };
  ev.clipboardData = { setData: (type: string, value: string) => void (data[type] = value) };
  document.dispatchEvent(ev);
  return data;
}

afterEach(() => vi.unstubAllGlobals());

describe("copyRichToClipboard", () => {
  /** A webview that refuses the clipboard API still gets the link: the
   *  fallback copied the plain text alone, so a paste was no link. */
  it("carries the link on the copy route when the API is refused", async () => {
    vi.stubGlobal("navigator", {
      clipboard: { write: vi.fn(async () => Promise.reject(new Error("denied"))) },
    });
    vi.stubGlobal("ClipboardItem", class {});
    let copied: Record<string, string> = {};
    document.execCommand = vi.fn(() => {
      copied = fireCopy();
      return true;
    });
    const html = '<a href="datalib:doc/x">#bridge</a>';
    expect(await copyRichToClipboard("#bridge (datalib:doc/x)", html)).toBe(true);
    expect(copied).toEqual({ "text/plain": "#bridge (datalib:doc/x)", "text/html": html });
    // The listener is gone after the copy.
    expect(fireCopy()).toEqual({});
  });
});
