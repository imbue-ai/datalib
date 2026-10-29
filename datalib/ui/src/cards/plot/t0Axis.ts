// A time axis drawn from T0 (docs/dev/chips.md § "A date"): ticks on
// round steps counted from T0, labelled by their distance from it in the
// units the step needs, `+1d 6h`, `-30s`, `+2y`. Every chart's time axis
// draws from here while a T0 is set.
import { partsSinceT0, yearsOn } from "@/config/timeView";

const S = 1000;
const MIN = 60 * S;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;
const YEAR = 365.2425 * DAY;

/** Steps shorter than a year, each a fixed length. */
const STEPS = [
  1,
  2,
  5,
  10,
  20,
  50,
  100,
  200,
  500,
  S,
  2 * S,
  5 * S,
  10 * S,
  15 * S,
  30 * S,
  MIN,
  2 * MIN,
  5 * MIN,
  10 * MIN,
  15 * MIN,
  30 * MIN,
  HOUR,
  2 * HOUR,
  3 * HOUR,
  6 * HOUR,
  12 * HOUR,
  DAY,
  2 * DAY,
  7 * DAY,
  14 * DAY,
  30 * DAY,
  60 * DAY,
  90 * DAY,
  180 * DAY,
];
/** Steps of a year or more, each in calendar years, as `sinceT0` counts. */
const YEAR_STEPS = [1, 2, 5, 10, 20, 50, 100, 200, 500, 1000];

/** Ticks between `min` and `max` at whole steps from T0: the shortest
 *  step that leaves at most `count` intervals across the span. */
export function t0Ticks(min: number, max: number, t0: number, count: number): number[] {
  const span = max - min;
  const step = STEPS.find((s) => span / s <= count);
  if (step !== undefined) {
    const out: number[] = [];
    for (let n = Math.ceil((min - t0) / step); t0 + n * step <= max; n++) out.push(t0 + n * step);
    return out;
  }
  const years = YEAR_STEPS.find((y) => span / (y * YEAR) <= count) ?? YEAR_STEPS.at(-1)!;
  const out: number[] = [];
  const first = Math.floor((min - t0) / (years * YEAR)) - 1;
  const last = Math.ceil((max - t0) / (years * YEAR)) + 1;
  for (let n = first; n <= last; n++) {
    const at = yearsOn(t0, n * years);
    if (at >= min && at <= max) out.push(at);
  }
  return out;
}

const UNITS = ["y", "d", "h", "m", "s", "ms"] as const;

/** The smallest unit a label needs for ticks `stepMs` apart. */
function precision(stepMs: number): number {
  if (stepMs >= 365 * DAY) return 0;
  if (stepMs >= DAY) return 1;
  if (stepMs >= HOUR) return 2;
  if (stepMs >= MIN) return 3;
  if (stepMs >= S) return 4;
  return 5;
}

/** Each tick's distance from T0, down to the unit the spacing of the
 *  ticks needs and leaving out the units that are zero: `T0`, `+6h`,
 *  `+1d 6h`, `-1y 45d`. `spanMs` stands in for the spacing when there is
 *  only one tick. */
export function t0TickTexts(ticks: number[], t0: number, spanMs: number): string[] {
  const unit = precision(ticks.length > 1 ? ticks[1]! - ticks[0]! : spanMs);
  return ticks.map((ms) => {
    const p = partsSinceT0(ms, t0);
    if (p.sign === 0) return "T0";
    const values = [p.years, p.days, p.hours, p.minutes, p.seconds, p.millis];
    const shown = values
      .slice(0, unit + 1)
      .flatMap((v, i) => (v ? [`${v}${UNITS[i]}`] : []))
      .join(" ");
    return `${p.sign < 0 ? "-" : "+"}${shown || `0${UNITS[unit]}`}`;
  });
}
