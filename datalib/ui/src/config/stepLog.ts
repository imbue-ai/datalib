// One step's share of a job log.
//
// A job log is the whole run: `datalib-dag`'s NDJSON event stream for
// every step in it, interleaved. The question a red Status raises is
// about exactly one of them — "why did *this* fail?" — and answering it
// from the raw file means reading past every other step's chatter and
// then past a second layer of JSON, because a step's `log` events carry
// a whole `tracing` envelope escaped inside `msg`.
//
// So this narrows on two axes at once: to the step, and to the sentence.
// What survives is what a person would have written down.
//
// Kept out of the view, and pure, so the unwrapping rules can be tested
// against real log lines rather than eyeballed through a modal.

/// One line, ready to paint.
export type StepLogLine = {
  /// The runner's own timestamp for the event, ISO-8601 with offset.
  /// Null for a line we could not attribute one to.
  ts: string | null;
  level: "info" | "warn" | "error";
  text: string;
};

/// Envelope keys that describe *where the line came from* rather than
/// what happened. A file and a line number are the right thing to log
/// and the wrong thing to show someone asking why their sync is red.
const ENVELOPE_NOISE = new Set([
  "timestamp",
  "level",
  "target",
  "filename",
  "line_number",
  "threadId",
]);

/// Field keys already spent on the line's own prose, so they don't get
/// repeated as `k=v` after it.
const SPENT = new Set(["message", "event"]);

function normalizeLevel(raw: unknown): "info" | "warn" | "error" | null {
  if (typeof raw !== "string") return null;
  const s = raw.toLowerCase();
  if (s === "error" || s === "fatal") return "error";
  if (s === "warn" || s === "warning") return "warn";
  if (s === "info" || s === "debug" || s === "trace") return "info";
  return null;
}

/// Render a `fields` bag's leftovers as `k=v`, in the order the step
/// wrote them.
function trailing(fields: Record<string, unknown>): string {
  return Object.entries(fields)
    .filter(([k]) => !SPENT.has(k))
    .map(([k, v]) => `${k}=${typeof v === "string" ? v : JSON.stringify(v)}`)
    .join(" ");
}

/// Pull the human sentence out of a `log` event's `msg`.
///
/// `msg` is one of two things and there is no flag saying which: a
/// plain string, or a `tracing` JSON envelope serialized into a string.
/// The steps emit *both* for the same event — the envelope and then the
/// bare line — which is why the caller dedupes.
function unwrapMessage(msg: string): { text: string; level: "info" | "warn" | "error" | null } {
  let parsed: unknown;
  try {
    parsed = JSON.parse(msg);
  } catch {
    return { text: msg, level: null };
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    return { text: msg, level: null };
  }
  const obj = parsed as Record<string, unknown>;
  const fields = obj.fields;
  if (!fields || typeof fields !== "object" || Array.isArray(fields)) {
    return { text: msg, level: null };
  }
  const bag = fields as Record<string, unknown>;
  // `message` is the ordinary case; `event` is what the providers use
  // for their structured "this happened" records, which have no prose
  // at all — the event name is the sentence.
  const head =
    typeof bag.message === "string"
      ? bag.message
      : typeof bag.event === "string"
        ? bag.event
        : "";
  const rest = trailing(bag);
  const extra = Object.entries(obj)
    .filter(([k]) => !ENVELOPE_NOISE.has(k) && k !== "fields")
    .map(([k, v]) => `${k}=${typeof v === "string" ? v : JSON.stringify(v)}`)
    .join(" ");
  const text = [head, rest, extra].filter(Boolean).join(" ");
  return { text: text || msg, level: normalizeLevel(obj.level) };
}

/// Every line in `logText` that belongs to `stepId`, unwrapped.
///
/// Non-JSON lines are dropped: `datalib-dag` prints a human summary
/// table after the stream, and it names every step, so keeping those
/// would put other steps' outcomes into this step's log.
///
/// `progress_inc` is dropped too — one event per item, saying nothing a
/// reader wants, and the Status column already draws the bar.
export function stepLogLines(logText: string, stepId: string): StepLogLine[] {
  const out: StepLogLine[] = [];
  for (const raw of logText.split("\n")) {
    const line = raw.trim();
    if (!line.startsWith("{")) continue;
    let e: Record<string, unknown>;
    try {
      const parsed = JSON.parse(line);
      if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) continue;
      e = parsed as Record<string, unknown>;
    } catch {
      continue;
    }
    if (e.step !== stepId) continue;
    const ts = typeof e.ts === "string" ? e.ts : null;

    let text = "";
    let level: "info" | "warn" | "error" = "info";
    switch (e.event) {
      case "step_start":
        text = typeof e.attempt === "number" && e.attempt > 1
          ? `started (attempt ${e.attempt})`
          : "started";
        break;
      case "step_finish": {
        const status = typeof e.status === "string" ? e.status : "finished";
        const err = typeof e.error === "string" ? e.error : "";
        text = err ? `${status}: ${err}` : status;
        level = status === "failed" ? "error" : "info";
        break;
      }
      case "progress_length":
        if (typeof e.total !== "number") continue;
        text = `${e.total} to do`;
        break;
      case "log": {
        const msg = typeof e.msg === "string" ? e.msg : "";
        if (!msg) continue;
        const un = unwrapMessage(msg);
        text = un.text;
        level = un.level ?? normalizeLevel(e.level) ?? "info";
        break;
      }
      default:
        continue;
    }
    if (!text) continue;
    // The envelope and the bare line say the same thing one after the
    // other. Keep the first, which is the one carrying the fields.
    const prev = out[out.length - 1];
    if (prev && prev.text === text) continue;
    if (prev && prev.text.startsWith(text) && prev.level === level) continue;
    out.push({ ts, level, text });
  }
  return out;
}
