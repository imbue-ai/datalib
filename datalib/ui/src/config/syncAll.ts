// The Manage header's one sync button: Sync everything while nothing is
// syncing, Stop everything while anything is. It reads the rows, so it
// agrees with the per-row buttons it sits above.

import type { Action } from "../api";

export type SyncAllRow = { actions: Action[]; stop_request_ids: string[] };

export type SyncAllButton = {
  glyph: "sync" | "stop";
  /// The tooltip and the accessible name: the button shows no text.
  label: string;
  /// Why it is disabled, or null.
  blocked: string | null;
  /// What a click stops; empty for Sync everything.
  stops: string[];
};

export function syncAllButton(rows: SyncAllRow[]): SyncAllButton {
  if (rows.length === 0) {
    return {
      glyph: "sync",
      label: "Sync everything",
      blocked: "Nothing configured yet.",
      stops: [],
    };
  }
  const stops = [...new Set(rows.flatMap((r) => r.stop_request_ids))];
  const stopButtons = rows.flatMap((r) => r.actions.filter((a) => a.id === "stop"));
  if (stopButtons.length === 0) {
    return { glyph: "sync", label: "Sync everything", blocked: null, stops: [] };
  }
  if (!stopButtons.some((a) => a.enabled)) {
    return {
      glyph: "stop",
      label: "Stopping everything",
      blocked: "Stopping everything — the steps are checkpointing and exiting.",
      stops,
    };
  }
  return { glyph: "stop", label: "Stop everything", blocked: null, stops };
}
