import { describe, expect, it } from "vitest";
import type { IssueKind } from "@/api";
import { firstLine, issueText } from "@/config/issues";

const KINDS: IssueKind[] = [
  "no_credential",
  "expired",
  "rejected",
  "forbidden",
  "blocked",
  "rate_limited",
  "service_error",
  "unreachable",
  "gateway_unreachable",
  "unexpected_response",
  "no_runtime",
  "no_browser",
  "keychain",
  "unknown",
];

describe("issueText", () => {
  it("says every kind in one short line, whatever the detail", () => {
    const detail = "error: " + "x".repeat(4000) + "\nstep 1: run a command";
    for (const issue of KINDS) {
      for (const where of ["here", "gateway", "terminal"] as const) {
        const { headline, advice } = issueText({ issue, detail }, "Google Calendar", where);
        expect(headline.length, `${issue}: ${headline}`).toBeLessThanOrEqual(140);
        expect(headline).not.toContain("\n");
        expect(advice ?? "").not.toContain("\n");
      }
    }
  });

  it("points a refused sign-in at wherever signing in happens", () => {
    const failure = { issue: "rejected" as const, detail: "" };
    expect(issueText(failure, "Slack", "here").advice).toContain("Sign in again above");
    expect(issueText(failure, "Slack", "gateway").advice).toContain("gateway is managed");
  });

  it("does not call a bot wall a sign-in problem", () => {
    const { advice } = issueText({ issue: "blocked", detail: "" }, "Claude", "here");
    expect(advice).toContain("not a sign-in problem");
  });
});

describe("firstLine", () => {
  it("drops the chain's prefixes and keeps the first line", () => {
    expect(firstLine("error: error: list orgs: boom\nerror: cause")).toBe("list orgs: boom");
  });
});
