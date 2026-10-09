// What a stubbed `POST /api/probe` answers: a probe that has already
// finished, in the status shape the wizard polls (`ProbeStatus` in
// datalib/backend/http/src/probe.rs), so no poll follows.
import type { Route } from "@playwright/test";

/// What the wizard asked for: `list` is null for "Check connection".
export type ProbeAsk = { type?: string; params?: Record<string, unknown>; list?: string | null };

export const probeAsk = (route: Route): ProbeAsk => route.request().postDataJSON() as ProbeAsk;

export const probeDone = (report: object) => ({
  json: { id: "p1", status: "ok", progress: null, report, failure: null },
});

/// A probe that failed, as the backend classified it (`IssueKind` in
/// datalib/backend/probe/src/issue.rs).
export const probeFailed = (issue: string, detail: string) => ({
  json: { id: "p1", status: "failed", progress: null, report: null, failure: { issue, detail } },
});

/// A report answered the way the provider would for `ask`: the account
/// alone for a check, and only the asked-for kinds for a list.
export function reportFor<R extends { items: { kind: string }[] }>(
  report: R,
  ask: ProbeAsk,
  kinds: Record<string, string[]>,
): R {
  const wanted = ask.list ? (kinds[ask.list] ?? []) : [];
  return { ...report, items: report.items.filter((i) => wanted.includes(i.kind)) };
}
