// The two calls a chart of time series makes to the time-series applet:
// the list, and the points of what is picked over a span.
import { appletAbsent } from "@/api";
import {
  TIME_SERIES_APPLET,
  fromMs,
  type Answered,
  type Listed,
  type TimeKind,
} from "./timeSeries";

/// Every time series the index holds; null on a root that keeps no index.
export async function fetchTimeSeries(): Promise<Listed[] | null> {
  const r = await fetch(`${TIME_SERIES_APPLET}/time_series`);
  const text = await r.text();
  if (appletAbsent("unified_time_series_index", r.status, text)) return null;
  if (!r.ok) throw new Error(`time series → ${r.status}: ${text}`);
  return (JSON.parse(text) as { time_series: Listed[] }).time_series;
}

export type Unreadable = { time_series_id: string; error: string };

/// The points of `picked` over `span`, about `budget` a time series. The
/// applet reads `from` and `to` in each time series' own `t`, so days and
/// instants are asked for separately.
export async function fetchPoints(
  picked: Listed[],
  span: { fromMs: number; toMs: number },
  budget: number,
): Promise<{ answered: Map<string, Answered>; unreadable: Unreadable[] }> {
  const kinds = new Map<TimeKind, string[]>();
  for (const p of picked)
    kinds.set(p.time_kind, [...(kinds.get(p.time_kind) ?? []), p.time_series_id]);
  const answers = await Promise.all(
    [...kinds.entries()].map(async ([kind, ids]) => {
      const r = await fetch(`${TIME_SERIES_APPLET}/points`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          ids,
          from: fromMs(kind, span.fromMs),
          to: fromMs(kind, span.toMs),
          budget,
        }),
      });
      const text = await r.text();
      if (!r.ok) throw new Error(`points → ${r.status}: ${text}`);
      return JSON.parse(text) as { time_series: Answered[]; unreadable?: Unreadable[] };
    }),
  );
  return {
    answered: new Map(answers.flatMap((a) => a.time_series).map((a) => [a.time_series_id, a])),
    unreadable: answers.flatMap((a) => a.unreadable ?? []),
  };
}
