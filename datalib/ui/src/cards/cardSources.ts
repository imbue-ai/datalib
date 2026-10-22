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

export type SyncDashboardOpts = {
  group: string;
  step?: string;
};

/// The source of a dashboard card, for whoever opens one.
export function syncDashboardSource(opts: SyncDashboardOpts): string {
  return `syncDashboardView(${JSON.stringify(opts)})`;
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
/// a tab (`containerTree.ts::tabShowing`). A group's sync dashboard is one
/// per window, wherever it was opened, and its `step` only says where to
/// scroll. Every other card is the same card only by the same source, and
/// only in a tab near its opener (docs/dev/cards.md § "An open that shows
/// a tab already there").
export function sameCard(source: string): { key: string; anywhere: boolean } {
  try {
    const call = parseCardSource(source);
    const opts = call.args[0] as Partial<SyncDashboardOpts> | undefined;
    if (call.name === "syncDashboardView" && typeof opts?.group === "string") {
      return { key: `syncDashboardView:${opts.group}`, anywhere: true };
    }
  } catch {
    // A source that does not parse is only ever itself.
  }
  return { key: source, anywhere: false };
}

/// The step a sync dashboard's source names, or null.
export function syncDashboardStep(source: string): string | null {
  try {
    const opts = parseCardSource(source).args[0] as Partial<SyncDashboardOpts> | undefined;
    return typeof opts?.step === "string" ? opts.step : null;
  } catch {
    return null;
  }
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
