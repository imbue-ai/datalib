// The paste form reads its shape off latchkey's `setCredentialsExample`.
// Read wrong, it stores a credential latchkey sends in the wrong place —
// which looks like success until the first sync.
import { describe, expect, it } from "vitest";
import { credentialShape, pastedCredential } from "../src/config/credentialShape";

describe("credentialShape", () => {
  it("reads an app password as a username and password", () => {
    const shape = credentialShape(
      'latchkey auth set fastmail-dav -u "you@fastmail.com:<app password>"',
    );
    expect(shape).toEqual({
      kind: "basic",
      userHint: "you@fastmail.com",
      secretLabel: "app password",
    });
    expect(pastedCredential(shape, "picard@enterprise.test", " engage ")).toEqual({
      kind: "basic",
      username: "picard@enterprise.test",
      password: "engage",
    });
    expect(pastedCredential(shape, "", "engage")).toBeNull();
  });

  it("puts the secret where latchkey's placeholder is", () => {
    const shape = credentialShape('latchkey auth set gitlab -H "PRIVATE-TOKEN: <token>"');
    expect(pastedCredential(shape, "", "glpat-1")).toEqual({
      kind: "headers",
      headers: ["PRIVATE-TOKEN: glpat-1"],
    });
  });

  it("takes a sample value's last word as the secret", () => {
    const shape = credentialShape(
      'latchkey auth set slack -H "Authorization: Bearer xoxb-your-token"',
    );
    expect(pastedCredential(shape, "", "xoxp-2")).toEqual({
      kind: "headers",
      headers: ["Authorization: Bearer xoxp-2"],
    });
  });

  it("prefers the catalog's shape where latchkey's example is generic", () => {
    const shape = credentialShape(
      'latchkey auth set claude-ai -H "Authorization: Bearer <token>"',
      ["Cookie: sessionKey={secret}"],
    );
    expect(pastedCredential(shape, "", "sk-ant-3")).toEqual({
      kind: "headers",
      headers: ["Cookie: sessionKey=sk-ant-3"],
    });
  });

  it("falls back to the whole header line", () => {
    const shape = credentialShape(null);
    expect(pastedCredential(shape, "", "X-Api-Key: 4")).toEqual({
      kind: "headers",
      headers: ["X-Api-Key: 4"],
    });
    expect(pastedCredential(shape, "", "   ")).toBeNull();
  });
});
