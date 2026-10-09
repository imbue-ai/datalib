// What the toolbar's sync indicator says and lists, read off the Sources
// table's rows: a group with a stoppable request is syncing.
import type { ManageRow } from "@/api";
import { quantityText } from "@/cards/cellRenderers";

export type SyncingGroup = {
  id: string;
  name: string;
  /// A source, as against the search index every source's sync reaches.
  source: boolean;
  /// "1,204 to go · 3 min left", or "" when the steps report neither.
  progress: string;
  requestIds: string[];
};

export function syncingGroups(rows: ManageRow[]): SyncingGroup[] {
  return rows
    .filter((r) => r.kind === "group" && r.stop_request_ids.length > 0)
    .map((r) => ({
      id: r.id,
      name: r.name.label,
      source: r.type !== null,
      progress: [
        quantityText(r.queue) && `${quantityText(r.queue)} to go`,
        quantityText(r.eta) && `${quantityText(r.eta)} left`,
      ]
        .filter(Boolean)
        .join(" · "),
      requestIds: r.stop_request_ids,
    }))
    .sort((a, b) => Number(b.source) - Number(a.source));
}

/// The sources are what a person asked to sync; the index follows them,
/// so it is counted only when it is all that is left.
export function pillLabel(groups: SyncingGroup[]): string {
  const sources = groups.filter((g) => g.source).length;
  if (sources > 0) return `Syncing ${sources} ${sources === 1 ? "source" : "sources"}`;
  if (groups.length === 1) return `Syncing ${groups[0].name}`;
  return "Syncing";
}
