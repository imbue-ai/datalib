// The Dashboard's sections, each a card of its own, which the Dashboard
// composite (views/composites.ts) lays out as a Page: the sync bar, what
// needs the person, the library's size, each source's state, and the
// newest documents. They share one look (dashboardCard.css) and one way
// of loading (useDashboard.ts).
import type { Component } from "vue";
import DashboardSection from "../DashboardSection.ce.vue";
import DashboardSyncBar from "../DashboardSyncBar.ce.vue";
import DashboardNeedsYou from "../DashboardNeedsYou.ce.vue";
import DashboardLibrary from "../DashboardLibrary.ce.vue";
import DashboardSources from "../DashboardSources.ce.vue";
import DashboardActivity from "../DashboardActivity.ce.vue";
import dashboardCss from "../dashboardCard.css?inline";
import { BUILTIN_META } from "../catalog";
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

// A section's card is titled as the catalog names it ("Dashboard: …"),
// which is what its tab reads when it is opened on its own.
function section(
  component: Component,
  title: string,
  opts: { recent?: boolean; flush?: boolean } = {},
): CardRender {
  return vueCard(
    DashboardSection,
    { section: component, title, ...opts },
    { styleSources: [dashboardCss] },
  );
}

export function syncStatusView(): CardRender {
  return section(DashboardSyncBar, BUILTIN_META.syncStatusView.title, { flush: true });
}

export function needsYouView(): CardRender {
  return section(DashboardNeedsYou, BUILTIN_META.needsYouView.title);
}

export function libraryView(): CardRender {
  return section(DashboardLibrary, BUILTIN_META.libraryView.title);
}

export function sourcesOverviewView(): CardRender {
  return section(DashboardSources, BUILTIN_META.sourcesOverviewView.title);
}

export function latestActivityView(): CardRender {
  return section(DashboardActivity, BUILTIN_META.latestActivityView.title, { recent: true });
}
