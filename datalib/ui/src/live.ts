// The page's one live connection to the server.
//
// `GET /api/sync/stream` carries two kinds of frame: unnamed ones for
// sync-job progress, and `root` ones for everything that changes in the
// data root without a job behind it — the config, the runner's record,
// the component store — plus a heartbeat. The backend half is
// `datalib/backend/http/src/watch.rs`, which explains why the second
// kind exists at all.
//
// This module exists for three reasons, in ascending order of how much
// trouble each was causing.
//
// ## One connection, not four
//
// Every consumer used to call `openJobStream` and get an `EventSource`
// of its own. The header's sync indicator is always mounted, the open
// view has one, and each `sourceDagView` card adds another — so three
// or four connections to the same origin, each holding a socket open
// forever. Browsers allow six per origin over HTTP/1.1, and an SSE
// connection never returns one. The app was two DAG cards away from
// starving its own `fetch` calls with no symptom but hanging requests.
// Here there is one connection however many subscribers there are.
//
// ## A stream that dies quietly
//
// `EventSource` reconnects on its own only when it *notices* a drop.
// A proxy that stops forwarding, or a laptop that slept, looks exactly
// like a server with nothing to say — and nothing here set `onerror`,
// so a dead stream was indistinguishable from a quiet one, forever.
// Every consumer hedged with an unconditional slow poll: 15 s in the
// header, 15 s in Sources, 5 s in the Pipeline table, whether or not
// anything was wrong.
//
// The server now sends a heartbeat every 10 s, which turns that into a
// question with an answer: no frame for `STALL_MS` means the stream is
// gone. So we reconnect and tell subscribers to reconcile *once*,
// instead of refetching forever against the possibility.
//
// ## A backgrounded tab is not a stalled one
//
// Browsers throttle timers in hidden tabs to about once a minute, so a
// watchdog running there would fire on its own throttling and reconnect
// a perfectly healthy stream. The stall check is therefore suspended
// while hidden, and becoming visible triggers one reconcile — which is
// also what a tab that genuinely missed frames needs.

import type { JobProgressEvent } from "@/api";

/// One `root` frame. Mirrors `watch::RootEvent`; see that module for
/// what each kind covers and why the frame carries no payload (every
/// consumer already diffs what it fetches, so the event only has to say
/// "ask again").
export type RootEvent =
  | { kind: "config_changed" }
  | { kind: "dag_changed" }
  | { kind: "frontend_changed" }
  | { kind: "heartbeat" };

export type LiveHandlers = {
  /// A sync job moved.
  job?: (e: JobProgressEvent) => void;
  /// Something in the data root moved. Heartbeats are handled here and
  /// are not delivered — a subscriber never has to know about them.
  root?: (e: RootEvent) => void;
  /// "You may have missed frames — refetch what you hold." Fires after
  /// a reconnect and when a hidden tab comes back. Deliberately not
  /// fired on the first connect: subscribers load their initial state
  /// themselves, and a resync there would double every mount.
  resync?: () => void;
};

export type Unsubscribe = () => void;

/// How long without any frame counts as a dead stream. The server beats
/// every 10 s (`watch::HEARTBEAT`), so this is three missed beats — long
/// enough that a slow network or a busy main thread doesn't trip it,
/// short enough that a genuinely dropped stream is noticed while the
/// user is still looking at the same screen.
const STALL_MS = 35_000;

const subscribers = new Set<LiveHandlers>();
let source: EventSource | null = null;
let watchdog: ReturnType<typeof setTimeout> | null = null;
let visibilityBound = false;

function fanOut(pick: (h: LiveHandlers) => void) {
  // Copy first: a handler is allowed to unsubscribe itself, which would
  // otherwise mutate the set mid-iteration.
  for (const h of [...subscribers]) {
    try {
      pick(h);
    } catch (e) {
      // One bad subscriber must not take down the others, or the
      // connection.
      console.error("live: subscriber threw", e);
    }
  }
}

function armWatchdog() {
  if (watchdog) clearTimeout(watchdog);
  watchdog = null;
  // A hidden tab's timers are throttled to roughly once a minute, so a
  // watchdog there measures the throttle rather than the stream. The
  // visibility handler covers what is missed instead.
  if (typeof document !== "undefined" && document.hidden) return;
  watchdog = setTimeout(() => {
    // Nothing for three beats. Assume the connection is gone whatever
    // `readyState` claims — the case this exists for is precisely the
    // one where the browser still believes it is open.
    reconnect();
  }, STALL_MS);
}

function reconnect() {
  if (source) source.close();
  source = null;
  connect();
  fanOut((h) => h.resync?.());
}

function connect() {
  if (source || subscribers.size === 0) return;
  const es = new EventSource("/api/sync/stream");
  source = es;

  es.onmessage = (m) => {
    armWatchdog();
    let ev: JobProgressEvent;
    try {
      ev = JSON.parse(m.data) as JobProgressEvent;
    } catch {
      return; // malformed frame
    }
    fanOut((h) => h.job?.(ev));
  };

  es.addEventListener("root", (m) => {
    armWatchdog();
    let ev: RootEvent;
    try {
      ev = JSON.parse((m as MessageEvent).data) as RootEvent;
    } catch {
      return;
    }
    // The heartbeat's whole job was rearming the watchdog above.
    if (ev.kind === "heartbeat") return;
    fanOut((h) => h.root?.(ev));
  });

  es.onerror = () => {
    // `source !== es` means this handler belongs to a connection we
    // have already replaced. Its error is history; acting on it would
    // tear down the live one and reconnect in a loop.
    if (source !== es) return;
    // `CONNECTING` means the browser is already retrying by itself, and
    // racing it with a second connection is worse than waiting — if its
    // retry never lands, the watchdog is still running and will take
    // over. `CLOSED` means it has given up, and only we can restart it.
    if (es.readyState === EventSource.CLOSED) reconnect();
  };

  armWatchdog();

  if (!visibilityBound && typeof document !== "undefined") {
    visibilityBound = true;
    document.addEventListener("visibilitychange", () => {
      if (document.hidden) {
        // Stop measuring a clock that is about to be throttled.
        if (watchdog) clearTimeout(watchdog);
        watchdog = null;
        return;
      }
      // Back in the foreground. Whatever the stream did while we were
      // away, one reconcile settles it — and the watchdog starts again
      // from now rather than from whenever the last frame arrived.
      armWatchdog();
      fanOut((h) => h.resync?.());
    });
  }
}

/// Subscribe to the live stream, opening the connection if this is the
/// first subscriber. The returned function unsubscribes, and closes the
/// connection when the last subscriber leaves — so a component can call
/// this in `onMounted` and the teardown in `onUnmounted` without
/// knowing whether anyone else is listening.
export function subscribeLive(handlers: LiveHandlers): Unsubscribe {
  subscribers.add(handlers);
  connect();
  let done = false;
  return () => {
    if (done) return;
    done = true;
    subscribers.delete(handlers);
    if (subscribers.size === 0) {
      if (watchdog) clearTimeout(watchdog);
      watchdog = null;
      source?.close();
      source = null;
    }
  };
}
