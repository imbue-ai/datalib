// `nodeView({ node, name })` in card source: one node of the library's
// tree, as a list of what it holds beside a preview. See
// cards/NodeCard.ce.vue.
import NodeCard from "../NodeCard.ce.vue";
import DocCard from "../DocCard.ce.vue";
import ChatBody from "../ChatBody.ce.vue";
import ChipMenu from "../ChipMenu.ce.vue";
import DocTitleBar from "../DocTitleBar.ce.vue";
import HandlePopover from "../HandlePopover.ce.vue";
import chipCss from "../chip.css?inline";
import { vueCard } from "../vueCard";
import type { NodeViewOpts } from "../cardSources";
import { isNodeId } from "../library";
import type { CardRender } from "../types";

export function nodeView(opts?: Partial<NodeViewOpts>): CardRender {
  if (!isNodeId(opts?.node)) {
    throw new Error(
      'nodeView needs the node it shows, by uuid or as "source:<group id>": nodeView({ node: "<uuid>" })',
    );
  }
  return vueCard(
    NodeCard,
    { node: opts.node, name: typeof opts.name === "string" ? opts.name : null },
    { styleSources: [DocCard, ChatBody, ChipMenu, DocTitleBar, HandlePopover, chipCss] },
  );
}
