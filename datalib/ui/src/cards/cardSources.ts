// The card sources other cards open, as strings, with nothing else
// attached: a chip or a menu that only needs `logView(…)` to hand to
// `openCards` should not pull the card's components in with it.
import { parseCardSource } from "./cardCall";

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

/// Which half of a source's card shows: its documents, or its sync.
export type SourceTab = "documents" | "sync";

/// A source's card: `step` opens its Sync tab at that step's section;
/// without `tab` it opens on its documents, where it has any.
export type SourceViewOpts = {
  group: string;
  step?: string;
  tab?: SourceTab;
};

/// The source of a source's card, for whoever opens one.
export function sourceSource(opts: SourceViewOpts): string {
  return `sourceView(${JSON.stringify(opts)})`;
}

export type SyncDashboardOpts = {
  group: string;
  step?: string;
};

/// A source's card on its Sync tab: what "Show sync dashboard" opens.
/// `syncDashboardView(…)`, the card's older name, still opens it there,
/// so a saved layout or a link written before keeps working.
export function syncDashboardSource(opts: SyncDashboardOpts): string {
  return sourceSource({ ...opts, tab: "sync" });
}

/// What a source's card source asks for, under either name; null for
/// any other card.
export function sourceCardOpts(source: string): SourceViewOpts | null {
  try {
    const call = parseCardSource(source);
    const opts = call.args[0] as Partial<SourceViewOpts> | undefined;
    if (typeof opts?.group !== "string") return null;
    const step = typeof opts.step === "string" ? opts.step : undefined;
    if (call.name === "syncDashboardView") return { group: opts.group, step, tab: "sync" };
    if (call.name !== "sourceView") return null;
    const tab = opts.tab === "documents" || opts.tab === "sync" ? opts.tab : undefined;
    return { group: opts.group, step, tab };
  } catch {
    // A source that does not parse is no source's card.
    return null;
  }
}

export type HistoryViewOpts = {
  /// Group or step ids; every store under each is listed.
  trees: string[];
  /// What the history is of, for the card's title.
  title: string;
  /// The source whose download's commits can be compared, when the
  /// history is a comparable source's.
  source?: string | null;
  /// Open with a comparison of the newest two commits set up.
  compare?: boolean;
};

/// The source of a history card, for whoever opens one.
export function historySource(opts: HistoryViewOpts): string {
  return `historyView(${JSON.stringify(opts)})`;
}

/// What makes two card sources the same card, for an open that would add
/// a tab (`containerTree.ts::tabShowing`). A source's card is one per
/// window, wherever it was opened and under either name; its `step` and
/// `tab` only say where to look. Every other card is the same card only by the same source, and
/// only in a tab near its opener (docs/dev/cards.md § "An open that shows
/// a tab already there").
export function sameCard(source: string): { key: string; anywhere: boolean } {
  const card = sourceCardOpts(source);
  return card ? { key: `source:${card.group}`, anywhere: true } : { key: source, anywhere: false };
}

export type PersonViewOpts = {
  /** The source the chip was seen in: its record of the person leads. */
  seenIn?: string | null;
};

/// The source of a person card, for a chip that opens one.
export function personSource(handle: string, opts: PersonViewOpts = {}): string {
  return `personView(${JSON.stringify(handle)}, ${JSON.stringify(opts)})`;
}

/// The source of the contacts app's table.
export function contactsSource(): string {
  return "contactsView()";
}

/// The source of the Time series card.
export function timeSeriesSource(): string {
  return "timeSeriesView()";
}

/// What an applet row's Browse opens: the view of an applet whose rows
/// are its own (the server enables Browse on exactly those), else
/// nothing.
export function appletBrowseSource(appletId: string): string | null {
  if (appletId === "datalib_contacts") return contactsSource();
  if (appletId === "unified_time_series_index") return timeSeriesSource();
  return null;
}

/// The source of a search card on `q`.
export function searchSource(q: string): string {
  return `searchView(${JSON.stringify({ q })})`;
}
