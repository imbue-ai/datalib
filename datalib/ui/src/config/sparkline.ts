// Turning a compacted timeseries into polyline points.
//
// The series this draws — bytes on disk, from `GET
// /api/pipeline/storage` — is a **step function recorded only when it
// moves**: the backend drops a repeat and never records two samples of
// one series closer than five seconds apart (see
// `datalib/backend/http/src/usage.rs`). So a naive "join the dots"
// plot would be wrong twice over. It would slope between two samples
// that were flat the whole way, and a series that last changed an hour
// ago — one sample, far to the left — would draw as a single point
// instead of the flat line it actually is.

/// One measurement, as a `timeseries` cell carries it.
export type Sample = {
  /// ISO-8601 with an explicit offset, per the repo's convention.
  at: string;
  value: number;
};

export type SparkOpts = {
  /// The instant at the right edge, epoch ms.
  nowMs: number;
  /// How much time the plot spans, ms. The left edge is
  /// `nowMs - windowMs`.
  windowMs: number;
  /// The values the bottom and the top of the plot stand for. Every
  /// series is drawn against its own range (`ownRange`), not against
  /// zero or a neighbour: a source that jumps should look like it
  /// jumped, however small it is beside the others.
  min: number;
  max: number;
  width: number;
  height: number;
  /// Half the stroke width, kept clear at the top and bottom so a line
  /// at either extreme isn't sliced in half by the viewBox edge.
  inset?: number;
};

/// The polyline for a series, and the polygon that fills under it.
/// Null when there is nothing to draw — no parsable sample, or a
/// degenerate box.
export type Spark = { line: string; area: string };

export function sparkline(samples: Sample[], opts: SparkOpts): Spark | null {
  const { nowMs, windowMs, width, height } = opts;
  if (width <= 0 || height <= 0 || windowMs <= 0) return null;

  const points = parsed(samples);
  if (points.length === 0) return null;

  const start = nowMs - windowMs;
  const { min, max } = opts;
  const inset = opts.inset ?? 0.5;
  const span = max - min;

  const xOf = (ms: number) => clamp((ms - start) / windowMs, 0, 1) * width;
  const yOf = (v: number) => {
    const frac = span > 0 ? clamp((v - min) / span, 0, 1) : 0;
    return height - inset - frac * (height - 2 * inset);
  };

  let held = opening(points, start);

  const out: string[] = [];
  // Consecutive duplicates are dropped: two samples that round to the
  // same pixel, or a "step" from a value to itself, add nothing to the
  // picture and make the attribute unreadable in the inspector.
  const put = (x: number, y: number) => {
    const point = `${round(x)},${round(y)}`;
    if (out[out.length - 1] !== point) out.push(point);
  };

  put(0, yOf(held));
  for (const p of points) {
    // At or before the left edge it is carry-in, already folded into
    // `held` above; a value equal to `held` is not a step at all.
    if (p.ms <= start || p.value === held) continue;
    const x = xOf(p.ms);
    // Hold the old value up to the instant it changed, then step.
    put(x, yOf(held));
    held = p.value;
    put(x, yOf(held));
  }
  put(width, yOf(held));

  const line = out.join(" ");
  return {
    line,
    // Down to the floor at both ends, so the fill is the region under
    // the line rather than a closed loop through it.
    area: `0,${round(height)} ${line} ${round(width)},${round(height)}`,
  };
}

/// The span a series is drawn against: every value its line reaches
/// inside the window, and the present. A series that hasn't moved
/// straddles its value, so it draws through the middle of the box
/// rather than pinned to an edge.
export function ownRange(
  value: number,
  samples: Sample[],
  nowMs: number,
  windowMs: number,
): { min: number; max: number } {
  const start = nowMs - windowMs;
  const points = parsed(samples);
  const values = [value, ...points.filter((p) => p.ms > start).map((p) => p.value)];
  if (points.length > 0) values.push(opening(points, start));
  const min = Math.min(...values);
  const max = Math.max(...values);
  if (min !== max) return { min, max };
  return min === 0 ? { min: 0, max: 1 } : { min: min * 0.99, max: max * 1.01 };
}

/// How far a series moved across the window: the present against the
/// value the window opened at. Null when there is no sample to compare
/// against.
export function windowDelta(
  value: number,
  samples: Sample[],
  nowMs: number,
  windowMs: number,
): number | null {
  const points = parsed(samples);
  if (points.length === 0) return null;
  return value - opening(points, nowMs - windowMs);
}

type Point = { ms: number; value: number };

function parsed(samples: Sample[]): Point[] {
  return (
    samples
      .map((s) => ({ ms: Date.parse(s.at), value: s.value }))
      // A stamp we can't read is dropped rather than guessed at: every
      // one of these was written by us, so an unparsable one means a row
      // from somewhere else.
      .filter((p) => Number.isFinite(p.ms))
      .sort((a, b) => a.ms - b.ms)
  );
}

/// The value the window opens at: the newest sample at or before the
/// left edge, else the first sample we have. Without this a series
/// whose only sample predates the window would start from nothing.
function opening(points: Point[], start: number): number {
  let held = points[0].value;
  for (const p of points) {
    if (p.ms > start) break;
    held = p.value;
  }
  return held;
}

function clamp(v: number, lo: number, hi: number): number {
  return v < lo ? lo : v > hi ? hi : v;
}

/// Two decimals is well under a pixel at these sizes, and keeps the
/// attribute short enough to read in the inspector.
function round(v: number): number {
  return Math.round(v * 100) / 100;
}
