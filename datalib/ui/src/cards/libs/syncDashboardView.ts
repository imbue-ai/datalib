// `syncDashboardView({ group, step })` in card source: one group's sync
// as a dashboard — its row and each step's, laid out vertically with
// charts over the run and the group's log (cards/SyncDashboardCard.ce.vue).
// Opened from a Manage row's menu or its Queue or ETA cell; `step`
// scrolls to that step's section.
import SyncDashboardCard from "../SyncDashboardCard.ce.vue";
import TimeChart from "../TimeChart.ce.vue";
import RunLogPanel from "@/components/RunLogPanel.ce.vue";
import tableGridCss from "../tableGrid.css?inline";
// uPlot's own layout rules (the canvas, the cursor and the overlay),
// which have to be inside the card's shadow root to reach them.
import uplotCss from "uplot/dist/uPlot.min.css?inline";
// The log panel's grid needs its theme in the same root.
import slickCss from "@slickgrid-universal/common/dist/styles/css/slickgrid-theme-default.css?inline";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

export type SyncDashboardOpts = {
  group: string;
  step?: string;
};

export function syncDashboardView(opts: SyncDashboardOpts): CardRender {
  return vueCard(
    SyncDashboardCard,
    { opts },
    { styleSources: [slickCss, tableGridCss, uplotCss, TimeChart, RunLogPanel] },
  );
}

/// The source of a dashboard card, for whoever opens one.
export function syncDashboardSource(opts: SyncDashboardOpts): string {
  return `syncDashboardView(${JSON.stringify(opts)})`;
}
