// `nodeView({ node, name, doc })` in card source: one node of the
// library's tree as the Search card held to it (`scopeOf`): what is
// directly in it, newest first, beside a preview, with a box that searches
// within it. A node that is a document itself (a page, a chat) leads with
// that document.
import GridCard from "../GridCard.ce.vue";
import { DEFAULT_QUERY } from "../searchDefaults";
import { vueCard } from "../vueCard";
import type { NodeViewOpts } from "../cardSources";
import { nodeCardProps, scopeOf } from "../library";
import type { CardRender } from "../types";
import { STYLES } from "./gridView";

export function nodeView(opts?: Partial<NodeViewOpts>): CardRender {
  const { node, name, doc } = nodeCardProps(opts);
  return vueCard(
    GridCard,
    {
      q: DEFAULT_QUERY,
      scope: scopeOf(node),
      name: name ?? "Library",
      lead: doc ?? undefined,
      view: "list",
      oneSource: true,
    },
    STYLES,
  );
}
