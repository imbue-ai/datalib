// Whether the server still knows this tab. Each run of the server mints
// a new token (`http/src/auth.rs`), so a tab left open across a restart
// is refused from then on and nothing it asks is answered again.

const listeners = new Set<() => void>();
let ended = false;

export function sessionEnded(): boolean {
  return ended;
}

export function onSessionEnded(listener: () => void): void {
  listeners.add(listener);
}

/// `status` is how the server answered `url`. Only the server's own
/// routes speak for the session: an applet's 401 is its gateway's.
export function noteAnswer(url: string, status: number): void {
  if (status !== 401 || ended) return;
  if (!new URL(url, window.location.href).pathname.startsWith("/api/")) return;
  ended = true;
  for (const listener of listeners) listener();
}
