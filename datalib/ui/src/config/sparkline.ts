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
//
// Hence: horizontal to the sample's instant at the old value, vertical
// to the new one, and a run out to the right edge at whatever the last
// value was. The response's carry-in sample (the newest one from
// *before* the window) is what gives the left edge a value to start at.
//
// No plotting library. The output is a `points` string for a
// `<polyline>` and one for the `<polygon>` under it, which is the whole
// job — a dependency would be more code to pin and vendor than the
// thirty lines below.

/// One measurement, as the API hands it out.
export type UsageSample = {
  /// ISO-8601 with an explicit offset, per the repo's convention.
  at: string;
  bytes: number;
};

export type SparkOpts = {
  /// The instant at the right edge, epoch ms.
  nowMs: number;
  /// How much time the plot spans, ms. The left edge is
  /// `nowMs - windowMs`.
  windowMs: number;
  /// The value the top of the plot stands for.
  ///
  /// Passing one shared value across a column is what makes its rows
  /// comparable — a per-row maximum would draw a 2 kB source and a
  /// 40 GB one as the same shape.
  max: number;
  /// The value the bottom stands for. Zero for a calibrated column,
  /// where a row's height should mean its size. Non-zero only where
  /// the question is the *shape* of a change too small to see against
  /// zero — the whole root's total, which moves by fractions of a
  /// percent.
  min?: number;
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

export function sparkline(samples: UsageSample[], opts: SparkOpts): Spark | null {
  const { nowMs, windowMs, width, height } = opts;
  if (width <= 0 || height <= 0 || windowMs <= 0) return null;

  const points = samples
    .map((s) => ({ ms: Date.parse(s.at), bytes: s.bytes }))
    // A stamp we can't read is dropped rather than guessed at: every
    // one of these was written by us, so an unparsable one means a row
    // from somewhere else.
    .filter((p) => Number.isFinite(p.ms))
    .sort((a, b) => a.ms - b.ms);
  if (points.length === 0) return null;

  const start = nowMs - windowMs;
  const min = opts.min ?? 0;
  const inset = opts.inset ?? 0.5;
  const span = opts.max - min;

  const xOf = (ms: number) => clamp((ms - start) / windowMs, 0, 1) * width;
  const yOf = (v: number) => {
    const frac = span > 0 ? clamp((v - min) / span, 0, 1) : 0;
    return height - inset - frac * (height - 2 * inset);
  };

  // The value the window opens at: the newest sample at or before the
  // left edge, else the first sample we have. Without this a series
  // whose only sample predates the window would start from nothing.
  let held = points[0].bytes;
  for (const p of points) {
    if (p.ms > start) break;
    held = p.bytes;
  }

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
    if (p.ms <= start || p.bytes === held) continue;
    const x = xOf(p.ms);
    // Hold the old value up to the instant it changed, then step.
    put(x, yOf(held));
    held = p.bytes;
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

/// The largest value a set of series reaches, current values included.
///
/// This is the number a calibrated column is drawn against: it has to
/// cover the history as well as the present, or a row that has just
/// shrunk would draw its own past off the top of the box.
export function calibrationMax(
  series: { bytes: number | null; history: UsageSample[] }[],
): number {
  let max = 0;
  for (const s of series) {
    if (s.bytes !== null) max = Math.max(max, s.bytes);
    for (const h of s.history) max = Math.max(max, h.bytes);
  }
  return max;
}

function clamp(v: number, lo: number, hi: number): number {
  return v < lo ? lo : v > hi ? hi : v;
}

/// Two decimals is well under a pixel at these sizes, and keeps the
/// attribute short enough to read in the inspector.
function round(v: number): number {
  return Math.round(v * 100) / 100;
}
