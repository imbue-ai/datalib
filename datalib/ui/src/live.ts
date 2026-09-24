// The page's one live connection to the server.
//
// Every consumer used to open an `EventSource` of its own. The toolbar's sync indicator is always mounted, the open
// view has one, and each `sourceDagView` card adds another — so three
// or four connections to the same origin, each holding a socket open
// forever. Browsers allow six per origin over HTTP/1.1, and an SSE
// connection never returns one. The app was two DAG cards away from
// starving its own `fetch` calls with no symptom but hanging requests.
// Here there is one connection however many subscribers there are.

/// A dataset the server serves, named by what serves it. Mirrors
/// `watch::Table` by hand. A consumer names the ones it reads and
/// refetches on those; a change to anything else never reaches it.
export type LiveTable = "dag" | "manage.rows" | "runs" | "log" | "storage";

/// One `root` frame. Mirrors `watch::RootFrame`; see that module for
/// what each kind covers and why the frame carries no payload (every
/// consumer already diffs what it fetches, so the event only has to say
/// "ask again"). `chain` is set when a request's own effect moved: a
/// fetch made while the frame is handled echoes it (`CAUSE_HEADER`), which
/// is how the server counts a page refetching on its own echo
/// (`loop_guard.rs`).
export type RootEvent = (
  | { kind: "config_changed" }
  | { kind: "table_changed"; table: LiveTable }
  | { kind: "frontend_changed" }
  | { kind: "index_changed" }
  | { kind: "heartbeat" }
) & { chain?: number };

export const CAUSE_HEADER = "X-Datalib-Cause";

/// The chain of the frame being handled right now (0 for a frame that
/// carries none), for the fetch wrapper in `telemetry.ts`: a fetch that
/// sends it is a live refetch, which the server logs at `debug`. Only a
/// fetch started synchronously inside a handler sees it; one started
/// after an await or a timer does not, and counts as nobody's echo.
let handlingChain: number | undefined;

export function chainOfFrameBeingHandled(): number | undefined {
  return handlingChain;
}

function handling(e: RootEvent, deliver: (e: RootEvent) => void) {
  handlingChain = e.chain ?? 0;
  try {
    deliver(e);
  } finally {
    handlingChain = undefined;
  }
}

/// Whether a frame says `table` should be fetched again.
export function changed(e: RootEvent, table: LiveTable): boolean {
  return e.kind === "table_changed" && e.table === table;
}

export type LiveHandlers = {
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

export type LiveOptions = {
  /// Deliver `root` frames and resyncs only while this element is on
  /// screen. A card in a hidden tab or layout stays mounted, and without
  /// this it refetches on every frame for nobody to see.
  onScreen?: Element;
};

/// `inner`, holding back what arrives while it is off screen and
/// delivering it on the way back: each frame once however often it came,
/// or one resync in place of them all, since that refetches everything.
export function holdWhileOffScreen(inner: LiveHandlers): {
  handlers: LiveHandlers;
  setOnScreen: (onScreen: boolean) => void;
} {
  let onScreen = true;
  const frames = new Map<string, RootEvent>();
  let resync = false;
  const root = inner.root;
  const handlers: LiveHandlers = {
    root:
      root &&
      ((e) => {
        if (onScreen) return root(e);
        const event = { ...e };
        delete event.chain;
        frames.set(JSON.stringify(event), event);
      }),
    resync:
      inner.resync &&
      (() => {
        if (onScreen) inner.resync?.();
        else resync = true;
      }),
  };
  function setOnScreen(next: boolean) {
    if (next === onScreen) return;
    onScreen = next;
    if (!onScreen) return;
    const held = [...frames.values()];
    frames.clear();
    if (resync) {
      resync = false;
      inner.resync?.();
      return;
    }
    for (const e of held) handling(e, (f) => root?.(f));
  }
  return { handlers, setOnScreen };
}

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
    handling(ev, (e) => fanOut((h) => h.root?.(e)));
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
export function subscribeLive(handlers: LiveHandlers, opts: LiveOptions = {}): Unsubscribe {
  let observer: IntersectionObserver | null = null;
  if (opts.onScreen && typeof IntersectionObserver !== "undefined") {
    const held = holdWhileOffScreen(handlers);
    handlers = held.handlers;
    // `display: none` — a hidden tab or layout — never intersects.
    observer = new IntersectionObserver((entries) => {
      const last = entries[entries.length - 1];
      if (last) held.setOnScreen(last.isIntersecting);
    });
    observer.observe(opts.onScreen);
  }
  subscribers.add(handlers);
  connect();
  let done = false;
  return () => {
    if (done) return;
    done = true;
    observer?.disconnect();
    subscribers.delete(handlers);
    if (subscribers.size === 0) {
      if (watchdog) clearTimeout(watchdog);
      watchdog = null;
      source?.close();
      source = null;
    }
  };
}
