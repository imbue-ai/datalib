// What the wizard says about a failure: one sentence for its kind, and
// what to do about it. The kinds come from the backend's classifier
// (`datalib/backend/probe/src/issue.rs`); the words are here, once, so
// every source and every button says the same thing the same way.
import type { Failure, IssueKind } from "@/api";

/// Where a person can get a credential into latchkey from this dialog.
export type SignInWhere = "here" | "gateway" | "terminal";

export type IssueText = { headline: string; advice: string | null };

/// Longest headline kept whole; an unclassified failure is its first
/// line, cut here.
const HEADLINE_MAX = 140;

function signInAgain(where: SignInWhere): string {
  switch (where) {
    case "here":
      return "Sign in again above, then check again.";
    case "gateway":
      return "Sign in where the latchkey gateway is managed, then check again.";
    case "terminal":
      return "Store a new one with latchkey in a terminal, then check again.";
  }
}

/// The first line of a raw failure, without the `error:` prefixes the
/// step's chain carries, short enough to read at a glance.
export function firstLine(detail: string): string {
  const line = (detail.split("\n").find((l) => l.trim()) ?? "")
    .replace(/^(\s*error:\s*)+/, "")
    .trim();
  return line.length > HEADLINE_MAX ? `${line.slice(0, HEADLINE_MAX - 1)}…` : line;
}

export function issueText(failure: Failure, service: string, where: SignInWhere): IssueText {
  const kind: IssueKind = failure.issue;
  switch (kind) {
    case "no_credential":
      return {
        headline: `No ${service} sign-in is stored yet.`,
        advice:
          where === "here" ? "Sign in or paste a key above, then check again." : signInAgain(where),
      };
    case "expired":
      return { headline: `The stored ${service} sign-in has expired.`, advice: signInAgain(where) };
    case "rejected":
      return {
        headline: `${service} turned the stored sign-in away.`,
        advice: `It may have expired or been signed out. ${signInAgain(where)}`,
      };
    case "forbidden":
      return {
        headline: `${service} accepted the sign-in but refused this request.`,
        advice: "The account, or the key's permissions, may not cover it.",
      };
    case "blocked":
      return {
        headline: `${service}'s bot protection blocked the request.`,
        advice: "This is not a sign-in problem. Try again later, or from another network.",
      };
    case "rate_limited":
      return {
        headline: `${service} asked datalib to slow down.`,
        advice: "Wait a minute, then try again.",
      };
    case "service_error":
      return {
        headline: `${service} is having trouble right now.`,
        advice: "Try again in a few minutes.",
      };
    case "unreachable":
      return {
        headline: `Couldn't reach ${service}.`,
        advice: "Check the network connection, then try again.",
      };
    case "gateway_unreachable":
      return {
        headline: "Couldn't reach the latchkey gateway that holds the sign-in.",
        advice: "It may have stopped. Check where it is managed, then try again.",
      };
    case "unexpected_response":
      return {
        headline: `${service} answered with something datalib didn't expect.`,
        advice: "Try again. If it keeps happening, the details are what to report.",
      };
    case "no_runtime":
      return {
        headline: "datalib's bundled sign-in tool, latchkey, is missing.",
        advice: "Reinstalling datalib puts it back. The details say where it was looked for.",
      };
    case "no_browser":
      return {
        headline: "No browser was found for the sign-in, and one couldn't be fetched.",
        advice: "Install Chrome or Chromium, then try again.",
      };
    case "keychain":
      return {
        headline: "latchkey couldn't open its key in the system keychain.",
        advice: "If macOS asked for keychain access, allow it, then try again.",
      };
    default:
      return { headline: firstLine(failure.detail) || "Something went wrong.", advice: null };
  }
}
