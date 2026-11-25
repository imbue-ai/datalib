<script setup lang="ts">
// Renders a chat conversation. The backend serves the markdown as the
// renderer's pieces (CommonMark + per-section `<div id="m-{uuid}"
// data-section-uuid="…">` wrappers — one wrapper per message, plus nested
// ones for tool_use / tool_result / thinking blocks); `renderDocument`
// runs markdown-it and the sanitizer over it, one message at a time.
//
// The body is drawn inside a frame whose policy runs no script
// (`docFrame.ts`). `body` is the frame's `<body>`: the decorations below
// work on it, and its events are wired to the handlers here.
//
// `selectedSectionUuid` picks which section to scroll to and visually
// highlight via the `.msg.selected` CSS rule. The value must match the
// grid row's `uuid` exactly — for messages that's the message UUID;
// for block rows it's the prefixed form (`tu-…`/`tr-…`/`th-…`) the
// renderer emits.

// NOTE: no side-effect CSS imports here — this component renders
// inside a shadow root, so document-head styles don't reach it. The
// body's stylesheet (`documentBody.css`) goes in with the frame's document.
import { ref, computed, watch, nextTick, onMounted, onBeforeUnmount } from "vue";
import type { BodyNode, EdgeOut } from "@/api";
import { decorateRemoteMedia, type RemoteContext, type RemoteRef } from "./remoteMedia";
import {
  copyWithHandles,
  decorateHandles,
  type NormalizedContact,
  type Who,
  people,
  NOBODY,
} from "./contacts";
import { contactPages, decorateContactPages } from "./contactPages";
import ChipMenu from "./ChipMenu.ce.vue";
import {
  chipDragQuery,
  chipElAt,
  chipMenuOf,
  chipOf,
  runChipAction,
  type Chip,
  type ChipEntryId,
  type ChipEntry,
  type ChipHost,
} from "./chipActions";
import { decorateChatDays, decorateDates, rewriteDatesForCopy } from "./dates";
import { decorateLabels } from "./labelChip";
import { timeView } from "@/config/timeView";
import { decorateEntities, entities } from "./entities";
import { renderDocument } from "./renderDocument";
import { NEAR_END_PX, growth } from "./pieceWindow";
import { mountTimeSeries } from "./timeSeriesEmbed";
import {
  finishFrameClick,
  isBrowserClick,
  linkFromClick,
  linkOfButton,
  rewriteLinkButtonsForCopy,
  type ClickedLink,
} from "./chatLink";
import { DOC_FRAME_SRCDOC, asElement, forwardAppKeys, mirrorDensity } from "./docFrame";
// Shared with `tools/chat_preview.mjs`, which inlines this same file so
// the preview page behaves like the app rather than imitating it.
import {
  decorateLongMessages,
  injectCopyLinkButtons,
  openEnclosingDetails,
  scrollSectionToTop,
  sectionEl,
} from "./chatSections.js";

const props = defineProps<{
  /** The body as the renderer's pieces (`renderDocument`). */
  pieces: BodyNode[];
  selectedSectionUuid?: string | null;
  /**
   * Outgoing edges for the doc we're rendering. ChatBody decorates
   * every `[data-section-uuid]` whose value appears as the
   * `src_anchor_uuid` of an edge with `class="edge-source"` and
   * `data-edge-id="…"` so the user-visible styling + click handler
   * pick it up. Limitations: only the FIRST edge per source anchor
   * is used; spans whose source anchors overlap inside the body are
   * not specially handled (see `docs/dev/edges.md`).
   */
  outgoingEdges?: EdgeOut[];
  /**
   * Anchor uuid to highlight as an *incoming* edge destination. Set
   * by the parent column when the user hovers an edge-source in
   * *another* column whose destination lives inside this doc.
   * Distinct from `selectedSectionUuid` (the persistent click-driven
   * highlight): hover-driven, transient, and styled the same color
   * as the originating span's hover background so the link between
   * source and destination is visually obvious across columns.
   */
  hoverAnchorUuid?: string | null;
  /** The markdown_uuid of the body we're rendering: the page a 🔗
   *  button's link names. */
  markdownUuid?: string | null;
  /** The document's title, written into a 🔗 link for a person to read. */
  docTitle?: string | null;
  /**
   * Which of the body's remote references may load, through the
   * server: the allow-list applied (`remoteMedia.ts`). Absent: none.
   * A new function re-renders the body under the new answer.
   */
  remoteAccept?: (url: string) => boolean;
  /** What this body is, for the server's answer (`remoteMedia.ts`). */
  remoteContext?: RemoteContext;
  /** The source this document came from: a person card opened from one
   *  of its chips leads with that source's record. */
  sourceId?: string | null;
}>();

const emit = defineEmits<{
  (e: "open-edge", edge: EdgeOut): void;
  /** A body was written into the frame: the first, or a changed one. */
  (e: "painted"): void;
  /** The reader is near the top or the bottom of what is drawn: the
   *  page there, if the document has one, is wanted (`pieceWindow.ts`). */
  (e: "need-more", where: "above" | "below"): void;
  /** Every remote reference the body carries, after each render;
   *  those held back are shown as placeholders here. */
  (e: "remote-media", refs: RemoteRef[]): void;
  /** A placeholder was clicked: the person wants this one loaded. */
  (e: "remote-load", url: string): void;
  /**
   * Fired when the cursor enters or leaves an `.edge-source` span.
   * Payload is the edge's destination — `{ md, anchor }` — or null
   * on hover-out. The parent publishes it on the card bus so other
   * cards can highlight whatever the source points at.
   */
  (e: "hover-edge", target: { md: string; anchor: string | null } | null): void;
  /** A link in a framed body was clicked: the frame never navigates
   *  itself, so the card decides where it goes. */
  (e: "frame-link", link: ClickedLink): void;
  /** A right-click in a framed body, with the frame's window (whose
   *  selection it is) and where the frame sits in this one. */
  (e: "frame-contextmenu", ev: MouseEvent, view: { win: Window; dx: number; dy: number }): void;
  /** A chip asked for everything with its person: the search to open. */
  (e: "open-search", q: string): void;
  /** A chip asked to open its card — a person's, a group's or a
   *  step's: the card source. */
  (e: "open-card", source: string): void;
}>();

const frameEl = ref<HTMLIFrameElement | null>(null);
const frameBody = ref<HTMLElement | null>(null);
/// The element the body is in: the frame's `<body>`.
const body = frameBody;
/// What the frame's body holds now: the nodes drawn, under which answer
/// about remote media, and the remote references they carry.
let drawnNodes: readonly BodyNode[] = [];
let drawnAccept: RemoteAccept | undefined;
let drawnRemote: RemoteRef[] = [];
let frameStops: (() => void)[] = [];

type RemoteAccept = ((url: string) => boolean) | undefined;

function render(nodes: readonly BodyNode[]) {
  return renderDocument(nodes, { accept: props.remoteAccept, context: props.remoteContext });
}

// A re-render that only let an image through must not scroll the
// reader back to the selected section; only a new body earns that.
let bodyChanged = true;
watch(
  () => props.pieces,
  () => {
    bodyChanged = true;
  },
);

async function redrawHandles() {
  if (!body.value) return;
  await decorateHandles(body.value);
}

async function redrawContactPages() {
  if (body.value) await decorateContactPages(body.value);
}

// An answer changed somewhere — a link made in another document, or in
// this one — so draw again if any chip here shows one of those handles.
const stopPeople = people.subscribe((handles) => {
  const root = body.value;
  if (!root) return;
  const shown = Array.from(root.querySelectorAll<HTMLElement>("a.chip[data-handle]"));
  if (shown.some((a) => handles.has(a.dataset.handle ?? ""))) void redrawHandles();
});
onBeforeUnmount(stopPeople);

const stopPages = contactPages.subscribe(() => void redrawContactPages());
onBeforeUnmount(stopPages);

// The same for the group and step chips, answered by `entities`.
const stopEntities = entities.subscribe((uris) => {
  const root = body.value;
  if (!root) return;
  const shown = Array.from(root.querySelectorAll<HTMLElement>("a.chip[data-entity]"));
  if (shown.some((a) => uris.has(a.dataset.entity ?? ""))) void decorateEntities(root);
});
onBeforeUnmount(stopEntities);

/// Where the frame's viewport sits in this window: the chip menu
/// is drawn out here, over the frame, from a point inside it.
function frameOrigin(): { x: number; y: number } {
  const r = frameEl.value?.getBoundingClientRect();
  return { x: r?.left ?? 0, y: r?.top ?? 0 };
}

function chipAt(ev: MouseEvent): HTMLElement | null {
  return asElement(ev.target)?.closest<HTMLElement>("a.chip[data-handle]") ?? null;
}

function entityChipAt(ev: MouseEvent): HTMLElement | null {
  return (
    asElement(ev.target)?.closest<HTMLElement>("a.chip[data-entity], a.chip[data-contact-page]") ??
    null
  );
}

/// A `datalib:` chip's href names something in this app, not a page: no
/// click on it, plain or modified, leaves for the browser or the OS.
function onEntityChipClick(ev: MouseEvent) {
  if (!entityChipAt(ev)) return;
  ev.preventDefault();
  ev.stopPropagation();
}

/// A click on a person does nothing, and above all does not open the mail
/// client its href names. A modifier click stays the browser's, as on any
/// link.
function onHandleChipClick(ev: MouseEvent) {
  if (chipAt(ev) && !isBrowserClick(ev)) ev.preventDefault();
}

/// Double-click opens the chip's card: a person's, led by this
/// document's source, or a group's or a step's (docs/dev/chips.md § Clicks).
function onChipDblClick(ev: MouseEvent) {
  const el = chipElAt(asElement(ev.target));
  const chip = el ? chipOf(el) : null;
  if (!el || !chip || (chip.ref.kind === "person" && isBrowserClick(ev))) return;
  ev.preventDefault();
  ev.stopPropagation();
  void runChipAction("open", chip, chipHost);
}

/// What a chip's entries do in a document: open a card or a search
/// beside it.
const chipHost: ChipHost = {
  openCard: (source) => emit("open-card", source),
  search: (q) => emit("open-search", q),
  get seenIn() {
    return props.sourceId ?? null;
  },
};

/// A chip dragged into a search bar drops the search it stands for, as
/// text the field takes like any typed (docs/dev/chips.md § "A date"),
/// spaced so it never runs into a word already there.
function onChipDragStart(ev: DragEvent) {
  const el = chipElAt(asElement(ev.target));
  const chip = el ? chipOf(el) : null;
  const q = chip ? chipDragQuery(chip.ref) : null;
  if (!q || !ev.dataTransfer) return;
  ev.dataTransfer.setData("text/plain", ` ${q} `);
}

type ChipMenuAt = { chip: Chip; entries: ChipEntry[]; x: number; y: number };
const chipMenuAt = ref<ChipMenuAt | null>(null);

/// Right-click on a chip is the chip's menu, not the document's.
function onChipContextMenu(ev: MouseEvent): boolean {
  const el = chipElAt(asElement(ev.target));
  const chip = el ? chipOf(el) : null;
  if (!el || !chip) return false;
  ev.preventDefault();
  const at = frameOrigin();
  chipMenuAt.value = {
    chip,
    entries: chipMenuOf(chip),
    x: at.x + ev.clientX,
    y: at.y + ev.clientY,
  };
  return true;
}

async function onChipMenuPick(id: string) {
  const menu = chipMenuAt.value;
  chipMenuAt.value = null;
  if (menu) await runChipAction(id as ChipEntryId, menu.chip, chipHost);
}

function onRemoteChipClick(ev: MouseEvent) {
  const chip = asElement(ev.target)?.closest<HTMLButtonElement>("button.remote-media");
  if (!chip) return;
  ev.preventDefault();
  ev.stopPropagation();
  const url = chip.dataset.remoteUrl;
  if (url) emit("remote-load", url);
}

async function onCopyClick(ev: MouseEvent) {
  const btn = asElement(ev.target)?.closest<HTMLButtonElement>("button.copy-link");
  if (!btn) return;
  ev.preventDefault();
  ev.stopPropagation();
  const markdownUuid = props.markdownUuid;
  if (!btn.dataset.uuid || !markdownUuid) return;
  try {
    await navigator.clipboard.writeText(linkOfButton(btn, markdownUuid, props.docTitle ?? ""));
    const prev = btn.textContent;
    btn.textContent = "✓";
    btn.classList.add("copied");
    setTimeout(() => {
      btn.textContent = prev;
      btn.classList.remove("copied");
    }, 900);
  } catch {
    btn.classList.add("copy-failed");
    setTimeout(() => btn.classList.remove("copy-failed"), 900);
  }
}

/**
 * Build a (src_anchor_uuid → first matching EdgeOut) lookup over the
 * outgoing edges that have a span-level source (`src_anchor_uuid !==
 * null`). When the renderer baked the same anchor uuid into multiple
 * edges, we keep only the first — see docs/dev/edges.md, "Limitations".
 */
const edgeBySrcAnchor = computed<Map<string, EdgeOut>>(() => {
  const m = new Map<string, EdgeOut>();
  for (const e of props.outgoingEdges ?? []) {
    if (!e.src_anchor_uuid) continue;
    if (!m.has(e.src_anchor_uuid)) m.set(e.src_anchor_uuid, e);
  }
  return m;
});

/**
 * Decorate the element each span-source edge starts at: stamp
 * `data-edge-id` on it and add `.edge-source` so the CSS picks up the
 * subtle background. Click handling lives below via `onBodyEdgeClick`.
 */
function decorateEdgeSources() {
  const root = body.value;
  if (!root) return;
  for (const [anchor, edge] of edgeBySrcAnchor.value) {
    const el = sectionEl(root, anchor);
    if (!el) continue;
    el.classList.add("edge-source");
    el.dataset.edgeId = edge.edge_uuid;
  }
}

function onBodyEdgeClick(ev: MouseEvent) {
  const el = asElement(ev.target)?.closest<HTMLElement>(".edge-source[data-edge-id]");
  if (!el) return;
  // Honor modifier clicks / non-primary buttons the same way
  // `chat_link.ts` does for inline `<a href="/chat/…">` links: let
  // the browser open the destination in a new tab/window if the user
  // explicitly asked for it.
  if (isBrowserClick(ev)) return;
  const edgeId = el.dataset.edgeId ?? "";
  const edge = (props.outgoingEdges ?? []).find((e) => e.edge_uuid === edgeId);
  if (!edge) return;
  ev.preventDefault();
  ev.stopPropagation();
  emit("open-edge", edge);
}

function onBodyMouseOver(ev: MouseEvent) {
  const el = asElement(ev.target)?.closest<HTMLElement>(".edge-source[data-edge-id]");
  if (!el) return;
  const edge = (props.outgoingEdges ?? []).find((e) => e.edge_uuid === el.dataset.edgeId);
  if (!edge) return;
  emit("hover-edge", {
    md: edge.dst_markdown_uuid,
    anchor: edge.dst_anchor_uuid || null,
  });
}

function onBodyMouseOut(ev: MouseEvent) {
  // mouseout fires both when leaving the span entirely AND when
  // moving between child nodes; relatedTarget tells us which.
  const span = asElement(ev.target)?.closest<HTMLElement>(".edge-source[data-edge-id]");
  if (!span) return;
  const to = asElement(ev.relatedTarget);
  if (to && span.contains(to)) return;
  emit("hover-edge", null);
}

/**
 * Mark the hover destination (if any) on the body. Adds `.hover-dst`
 * to the matching `[data-section-uuid="X"]` so CSS can style it as
 * an incoming-edge target. Single-target by design — overlapping
 * spans are out of scope (see docs/dev/edges.md).
 */
function applyHoverDst() {
  if (!body.value) return;
  for (const el of body.value.querySelectorAll(".hover-dst")) {
    el.classList.remove("hover-dst");
  }
  const anchor = props.hoverAnchorUuid;
  if (!anchor) return;
  sectionEl(body.value, anchor)?.classList.add("hover-dst");
}

function applySelection(scroll = true) {
  if (!body.value) return;
  for (const el of body.value.querySelectorAll(".msg.selected")) {
    el.classList.remove("selected");
  }
  if (!props.selectedSectionUuid) return;
  const target = sectionEl(body.value, props.selectedSectionUuid);
  if (!target) return;
  target.classList.add("selected");
  openEnclosingDetails(target);
  // A clamped message shows only its first screenful, so a selection
  // deeper than that would be highlighted where nobody can see it.
  target.closest(".msg--clamped")?.classList.remove("msg--clamped");
  if (scroll) scrollSectionToTop(target);
}

/// Fill the frame and decorate.
/// What unmounts the charts mounted into the body as it stands.
let chartStops: (() => void)[] = [];
function unmountCharts() {
  for (const stop of chartStops) stop();
  chartStops = [];
}

/// The first element on screen and where it sits, so drawing a page above
/// it can leave it where it was.
function topOnScreen(el: HTMLElement): { node: Element; top: number } | null {
  for (const child of Array.from(el.children)) {
    const r = child.getBoundingClientRect();
    if (r.bottom > 0) return { node: child, top: r.top };
  }
  return null;
}

/// Draw `props.pieces`: beside what is drawn when the window only grew at
/// an end, so an open fold or an expanded message stays as it is and the
/// reader does not move; anew otherwise.
function draw(el: HTMLElement): "new" | "grew" | "same" {
  const g =
    drawnAccept === props.remoteAccept
      ? growth(drawnNodes, props.pieces)
      : { kind: "new" as const };
  if (g.kind === "new") {
    unmountCharts();
    const s = render(props.pieces);
    el.innerHTML = s.html;
    drawnRemote = s.remote;
  } else if (g.kind === "grew") {
    const above = render(g.above);
    const below = render(g.below);
    el.insertAdjacentHTML("beforeend", below.html);
    el.insertAdjacentHTML("afterbegin", above.html);
    drawnRemote = [...above.remote, ...drawnRemote, ...below.remote];
  }
  drawnNodes = props.pieces;
  drawnAccept = props.remoteAccept;
  if (g.kind !== "same") emit("remote-media", drawnRemote);
  return g.kind;
}

/// Ask for the page above or below when the reader is near that end of
/// what is drawn, or what is drawn does not fill the frame.
function askNearEnds() {
  const scroller = body.value?.ownerDocument.scrollingElement;
  if (!scroller) return;
  if (scroller.scrollTop < NEAR_END_PX) emit("need-more", "above");
  if (scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < NEAR_END_PX) {
    emit("need-more", "below");
  }
}

function paint() {
  const el = body.value;
  if (!el) return;
  const anchor = topOnScreen(el);
  const kind = draw(el);
  if (kind === "new") emit("painted");
  chartStops.push(mountTimeSeries(el));
  // The document card's title bar has the page's own ID button.
  injectCopyLinkButtons(el, { pageTitle: false });
  decorateRemoteMedia(el);
  decorateLongMessages(el);
  void redrawHandles();
  void redrawContactPages();
  void decorateEntities(el);
  decorateDates(el);
  decorateChatDays(el);
  decorateLabels(el);
  decorateEdgeSources();
  if (kind === "grew" && anchor) {
    // A page drawn above pushed what the reader was looking at down; put
    // it back where it was, decorations and all.
    const scroller = el.ownerDocument.scrollingElement;
    if (scroller) scroller.scrollTop += anchor.node.getBoundingClientRect().top - anchor.top;
  }
  if (bodyChanged && kind === "new") {
    // The desktop app's WebKit could leave a body scrolled before it was
    // first drawn unpainted until the wheel moved it; scroll once it is.
    const win = el.ownerDocument.defaultView;
    win?.requestAnimationFrame(() => win.requestAnimationFrame(() => applySelection(true)));
  } else {
    applySelection(false);
  }
  bodyChanged = false;
  applyHoverDst();
  askNearEnds();
}

function onFrameLinkClick(ev: MouseEvent) {
  if (ev.defaultPrevented) return;
  const link = linkFromClick(ev);
  if (!link) return;
  ev.preventDefault();
  emit("frame-link", link);
}

function releaseFrame() {
  unmountCharts();
  for (const stop of frameStops) stop();
  frameStops = [];
  frameBody.value = null;
  drawnNodes = [];
}

/// Also on every reload: a frame moved in the DOM loads its document again.
function onFrameLoad() {
  releaseFrame();
  const frame = frameEl.value;
  const doc = frame?.contentDocument;
  const win = doc?.defaultView;
  if (!frame || !doc?.body || !win) return;
  const on = <K extends keyof DocumentEventMap>(type: K, fn: (ev: DocumentEventMap[K]) => void) => {
    doc.addEventListener(type, fn);
    frameStops.push(() => doc.removeEventListener(type, fn));
  };
  on("click", (ev) => {
    onEntityChipClick(ev);
    onHandleChipClick(ev);
    onBodyEdgeClick(ev);
    onCopyClick(ev);
    onRemoteChipClick(ev);
    onFrameLinkClick(ev);
    finishFrameClick(ev);
  });
  on("auxclick", (ev) => {
    // A right button's auxclick belongs to the context menu, not the link.
    if (ev.button === 2) return;
    onEntityChipClick(ev);
    onFrameLinkClick(ev);
    finishFrameClick(ev);
  });
  on("dblclick", onChipDblClick);
  on("dragstart", onChipDragStart);
  on("mouseover", onBodyMouseOver);
  on("mouseout", onBodyMouseOut);
  on("scroll", askNearEnds);
  on("copy", (ev) => {
    const markdownUuid = props.markdownUuid;
    const title = props.docTitle ?? "";
    copyWithHandles(ev, doc.body, (f) => {
      const dates = rewriteDatesForCopy(f);
      const links = markdownUuid ? rewriteLinkButtonsForCopy(f, markdownUuid, title) : false;
      return dates || links;
    });
  });
  on("contextmenu", (ev) => {
    if (onChipContextMenu(ev)) return;
    const r = frame.getBoundingClientRect();
    emit("frame-contextmenu", ev, { win, dx: r.left, dy: r.top });
  });
  frameStops.push(mirrorDensity(doc), forwardAppKeys(doc));
  doc.documentElement.dataset.markdownUuid = props.markdownUuid ?? "";
  frameBody.value = doc.body;
  bodyChanged = true;
  paint();
}
onBeforeUnmount(releaseFrame);
watch(
  () => props.markdownUuid,
  (uuid) => {
    const doc = frameBody.value?.ownerDocument;
    if (doc) doc.documentElement.dataset.markdownUuid = uuid ?? "";
  },
);

watch([() => props.pieces, () => props.remoteAccept], async () => {
  await nextTick();
  paint();
});
// The clock's zone or format changed: the same dates, read anew.
watch(timeView, () => {
  if (!body.value) return;
  decorateDates(body.value);
  decorateChatDays(body.value);
});
watch(
  () => props.selectedSectionUuid,
  async () => {
    // nextTick guards against a parent setting the prop in the same
    // tick that it loads a new conversation: the new body must be painted
    // before we look for `[data-section-uuid]`.
    await nextTick();
    applySelection();
  },
);
watch(
  () => props.outgoingEdges,
  async () => {
    await nextTick();
    decorateEdgeSources();
  },
);
watch(
  () => props.hoverAnchorUuid,
  async () => {
    await nextTick();
    applyHoverDst();
  },
);
onMounted(paint);
</script>

<template>
  <iframe
    ref="frameEl"
    class="doc-frame"
    title="Document"
    :srcdoc="DOC_FRAME_SRCDOC"
    @load="onFrameLoad"
  ></iframe>
  <ChipMenu
    v-if="chipMenuAt"
    :entries="chipMenuAt.entries"
    :x="chipMenuAt.x"
    :y="chipMenuAt.y"
    @pick="onChipMenuPick"
    @close="chipMenuAt = null"
  />
</template>

<style>
.doc-frame {
  display: block;
  width: 100%;
  height: 100%;
  border: 0;
}
</style>
