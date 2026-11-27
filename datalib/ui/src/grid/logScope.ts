// What a log panel shows, as the search terms that say it: a run
// (`run:`: a request or a job, or a run an older build recorded), one of
// its invocations (`invocation:`), one process (`process_id:`: a launch
// of the server, a page of the app, the runner), or a step's attempt
// (`step:` and `attempt:`). The panel's pickers read them off its query
// and write them back, so the query is the whole of what is on screen,
// and the one thing that is kept, edited or shared.

import { filterToken, replaceToken, tokenValue } from "./query";

export type LogScope = {
  run: string | null;
  invocation: string | null;
  processId: string | null;
  step: string | null;
  attempt: number | null;
};

export const EVERYTHING: LogScope = {
  run: null,
  invocation: null,
  processId: null,
  step: null,
  attempt: null,
};

export function scopeOf(query: string): LogScope {
  const attempt = tokenValue(query, "attempt");
  return {
    run: tokenValue(query, "run"),
    invocation: tokenValue(query, "invocation"),
    processId: tokenValue(query, "process_id"),
    step: tokenValue(query, "step"),
    attempt: attempt === null ? null : Number(attempt),
  };
}

/// `query` narrowed to `scope` instead of whatever scope it had; the rest
/// of what was typed is left as it was.
export function withScope(query: string, scope: LogScope): string {
  const term = (key: string, value: string | number | null) =>
    value === null ? null : filterToken(key, String(value), false);
  return [
    ["run", scope.run],
    ["invocation", scope.invocation],
    ["process_id", scope.processId],
    ["step", scope.step],
    ["attempt", scope.attempt],
  ].reduce((q, [key, value]) => replaceToken(q, key as string, term(key as string, value)), query);
}
