// What the Manage screen's Activity cell says about a running step, from
// what the run store holds for it: how much is queued ahead of it, what
// it has counted so far and how fast, whether it has stopped advancing,
// and how many warnings and errors it has logged — the USE questions.

import type { DagStepProgress } from "@/api";

export type ActivityChip = {
  kind: "queued" | "idle" | "metric" | "stalled" | "errors";
  text: string;
  title: string;
};

/// How long a running step may go without a metric moving before the
/// cell says so. A minute: a download that has not written a row in a
/// minute is either waiting on the network or stuck, and either is
/// worth a glance.
export const STALL_AFTER_SECS = 60;

/// A per-second rate, short: "20/s", "1.2k/s".
export function formatRate(perSec: number): string {
  if (perSec >= 1000) return `${(perSec / 1000).toFixed(1)}k/s`;
  if (perSec >= 10) return `${Math.round(perSec)}/s`;
  return `${perSec.toFixed(1)}/s`;
}

/// A span of seconds, short: "45s", "2m", "1.5h".
export function formatAge(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.round(secs / 60)}m`;
  return `${(secs / 3600).toFixed(1)}h`;
}

const isQueued = (name: string) => name === "queued" || name.startsWith("queued{");

/// The chips, in the order the cell draws them: `queued` first, because it
/// is the one that says whether the step is keeping up; then every other
/// series the step reported, with its rate; then a stall, if any; then
/// the warn/error count when there is one.
export function activityChips(p: DagStepProgress): ActivityChip[] {
  const chips: ActivityChip[] = [];
  // Every `queued` series: the step's own gauge, plus one per producer
  // (`queued{from=slack/ingest}`) that the runner keeps from what the
  // producers sealed. One chip, summed; the breakdown is on hover.
  const queuedSeries = Object.entries(p.metrics).filter(([name]) => isQueued(name));
  if (queuedSeries.length > 0) {
    const queued = queuedSeries.reduce((n, [, v]) => n + v, 0);
    const breakdown = queuedSeries
      .map(([name, v]) => {
        const from = name.match(/^queued\{from=(.*)\}$/)?.[1];
        return from ? `${v.toLocaleString()} from ${from}` : `${v.toLocaleString()} of its own`;
      })
      .join(", ");
    chips.push({
      kind: queued > 0 ? "queued" : "idle",
      text: `${queued.toLocaleString()} queued`,
      title:
        queuedSeries.length > 1
          ? `Work still ahead of the step: ${breakdown}`
          : "Work the step says is still ahead of it",
    });
  }
  for (const [name, value] of Object.entries(p.metrics)) {
    if (isQueued(name)) continue;
    const rate = p.rates[name];
    const moving = rate != null && rate > 0;
    chips.push({
      kind: "metric",
      text: `${name} ${value.toLocaleString()}${moving ? ` · ${formatRate(rate)}` : ""}`,
      title:
        `${name} = ${value.toLocaleString()} so far this run` +
        (moving ? `, moving at ${formatRate(rate)}` : ""),
    });
  }
  // Not advancing: a running step whose numbers have not moved for a
  // while. Whether it is still logging is the difference between "busy
  // but stuck" and "silent", and goes on the hover.
  if (p.progress_age_secs != null && p.progress_age_secs >= STALL_AFTER_SECS) {
    const logAge = p.log_age_secs;
    const talking = logAge != null && logAge < STALL_AFTER_SECS;
    const since = formatAge(p.progress_age_secs);
    chips.push({
      kind: "stalled",
      text: `no progress ${since}`,
      title: talking
        ? `No metric has moved for ${since}, but the step is still logging (last line ${formatAge(logAge)} ago) — busy, not advancing`
        : logAge == null
          ? `No metric has moved for ${since}, and the step has logged nothing`
          : `No metric has moved for ${since}, and the step last logged ${formatAge(logAge)} ago — silent`,
    });
  }
  if (p.errors > 0) {
    chips.push({
      kind: "errors",
      text: `${p.errors.toLocaleString()} ⚠`,
      title: `${p.errors} warning${p.errors === 1 ? "" : "s"} or error${p.errors === 1 ? "" : "s"} logged this run — double-click Status to read them`,
    });
  }
  return chips;
}

/// The cell's sortable, filterable value: the chips as text.
export function activityText(p: DagStepProgress | null): string {
  return p
    ? activityChips(p)
        .map((c) => c.text)
        .join("  ")
    : "";
}
