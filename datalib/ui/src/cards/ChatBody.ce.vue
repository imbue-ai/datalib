<script setup lang="ts">
// Renders a chat conversation. The backend serves the QMD body verbatim
// (CommonMark + per-section `<div id="m-{uuid}" data-section-uuid="…">`
// wrappers emitted by the ingest renderer — one wrapper per message,
// plus nested ones for tool_use / tool_result / thinking blocks);
// `renderDocument` runs markdown-it and the sanitizer once per body.
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
import type { EdgeOut } from "@/api";
import { decorateRemoteMedia, type RemoteContext, type RemoteRef } from "./remoteMedia";
import {
  copyWithHandles,
  decorateHandles,
  type NormalizedContact,
  type Who,
  chipMenu,
  composeUri,
  copyText,
  handleValue,
  searchQueryFor,
  people,
  canLinkHandles,
  NOBODY,
  type ChipMenuEntry,
  type ChipMenuId,
} from "./contacts";
import ChipMenu from "./ChipMenu.ce.vue";
import { entityFromUri } from "./chipLinks";
import {
  browseQuery,
  decorateEntities,
  entities,
  entityCardSource,
  entityCopyText,
  entityMenu,
  type EntityMenuEntry,
  type EntityMenuId,
} from "./entities";
import { personSource } from "./cardSources";
import HandlePopover from "./HandlePopover.ce.vue";
import { copyToClipboard } from "@/clipboard";
import { openExternal } from "@/externalLinks";
import { pushToast } from "@/toasts";
import { renderDocument } from "./renderDocument";
import { isBrowserClick, linkFromClick, type ClickedLink } from "./chatLink";
import { DOC_FRAME_SRCDOC, asElement, forwardAppKeys, mirrorDensity } from "./docFrame";
// Shared with `tools/chat_preview.mjs`, which inlines this same file so
// the preview page behaves like the app rather than imitating it.
import {
  decorateLongMessages,
  injectCopyUuidButtons,
  openEnclosingDetails,
  scrollSectionToTop,
} from "./chatSections.js";

const props = defineProps<{
  body: string;
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
  /**
   * The markdown_uuid of the body we're rendering. Used to rewrite
   * relative image references (`![](blobs/foo.png)`) to backend asset
   * URLs (`/applet/unified_index/asset/{markdownUuid}/blobs/foo.png`) so the browser
   * actually fetches them. Optional: when absent, relative refs pass
   * through unchanged.
   */
  markdownUuid?: string | null;
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
  /** A chip asked for everything from its person: the search to open. */
  (e: "open-search", q: string): void;
  /** A chip asked to open its card — a person's, a group's or a
   *  step's: the card source. */
  (e: "open-card", source: string): void;
}>();

const sanitized = computed(() =>
  renderDocument(props.body || "", props.markdownUuid ?? null, {
    accept: props.remoteAccept,
    context: props.remoteContext,
  }),
);
const html = computed(() => sanitized.value.html);
watch(sanitized, (s) => emit("remote-media", s.remote), { immediate: true });
const frameEl = ref<HTMLIFrameElement | null>(null);
const frameBody = ref<HTMLElement | null>(null);
/// The element the body is in: the frame's `<body>`, or the in-place div.
const body = frameBody;
/// What the frame's body holds now.
let painted: string | null = null;
let frameStops: (() => void)[] = [];

// A re-render that only let an image through must not scroll the
// reader back to the selected section; only a new body earns that.
let bodyChanged = true;
watch(
  () => props.body,
  () => {
    bodyChanged = true;
  },
);

type ChipTarget = {
  handle: string;
  shownAs: string;
  resolved: NormalizedContact | null;
  x: number;
  y: number;
};
const chipTarget = ref<ChipTarget | null>(null);

async function redrawHandles() {
  if (!body.value) return;
  await decorateHandles(body.value);
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

// The same for the group and step chips, answered by `entities`.
const stopEntities = entities.subscribe((uris) => {
  const root = body.value;
  if (!root) return;
  const shown = Array.from(root.querySelectorAll<HTMLElement>("a.chip[data-entity]"));
  if (shown.some((a) => uris.has(a.dataset.entity ?? ""))) void decorateEntities(root);
});
onBeforeUnmount(stopEntities);

/// Where the frame's viewport sits in this window: the popover and the
/// chip menu are drawn out here, over the frame, from points inside it.
function frameOrigin(): { x: number; y: number } {
  const r = frameEl.value?.getBoundingClientRect();
  return { x: r?.left ?? 0, y: r?.top ?? 0 };
}

function chipAt(ev: MouseEvent): HTMLElement | null {
  return asElement(ev.target)?.closest<HTMLElement>("a.chip[data-handle]") ?? null;
}

function whoIs(chip: HTMLElement): Who {
  return people.get(chip.dataset.handle ?? "") ?? NOBODY;
}

/// The link/create popover for `chip`, at a point in this window.
function openPopover(chip: HTMLElement, x: number, y: number) {
  chipTarget.value = {
    handle: chip.dataset.handle ?? "",
    shownAs: chip.dataset.shownAs ?? "",
    resolved: whoIs(chip).mine,
    x,
    y,
  };
}

function entityChipAt(ev: MouseEvent): HTMLElement | null {
  return asElement(ev.target)?.closest<HTMLElement>("a.chip[data-entity]") ?? null;
}

/// A group or step chip's href names something in this app, not a page:
/// no click on it, plain or modified, leaves for the browser or the OS.
function onEntityChipClick(ev: MouseEvent) {
  if (!entityChipAt(ev)) return;
  ev.preventDefault();
  ev.stopPropagation();
}

function openEntityCard(uri: string) {
  const source = entityCardSource(uri);
  if (source) emit("open-card", source);
}

function onHandleChipClick(ev: MouseEvent) {
  const chip = chipAt(ev);
  // A chip is a link; a plain click on it is for the chip, not for the
  // mail client its href would open. A modifier click stays the
  // browser's, as on any link.
  if (!chip || isBrowserClick(ev)) return;
  ev.preventDefault();
  // The second click of a double-click is the double-click's.
  if (ev.detail > 1) return;
  // Without a contacts app there is nothing to change; the tooltip is
  // all a chip has to say.
  if (!canLinkHandles()) return;
  ev.stopPropagation();
  const at = frameOrigin();
  openPopover(chip, at.x + ev.clientX, at.y + ev.clientY);
}

/// Double-click opens the chip's card: a person's, led by this
/// document's source, or a group's or a step's (docs/dev/chips.md § Clicks).
function onChipDblClick(ev: MouseEvent) {
  const entity = entityChipAt(ev);
  if (entity) {
    ev.preventDefault();
    ev.stopPropagation();
    openEntityCard(entity.dataset.entity ?? "");
    return;
  }
  const chip = chipAt(ev);
  if (!chip || isBrowserClick(ev)) return;
  ev.preventDefault();
  ev.stopPropagation();
  openPersonCard(chip.dataset.handle ?? "");
}

function openPersonCard(handle: string) {
  chipTarget.value = null;
  emit("open-card", personSource(handle, { seenIn: props.sourceId ?? null }));
}

type ChipMenuAt = {
  chip: HTMLElement;
  entries: (ChipMenuEntry | EntityMenuEntry)[];
  x: number;
  y: number;
};
const chipMenuAt = ref<ChipMenuAt | null>(null);

/// Right-click on a chip is the chip's menu, not the document's.
function onChipContextMenu(ev: MouseEvent): boolean {
  const entity = entityChipAt(ev);
  if (entity) {
    ev.preventDefault();
    const at = frameOrigin();
    const uri = entity.dataset.entity ?? "";
    chipMenuAt.value = {
      chip: entity,
      entries: entityMenu(uri, entity.dataset.label ?? entity.dataset.shownAs ?? uri),
      x: at.x + ev.clientX,
      y: at.y + ev.clientY,
    };
    return true;
  }
  const chip = chipAt(ev);
  if (!chip) return false;
  ev.preventDefault();
  const at = frameOrigin();
  chipMenuAt.value = {
    chip,
    entries: chipMenu(
      chip.dataset.handle ?? "",
      chip.dataset.shownAs ?? "",
      whoIs(chip),
      canLinkHandles(),
    ),
    x: at.x + ev.clientX,
    y: at.y + ev.clientY,
  };
  return true;
}

async function onChipMenuPick(id: ChipMenuId | EntityMenuId) {
  const menu = chipMenuAt.value;
  chipMenuAt.value = null;
  if (!menu) return;
  const { chip } = menu;
  const handle = chip.dataset.handle ?? "";
  const shownAs = chip.dataset.shownAs ?? "";
  const name = chip.dataset.label ?? shownAs;
  const copy = async (text: string) => {
    if (await copyToClipboard(text)) pushToast(`Copied ${text}`, "info");
    else pushToast("The clipboard refused the copy");
  };
  const uri = chip.dataset.entity;
  if (uri) {
    switch (id as EntityMenuId) {
      case "copy-name":
        await copy(name);
        break;
      case "copy-id":
        await copy(entityFromUri(uri)?.id ?? uri);
        break;
      case "copy-both":
        await copy(entityCopyText(uri, name));
        break;
      case "open":
        openEntityCard(uri);
        break;
      case "browse": {
        const q = browseQuery(uri);
        if (q) emit("open-search", q);
        break;
      }
    }
    return;
  }
  switch (id as ChipMenuId) {
    case "open":
      openPersonCard(handle);
      break;
    case "compose": {
      const mailto = composeUri(handle);
      if (mailto) void openExternal(mailto);
      break;
    }
    case "copy-name":
      await copy(name);
      break;
    case "copy-id":
      await copy(handleValue(handle));
      break;
    case "copy-both":
      await copy(copyText(handle, name));
      break;
    case "search":
      emit("open-search", searchQueryFor(handle));
      break;
    case "edit":
      openPopover(chip, menu.x, menu.y);
      break;
  }
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
  const btn = asElement(ev.target)?.closest<HTMLButtonElement>("button.copy-uuid");
  if (!btn) return;
  ev.preventDefault();
  ev.stopPropagation();
  const uuid = btn.dataset.uuid || "";
  if (!uuid) return;
  try {
    await navigator.clipboard.writeText(uuid);
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
 * Walk the rendered body and decorate every `[data-section-uuid]`
 * whose value matches a span-source edge. We stamp `data-edge-id` on
 * the element and add `.edge-source` so the CSS picks up the subtle
 * background. Click handling lives below via `onBodyEdgeClick`.
 */
function decorateEdgeSources() {
  if (!body.value) return;
  const lookup = edgeBySrcAnchor.value;
  if (lookup.size === 0) return;
  for (const el of body.value.querySelectorAll<HTMLElement>("[data-section-uuid]")) {
    const anchor = el.getAttribute("data-section-uuid") ?? "";
    const edge = lookup.get(anchor);
    if (!edge) continue;
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
  const target = body.value.querySelector<HTMLElement>(
    `[data-section-uuid="${anchor.replace(/"/g, '\\"')}"]`,
  );
  if (target) target.classList.add("hover-dst");
}

function applySelection(scroll = true) {
  if (!body.value) return;
  for (const el of body.value.querySelectorAll(".msg.selected")) {
    el.classList.remove("selected");
  }
  if (!props.selectedSectionUuid) return;
  // Attribute-selector lookup avoids the CSS-id escaping minefield —
  // tool_use block ids look like `tu-toolu_01ABC…`, which is a valid
  // HTML id but not a valid bare CSS selector (the digit-prefixed
  // chunks need escaping). `[data-section-uuid="…"]` keeps the
  // matching pure string equality.
  const target = body.value.querySelector<HTMLElement>(
    `[data-section-uuid="${props.selectedSectionUuid.replace(/"/g, '\\"')}"]`,
  );
  if (!target) return;
  target.classList.add("selected");
  openEnclosingDetails(target);
  // A clamped message shows only its first screenful, so a selection
  // deeper than that would be highlighted where nobody can see it.
  target.closest(".msg--clamped")?.classList.remove("msg--clamped");
  if (scroll) scrollSectionToTop(target);
}

/// Fill the frame and decorate.
function paint() {
  const el = body.value;
  if (!el) return;
  if (painted !== html.value) {
    el.innerHTML = html.value;
    painted = html.value;
  }
  injectCopyUuidButtons(el);
  decorateRemoteMedia(el);
  decorateLongMessages(el);
  void redrawHandles();
  void decorateEntities(el);
  decorateEdgeSources();
  if (bodyChanged) {
    // The desktop app's WebKit could leave a body scrolled before it was
    // first drawn unpainted until the wheel moved it; scroll once it is.
    const win = el.ownerDocument.defaultView;
    win?.requestAnimationFrame(() => win.requestAnimationFrame(() => applySelection(true)));
  } else {
    applySelection(false);
  }
  bodyChanged = false;
  applyHoverDst();
}

function onFrameLinkClick(ev: MouseEvent) {
  if (ev.defaultPrevented) return;
  const link = linkFromClick(ev);
  if (!link) return;
  ev.preventDefault();
  emit("frame-link", link);
}

function releaseFrame() {
  for (const stop of frameStops) stop();
  frameStops = [];
  frameBody.value = null;
  painted = null;
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
  });
  on("auxclick", (ev) => {
    // A right button's auxclick belongs to the context menu, not the link.
    if (ev.button === 2) return;
    onEntityChipClick(ev);
    onFrameLinkClick(ev);
  });
  on("dblclick", onChipDblClick);
  on("mouseover", onBodyMouseOver);
  on("mouseout", onBodyMouseOut);
  on("copy", (ev) => copyWithHandles(ev, doc.body));
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

watch(html, async () => {
  await nextTick();
  paint();
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
  <HandlePopover
    v-if="chipTarget"
    :key="chipTarget.handle"
    v-bind="chipTarget"
    @close="chipTarget = null"
    @changed="redrawHandles"
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
