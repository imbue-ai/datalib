import { describe, expect, it } from "vitest";
import { adoptLaunchToken, splitLaunchToken, withToken } from "./apiToken";

describe("splitLaunchToken", () => {
  it("takes the token out and keeps the rest of the URL", () => {
    expect(splitLaunchToken("http://127.0.0.1:8731/gridView()?token=abc&q=hi#x")).toEqual({
      href: "http://127.0.0.1:8731/gridView()?q=hi#x",
      token: "abc",
    });
  });

  it("leaves a URL without one alone", () => {
    const href = "http://127.0.0.1:8731/?q=hi";
    expect(splitLaunchToken(href)).toEqual({ href, token: null });
  });
});

describe("withToken", () => {
  it("adds the token to this origin's URLs and to nothing else", () => {
    history.replaceState(null, "", "/somewhere?token=t1&q=hi");
    adoptLaunchToken();
    expect(location.search).toBe("?q=hi");
    expect(withToken("/api/sync/stream")).toBe("/api/sync/stream?token=t1");
    expect(withToken("/api/remote_media?url=x")).toBe("/api/remote_media?url=x&token=t1");
    for (const other of ["https://elsewhere.example/a.png", "data:image/png;base64,AA"]) {
      expect(withToken(other)).toBe(other);
    }
  });
});
