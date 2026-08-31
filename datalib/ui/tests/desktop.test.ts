import { afterEach, describe, expect, it, vi } from "vitest";
import {
  filePathFromUrl,
  isDesktopApp,
  pickPath,
  revealActionLabel,
  revealInFileManager,
} from "../src/desktop";

/** Install a fake Tauri IPC bridge; returns the invoke spy. */
function fakeTauri(impl?: (cmd: string, args?: unknown) => Promise<unknown>) {
  const invoke = vi.fn(impl ?? (() => Promise.resolve(null)));
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    invoke,
  };
  return invoke;
}

afterEach(() => {
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
  vi.restoreAllMocks();
});

describe("isDesktopApp", () => {
  it("is false in a plain browser", () => {
    expect(isDesktopApp()).toBe(false);
  });

  it("is true when Tauri's IPC bridge is present", () => {
    fakeTauri();
    expect(isDesktopApp()).toBe(true);
  });

  it("is false when the bridge exists but has no invoke", () => {
    // What a partially-initialised or future-renamed bridge looks like.
    // Better to fall back to browser behavior than to throw on click.
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
    expect(isDesktopApp()).toBe(false);
  });
});

describe("revealInFileManager", () => {
  it("returns false in a browser rather than throwing", async () => {
    await expect(revealInFileManager("/tmp/x.pdf")).resolves.toBe(false);
  });

  it("reaches the opener plugin's reveal command with a paths ARRAY", async () => {
    // This asserts through `@tauri-apps/plugin-opener`, not around it:
    // the package builds the command name and normalizes the argument,
    // and its `invoke` bottoms out on the same `__TAURI_INTERNALS__`
    // this mock replaces. So if a Tauri upgrade renames the command or
    // the argument, THIS TEST GOES RED — which is the whole reason to
    // depend on the package rather than hand-rolling the invoke.
    //
    // (`paths` is plural and an array; the singular form is accepted by
    // the IPC layer and then ignored, which looks exactly like a no-op.)
    const invoke = fakeTauri();
    await expect(revealInFileManager("/tmp/x.pdf")).resolves.toBe(true);
    const [cmd, args] = invoke.mock.calls[0];
    expect(cmd).toBe("plugin:opener|reveal_item_in_dir");
    expect(args).toEqual({ paths: ["/tmp/x.pdf"] });
  });

  it("returns false when the IPC call rejects", async () => {
    // e.g. the capability is missing, or the file has since moved.
    vi.spyOn(console, "warn").mockImplementation(() => {});
    fakeTauri(() => Promise.reject(new Error("not allowed")));
    await expect(revealInFileManager("/tmp/x.pdf")).resolves.toBe(false);
  });
});

describe("filePathFromUrl", () => {
  it("decodes a plain file URL", () => {
    expect(filePathFromUrl("file:///corpus/a/b.pdf")).toBe("/corpus/a/b.pdf");
  });

  it("round-trips a percent-encoded '#', which Rust always encodes", () => {
    // Real filename from the corpus this was built against. Passing the
    // raw URL to the reveal IPC would hunt for a file named `...%23...`.
    const url = "file:///c/Imbue%20Mail%20-%20New%20Order%20%23%20101445654.pdf";
    expect(filePathFromUrl(url)).toBe(
      "/c/Imbue Mail - New Order # 101445654.pdf",
    );
  });

  it("round-trips non-ASCII", () => {
    expect(filePathFromUrl("file:///c/%E6%97%A5%E6%9C%AC.pdf")).toBe(
      "/c/日本.pdf",
    );
  });

  it("returns null for http(s) URLs so web rows keep Open source", () => {
    expect(filePathFromUrl("https://claude.ai/chat/abc")).toBeNull();
    expect(filePathFromUrl("http://localhost:8080/x")).toBeNull();
  });

  it("returns null for a bare path", () => {
    // The shape the pdf provider used to emit; must not be mistaken for
    // a local file, or the menu would offer to reveal nothing.
    expect(filePathFromUrl("engineering/warp_core_manual.pdf")).toBeNull();
    expect(filePathFromUrl("")).toBeNull();
  });
});

describe("revealActionLabel", () => {
  const setUA = (ua: string) =>
    vi.spyOn(navigator, "userAgent", "get").mockReturnValue(ua);

  it("uses the platform's own wording", () => {
    setUA("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)");
    expect(revealActionLabel()).toBe("Reveal in Finder");
    vi.restoreAllMocks();
    setUA("Mozilla/5.0 (Windows NT 10.0; Win64; x64)");
    expect(revealActionLabel()).toBe("Show in Explorer");
    vi.restoreAllMocks();
    setUA("Mozilla/5.0 (X11; Linux x86_64)");
    expect(revealActionLabel()).toBe("Show in File Manager");
  });
});

describe("pickPath", () => {
  const folder = { picks: "dir" as const, title: "Choose your WhatsApp backup folder" };

  it("is unavailable in a browser rather than throwing", async () => {
    // The wizard hides the button on `isDesktopApp()`, so this is the
    // belt to that braces: a browser must never see a dialog attempt.
    await expect(pickPath(folder)).resolves.toEqual({
      outcome: "unavailable",
      reason: "not running in the desktop app",
    });
  });

  it("reaches the dialog plugin's open command with the folder options", async () => {
    // Asserts THROUGH `@tauri-apps/plugin-dialog` for the same reason
    // the reveal test does: the package owns the command name and the
    // `{ options }` envelope, so a Tauri upgrade that renames either
    // turns this red instead of leaving a button that does nothing.
    const invoke = fakeTauri(() => Promise.resolve("/Users/x/backups/WhatsApp"));
    await expect(pickPath(folder)).resolves.toEqual({
      outcome: "picked",
      path: "/Users/x/backups/WhatsApp",
    });
    const [cmd, args] = invoke.mock.calls[0];
    expect(cmd).toBe("plugin:dialog|open");
    expect(args).toEqual({
      options: {
        title: "Choose your WhatsApp backup folder",
        directory: true,
        multiple: false,
        defaultPath: undefined,
        filters: undefined,
      },
    });
  });

  it("passes an extension filter only for file pickers", async () => {
    const invoke = fakeTauri(() => Promise.resolve("/Users/x/Catalog.lrcat"));
    await pickPath({ picks: "file", title: "Choose your Lightroom catalog", extensions: ["lrcat"] });
    expect((invoke.mock.calls[0][1] as any).options).toMatchObject({
      directory: false,
      filters: [{ name: "Supported files", extensions: ["lrcat"] }],
    });
  });

  it("reports cancel distinctly from denial, so the field survives it", async () => {
    // The platform dialog resolves null on cancel. Conflating that with
    // a denied command would either clear the user's typed path or put
    // an error under a field they simply changed their mind about.
    fakeTauri(() => Promise.resolve(null));
    await expect(pickPath(folder)).resolves.toEqual({ outcome: "canceled" });
  });

  it("reports an unauthorized command as unavailable, not as cancel", async () => {
    // What a missing/mismatched capability looks like from the webview:
    // a rejected invoke and no other signal. See
    // tauri/capabilities/pick-local-paths.json.
    vi.spyOn(console, "warn").mockImplementation(() => {});
    fakeTauri(() => Promise.reject(new Error("not allowed")));
    const result = await pickPath(folder);
    expect(result.outcome).toBe("unavailable");
  });

  describe("start directory", () => {
    const startAtOf = async (startAt: string) => {
      const invoke = fakeTauri(() => Promise.resolve(null));
      await pickPath({ ...folder, startAt });
      return (invoke.mock.calls[0][1] as any).options.defaultPath;
    };

    it("opens at an absolute path the user already had", async () => {
      await expect(startAtOf("/Users/x/backups/WhatsApp")).resolves.toBe(
        "/Users/x/backups/WhatsApp",
      );
      await expect(startAtOf(String.raw`C:\Users\x\WhatsApp`)).resolves.toBe(
        String.raw`C:\Users\x\WhatsApp`,
      );
    });

    it("drops a `~` path, which no one expands on this route", async () => {
      // Tauri hands defaultPath to the platform dialog verbatim — no
      // shell — so `~/backups` is a RELATIVE path resolved against the
      // process cwd, and the dialog would open somewhere arbitrary.
      await expect(startAtOf("~/backups/WhatsApp")).resolves.toBeUndefined();
    });

    it("drops empty and half-typed values", async () => {
      await expect(startAtOf("")).resolves.toBeUndefined();
      await expect(startAtOf("   ")).resolves.toBeUndefined();
      await expect(startAtOf("backups/WhatsApp")).resolves.toBeUndefined();
    });
  });
});
