// `nodeView({ node, name, doc })` in card source: one node of the
// library's tree, as a list of what it holds beside a preview, and the
// document it is when it is one. See
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
import { nodeCardProps } from "../library";
import type { CardRender } from "../types";

export function nodeView(opts?: Partial<NodeViewOpts>): CardRender {
  return vueCard(NodeCard, nodeCardProps(opts), {
    styleSources: [DocCard, ChatBody, ChipMenu, DocTitleBar, HandlePopover, chipCss],
  });
}
