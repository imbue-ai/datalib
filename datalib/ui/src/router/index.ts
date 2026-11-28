import { fetchHealth, fetchNode, healthSnapshot } from "@/api";
import { documentSource, nodeSource } from "@/cards/cardSources";
import { chatUuidFromHref } from "@/cards/chatLink";
import { ensureFrontend } from "@/cards/frontendRegistry";
import { photoCardSource } from "@/cards/photos";
import { encodeColumns } from "@/router/columns";
import { linkOfPath, routeOfLink, type Named } from "@/router/names";
import { pushToast } from "@/toasts";
import { createRouter, createWebHistory } from "vue-router";

// History-mode routing. A path naming cards — one segment per card,
// `code:state` (see `router/columns.ts`) — is a link: the card layout
// opens those cards as a tab and puts the address back to `/`, since
// what is open is kept in the library, not the URL. The routed
// component is `CardsView`.
//
// A `/name/<kind>/<id>` address (`router/names.ts`) opens the card what
// it names opens, before any route is matched.
//
// The catchall MUST come after the explicit routes (the
// `/data_sources` redirect); Vue Router does prefer specific over param routes by
// path-rank, but order is the simpler invariant.
/// The card stack `/data_sources` opens, and where a just-initialized
/// library lands: the sources tree alone, at 1.6× the default column
/// width. The config editor is a click away from that card, not open
/// beside it — the first thing to do on the screen is add a source.
export const MANAGE_STACK = encodeColumns([{ code: "sourcesView()", size: 1.6, state: "" }]);

const router = createRouter({
  history: createWebHistory(),
  routes: [
    // The sources card on the card surface. The path stays so links,
    // muscle memory and the launch URL keep working.
    { path: "/data_sources", redirect: MANAGE_STACK },
    {
      path: "/:stack(.*)*",
      name: "cards",
      component: () => import("@/views/CardsView.vue"),
    },
  ],
});

// The link every renderer writes into a document body — `/chat/<uuid>`,
// with or without a `#/` in front — is a page of its own: the document
// alone. A plain click never gets here (the document card opens the
// target beside itself); a new tab, a bookmark or a pasted link does.
export function documentStack(markdownUuid: string): string {
  return stackOf(documentSource(markdownUuid));
}

function stackOf(code: string): string {
  return encodeColumns([{ code, size: null, state: "" }]);
}

/// The card a name opens, as its chip's double-click opens it; null,
/// having said why, when there is none here.
async function cardOfName(named: Named): Promise<string | null> {
  switch (named.kind) {
    case "doc":
      return documentSource(named.page, named.section);
    case "node": {
      const node = await fetchNode(named.id).catch(() => null);
      if (!node) pushToast(`This library has no node ${named.id}`);
      return node && nodeSource({ node: node.uuid, name: node.name, doc: node.markdown_uuid });
    }
    case "photo": {
      await ensureFrontend();
      const source = photoCardSource(named.id);
      if (!source) pushToast("No applet in this library shows photos");
      return source;
    }
  }
}

/// A `/name/…` address opens what it names as a tab, and the address goes
/// back to `/` as a card path's does; one that opens nothing lands on `/`.
async function stackOfName(link: string): Promise<string> {
  const here = (healthSnapshot() ?? (await fetchHealth().catch(() => null)))?.library_id ?? null;
  const route = routeOfLink(link, here);
  if ("refused" in route) {
    pushToast(route.refused);
    return "/";
  }
  const card = await cardOfName(route.open);
  return card ? stackOf(card) : "/";
}

router.beforeEach((to) => {
  const link = linkOfPath(to.fullPath);
  if (link) return stackOfName(link);
  const uuid = chatUuidFromHref(to.fullPath);
  return uuid ? documentStack(uuid) : true;
});

export default router;
