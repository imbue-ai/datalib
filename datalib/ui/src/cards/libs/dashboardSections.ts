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
import { vueCard } from "../vueCard";
import type { CardRender } from "../types";

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
  return section(DashboardSyncBar, "Sync", { flush: true });
}

export function needsYouView(): CardRender {
  return section(DashboardNeedsYou, "Needs you");
}

export function libraryView(): CardRender {
  return section(DashboardLibrary, "Your library");
}

export function sourcesOverviewView(): CardRender {
  return section(DashboardSources, "Sources");
}

export function latestActivityView(): CardRender {
  return section(DashboardActivity, "Latest activity", { recent: true });
}
