// The card sources other cards open, as strings, with nothing else
// attached: a chip or a menu that only needs `logView(…)` to hand to
// `openCards` should not pull the card's components in with it.

export type LogViewOpts = {
  run?: string | null;
  step?: string | null;
  launch?: string | null;
  q?: string;
  jumpToEnd?: boolean;
};

/// The source of a log card, for whoever opens one.
export function logSource(opts: LogViewOpts): string {
  return `logView(${JSON.stringify(opts)})`;
}

export type SyncDashboardOpts = {
  group: string;
  step?: string;
};

/// The source of a dashboard card, for whoever opens one.
export function syncDashboardSource(opts: SyncDashboardOpts): string {
  return `syncDashboardView(${JSON.stringify(opts)})`;
}

export type PersonViewOpts = {
  /** The source the chip was seen in: its record of the person leads. */
  seenIn?: string | null;
};

/// The source of a person card, for a chip that opens one.
export function personSource(handle: string, opts: PersonViewOpts = {}): string {
  return `personView(${JSON.stringify(handle)}, ${JSON.stringify(opts)})`;
}

/// The source of a search card on `q`.
export function searchSource(q: string): string {
  return `searchView(${JSON.stringify({ q })})`;
}
