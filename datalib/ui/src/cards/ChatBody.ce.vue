<script setup lang="ts">
// Renders a chat conversation. The backend serves the QMD body verbatim
// (CommonMark + per-section `<div id="m-{uuid}" data-section-uuid="…">`
// wrappers emitted by the ingest renderer — one wrapper per message,
// plus nested ones for tool_use / tool_result / thinking blocks); we
// run markdown-it once.
//
// `selectedSectionUuid` picks which section to scroll to and visually
// highlight via the `.msg.selected` CSS rule. The value must match the
// grid row's `uuid` exactly — for messages that's the message UUID;
// for block rows it's the prefixed form (`tu-…`/`tr-…`/`th-…`) the
// renderer emits.

// NOTE: no side-effect CSS imports here — this component renders
// inside a shadow root, so document-head styles don't reach it. The
// highlight.js stylesheet is injected by documentView's vueCard call
// (imported with `?inline`).
import { ref, computed, watch, nextTick, onMounted } from "vue";
import MarkdownIt from "markdown-it";
import hljs from "highlight.js";
import type { EdgeOut } from "@/api";
import { assetUrl, isAbsoluteOrUrl, rewriteIframeSrcs } from "./asset_urls";
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
   * not specially handled (see `docs/edges.md`).
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
}>();

const emit = defineEmits<{
  (e: "open-edge", edge: EdgeOut): void;
  /**
   * Fired when the cursor enters or leaves an `.edge-source` span.
   * Payload is the edge's destination — `{ md, anchor }` — or null
   * on hover-out. The parent forwards this to MillerView so other
   * columns can highlight whatever the source points at.
   */
  (e: "hover-edge", target: { md: string; anchor: string | null } | null): void;
}>();

function highlight(code: string, lang: string): string {
  if (lang && hljs.getLanguage(lang)) {
    try {
      return hljs.highlight(code, { language: lang }).value;
    } catch {
      /* fall through to escape */
    }
  }
  return code
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

const md = new MarkdownIt({
  html: true,
  linkify: true,
  breaks: false,
  highlight,
});

// Rewrite relative asset references (`blobs/foo.png`, `plots/x.html`) to
// backend asset URLs. Absolute paths (`/...`) and full URLs
// (`http://...`, `data:...`, `//cdn/...`) pass through unchanged. The
// rules live in `./asset_urls` so they are unit-testable on their own.
function envUuid(env: unknown): string | null {
  return (
    (env as { markdownUuid?: string | null } | undefined)?.markdownUuid ?? null
  );
}
const defaultImageRender =
  md.renderer.rules.image ||
  ((tokens, idx, options, _env, self) =>
    self.renderToken(tokens, idx, options));
md.renderer.rules.image = (tokens, idx, options, env, self) => {
  const token = tokens[idx];
  const srcIdx = token.attrIndex("src");
  if (srcIdx >= 0 && token.attrs) {
    // markdown-it 15 types an attribute value as `string | number` (it
    // ships its own types now; @types/markdown-it 14 said `string`).
    // Anything the parser produces for `src` is a string — the number
    // arm is for tokens built programmatically — so narrow rather than
    // coerce, and leave a non-string alone.
    const raw = token.attrs[srcIdx][1];
    const src = typeof raw === "string" ? raw : null;
    const uuid = envUuid(env);
    if (uuid && src && !isAbsoluteOrUrl(src)) {
      token.attrs[srcIdx][1] = assetUrl(uuid, src);
    }
  }
  return defaultImageRender(tokens, idx, options, env, self);
};

// Same rewrite for `<iframe src>`, which arrives as raw HTML rather than
// as a parsed token — see `rewriteIframeSrcs`.
for (const rule of ["html_block", "html_inline"] as const) {
  const fallback = md.renderer.rules[rule];
  md.renderer.rules[rule] = (tokens, idx, options, env, self) => {
    const rendered = fallback
      ? fallback(tokens, idx, options, env, self)
      : tokens[idx].content;
    return rewriteIframeSrcs(rendered, envUuid(env));
  };
}

const html = computed(() =>
  md.render(props.body || "", { markdownUuid: props.markdownUuid ?? null }),
);
const root = ref<HTMLElement | null>(null);

async function onCopyClick(ev: MouseEvent) {
  const btn = (ev.target as HTMLElement | null)?.closest<HTMLButtonElement>(
    "button.copy-uuid",
  );
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
 * edges, we keep only the first — see docs/edges.md, "Limitations".
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
  if (!root.value) return;
  const lookup = edgeBySrcAnchor.value;
  if (lookup.size === 0) return;
  for (const el of root.value.querySelectorAll<HTMLElement>("[data-section-uuid]")) {
    const anchor = el.getAttribute("data-section-uuid") ?? "";
    const edge = lookup.get(anchor);
    if (!edge) continue;
    el.classList.add("edge-source");
    el.dataset.edgeId = edge.edge_uuid;
  }
}

function onBodyEdgeClick(ev: MouseEvent) {
  const t = ev.target;
  if (!(t instanceof Element)) return;
  const el = t.closest<HTMLElement>(".edge-source[data-edge-id]");
  if (!el) return;
  // Honor modifier clicks / non-primary buttons the same way
  // `chat_link.ts` does for inline `<a href="/chat/…">` links: let
  // the browser open the destination in a new tab/window if the user
  // explicitly asked for it.
  if (ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.button !== 0) return;
  const edgeId = el.dataset.edgeId ?? "";
  const edge = (props.outgoingEdges ?? []).find((e) => e.edge_uuid === edgeId);
  if (!edge) return;
  ev.preventDefault();
  ev.stopPropagation();
  emit("open-edge", edge);
}

function onBodyMouseOver(ev: MouseEvent) {
  const t = ev.target;
  if (!(t instanceof Element)) return;
  const el = t.closest<HTMLElement>(".edge-source[data-edge-id]");
  if (!el) return;
  const edge = (props.outgoingEdges ?? []).find(
    (e) => e.edge_uuid === el.dataset.edgeId,
  );
  if (!edge) return;
  emit("hover-edge", {
    md: edge.dst_markdown_uuid,
    anchor: edge.dst_anchor_uuid || null,
  });
}

function onBodyMouseOut(ev: MouseEvent) {
  // mouseout fires both when leaving the span entirely AND when
  // moving between child nodes; relatedTarget tells us which.
  const from = ev.target;
  if (!(from instanceof Element)) return;
  const span = from.closest<HTMLElement>(".edge-source[data-edge-id]");
  if (!span) return;
  const to = ev.relatedTarget;
  if (to instanceof Element && span.contains(to)) return;
  emit("hover-edge", null);
}

/**
 * Mark the hover destination (if any) on the body. Adds `.hover-dst`
 * to the matching `[data-section-uuid="X"]` so CSS can style it as
 * an incoming-edge target. Single-target by design — overlapping
 * spans are out of scope (see docs/edges.md).
 */
function applyHoverDst() {
  if (!root.value) return;
  for (const el of root.value.querySelectorAll(".hover-dst")) {
    el.classList.remove("hover-dst");
  }
  const anchor = props.hoverAnchorUuid;
  if (!anchor) return;
  const target = root.value.querySelector<HTMLElement>(
    `[data-section-uuid="${anchor.replace(/"/g, '\\"')}"]`,
  );
  if (target) target.classList.add("hover-dst");
}

function applySelection() {
  if (!root.value) return;
  for (const el of root.value.querySelectorAll(".msg.selected")) {
    el.classList.remove("selected");
  }
  if (!props.selectedSectionUuid) return;
  // Attribute-selector lookup avoids the CSS-id escaping minefield —
  // tool_use block ids look like `tu-toolu_01ABC…`, which is a valid
  // HTML id but not a valid bare CSS selector (the digit-prefixed
  // chunks need escaping). `[data-section-uuid="…"]` keeps the
  // matching pure string equality.
  const target = root.value.querySelector<HTMLElement>(
    `[data-section-uuid="${props.selectedSectionUuid.replace(/"/g, '\\"')}"]`,
  );
  if (!target) return;
  target.classList.add("selected");
  openEnclosingDetails(target);
  // A clamped message shows only its first screenful, so a selection
  // deeper than that would be highlighted where nobody can see it.
  target.closest(".msg--clamped")?.classList.remove("msg--clamped");
  scrollSectionToTop(target);
}

watch(html, async () => {
  await nextTick();
  if (root.value) {
    injectCopyUuidButtons(root.value);
    decorateLongMessages(root.value);
  }
  decorateEdgeSources();
  applySelection();
  applyHoverDst();
});
watch(
  () => props.selectedSectionUuid,
  async () => {
    // nextTick guards against a parent setting the prop in the same
    // tick that it loads a new conversation: we want the v-html patch
    // to land before we look for `[data-section-uuid]`.
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
onMounted(() => {
  if (root.value) {
    injectCopyUuidButtons(root.value);
    decorateLongMessages(root.value);
  }
  decorateEdgeSources();
  applySelection();
  applyHoverDst();
});
</script>

<template>
  <div
    class="chat-body markdown-body"
    ref="root"
    v-html="html"
    @click="(ev) => { onBodyEdgeClick(ev); onCopyClick(ev); }"
    @mouseover="onBodyMouseOver"
    @mouseout="onBodyMouseOut"
  ></div>
</template>

<style>
/* An unbroken token longer than the pane — a hash, a base64 blob, a
   URL with no slashes to break at — otherwise runs off the edge of its
   card and is simply not readable. `break-word` (not `anywhere`) so
   only the token that would overflow gets broken, and intrinsic widths
   are left alone. */
.chat-body {
  overflow-wrap: break-word;
}
/* A table wider than the pane scrolls itself rather than pushing the
   column out. `width: max-content` keeps it from stretching to fill
   when it is narrow. */
.chat-body table {
  display: block;
  width: max-content;
  max-width: 100%;
  overflow-x: auto;
}

/* Per-message wrappers emitted by ingest. Unscoped on purpose so the rules
   reach inside `v-html`. */
.chat-body .msg {
  scroll-margin-top: 1rem;
  padding: 0.5rem 0.75rem;
  border-left: 3px solid transparent;
  margin: 0.5rem 0;
}
/* Every outermost item — a message, or a whole run of tool steps — is a
   card, so where one ends and the next begins is drawn rather than
   inferred from whitespace. Nested block sections (tool_use, thinking)
   stay flat inside their parent; only direct children of the body get
   the box. Declared BEFORE the per-provider rules below so their
   `border-left-color` still wins. */
.chat-body > .msg,
.chat-body > details.tool-group {
  border: 1px solid var(--datalib-border, #d8d8d8);
  border-left-width: 3px;
  border-radius: 8px;
  background: var(--datalib-card-bg, #fafafa);
  margin: 0.4rem 0;
}
.chat-body .msg--claude {
  border-left-color: var(--datalib-accent, #6366f1);
}
.chat-body .msg--chatgpt {
  border-left-color: #16a34a;
}
.chat-body .msg--slack {
  border-left-color: #4a154b;
}
/* Perseus sections carry polytonic Greek (Greek Extended, U+1F00–
   U+1FFF: precomposed accented vowels). The body otherwise inherits
   `system-ui`, which on macOS resolves to `.AppleSystemUIFont` — and
   that font has NO polytonic glyphs. The browser then falls back
   per-character and decomposes each precomposed vowel into base +
   combining mark, rendering the accent at a default advance (floating
   up and to the right of the letter) instead of over it. Naming fonts
   that actually contain the precomposed glyphs makes the browser use
   their (correctly placed) baked-in forms instead of that broken
   fallback. Every face named here was verified to contain the
   precomposed Greek Extended glyphs (via a CoreText coverage probe);
   `Noto Sans` / `GFS Neohellenic` are sans faces built for polytonic
   Greek, `Helvetica Neue` / `Lucida Grande` are the macOS sans
   fallbacks that also cover the block. We deliberately end in generic
   `sans-serif` (which maps to Helvetica, NOT the polytonic-less
   `.AppleSystemUIFont` that `system-ui` resolves to) so the broken
   fallback can never re-enter. Applies to the English sections too,
   which is harmless. */
.chat-body .msg--perseus {
  font-family: "Noto Sans", "GFS Neohellenic", "Helvetica Neue",
    "Lucida Grande", "Arial Unicode MS", sans-serif;
}
/* Per-block sections (tool_use / tool_result / thinking). Nested
   inside their parent message wrapper, so we keep them visually
   subordinate: thinner left border, lighter accent. The selection
   outline below picks the same accent so the highlight still pops. */
.chat-body .msg--block {
  border-left-width: 2px;
  margin: 0.35rem 0;
  padding: 0.35rem 0.6rem;
}
.chat-body .msg--tool-use {
  border-left-color: #a78bfa;
}
.chat-body .msg--tool-result {
  border-left-color: #c4b5fd;
}
.chat-body .msg--thinking {
  border-left-color: #94a3b8;
}
/* A message too tall to scroll past comfortably shows its first
   screenful and says so. `--clamped` is toggled by the button
   `decorateLongMessages` appends; `--long` stays for as long as the
   message is long, which is what the sticky header keys off. */
.chat-body > .msg.msg--clamped {
  max-height: 22rem;
  overflow: hidden;
  position: relative;
}
.chat-body > .msg.msg--clamped::after {
  content: "";
  position: absolute;
  inset: auto 0 0 0;
  height: 5rem;
  background: linear-gradient(
    to bottom,
    transparent,
    var(--datalib-card-bg, #fafafa)
  );
  pointer-events: none;
}
.chat-body > .msg.msg--clamped.selected::after {
  background: linear-gradient(
    to bottom,
    transparent,
    var(--datalib-hover, #f0f0f0)
  );
}
.chat-body > .msg > button.msg-expand {
  display: block;
  margin: 0.4rem auto 0;
  padding: 0.1rem 0.6rem;
  font: inherit;
  font-size: 0.75rem;
  color: var(--datalib-muted, #94a3b8);
  background: var(--datalib-input-bg, #fff);
  border: 1px solid var(--datalib-border, #d8d8d8);
  border-radius: 999px;
  cursor: pointer;
}
.chat-body > .msg > button.msg-expand:hover {
  color: inherit;
}
.chat-body > .msg.msg--clamped > button.msg-expand {
  position: absolute;
  left: 50%;
  bottom: 0.4rem;
  transform: translateX(-50%);
  z-index: 2;
  margin: 0;
}
/* While you are inside a long message its header stays put, so the
   author, the time and the jump controls are never something you have
   to scroll back up for. Only long messages: pinning every header
   would leave a stack of them on screen. The negative margins let the
   pinned bar cover the card's full width rather than letting content
   slide through the padding beside it. */
.chat-body > .msg.msg--long > h2:has(> .msg-author) {
  position: sticky;
  top: 0;
  z-index: 1;
  background: var(--datalib-card-bg, #fafafa);
  margin: -0.5rem -0.75rem 0.15rem;
  padding: 0.3rem 0.75rem 0.25rem;
}
.chat-body > .msg.msg--long.selected > h2:has(> .msg-author) {
  background: var(--datalib-hover, #f0f0f0);
}
/* Only while it is actually pinned: an edge and a shadow, so the line
   of text passing beneath the bar reads as passing beneath it rather
   than as having gone missing. `.is-stuck` is set by the observer in
   `chatSections.js`. */
.chat-body > .msg.msg--long > h2.is-stuck {
  border-bottom: 1px solid var(--datalib-border, #d8d8d8);
  box-shadow: 0 4px 6px -4px rgba(0, 0, 0, 0.35);
}
.chat-body .msg-nav {
  margin-left: auto;
  display: inline-flex;
  gap: 0.15rem;
}
.chat-body button.msg-jump {
  font: inherit;
  font-size: 0.7rem;
  line-height: 1;
  padding: 0.15rem 0.35rem;
  color: var(--datalib-muted, #94a3b8);
  background: transparent;
  border: 1px solid var(--datalib-border, #d8d8d8);
  border-radius: 4px;
  cursor: pointer;
}
.chat-body button.msg-jump:hover {
  color: inherit;
  background: var(--datalib-hover, #f0f0f0);
}
.chat-body .msg.selected {
  background: var(--datalib-hover, #f0f0f0);
  border-color: var(--datalib-accent, #6366f1);
  /* `outline` (not a thicker border) so moving the selection doesn't
     reflow; the negative offset lays it over the card's own edge so
     the two read as one highlighted border rather than two rings. */
  outline: 2px solid var(--datalib-accent, #6366f1);
  outline-offset: -1px;
}
/* Inline span baked by ingest for sub-section edge anchors (today
   only: perseus first-word wrappers). When the span happens to also
   be the source of an outgoing edge, `.edge-source` is added by
   ChatBody at mount time and the user gets a link-y dotted
   underline (in the muted color so it doesn't read as "external
   link blue"); the hover fill calls out the target and the
   `.hover-dst` class on the matching destination anchor mirrors the
   same fill across whichever column the destination lives in.

   `.selected` on the same span is how we highlight the destination
   side after navigating via an edge (click-driven, persistent),
   distinct from `.hover-dst` (hover-driven, transient).

   We deliberately scope these to `span[data-section-uuid]` so the
   block-level `.msg.selected` styling above (which adds a 2px
   outline + 4px left border) doesn't accidentally fire for inline
   word-wrappers. */
.chat-body span[data-section-uuid].edge-source {
  text-decoration: underline;
  text-decoration-style: dotted;
  text-decoration-color: var(--datalib-muted, #94a3b8);
  text-underline-offset: 2px;
  cursor: pointer;
  transition: background-color 100ms ease-in-out;
}
.chat-body span[data-section-uuid].edge-source:hover,
.chat-body [data-section-uuid].hover-dst {
  background: rgba(99, 102, 241, 0.28);
  border-radius: 3px;
}
.chat-body span[data-section-uuid].selected {
  background: var(--datalib-card-bg, #1f2937);
  outline: 2px solid var(--datalib-accent, #6366f1);
  border-radius: 3px;
}
/* Attachment images arrive at their original resolution; without a
   cap a phone photo renders thousands of pixels wide inside the pane.
   Fit the column width and keep tall images from swallowing the whole
   scrollport; `width/height: auto` preserves the aspect ratio under
   whichever constraint bites. */
.chat-body img {
  max-width: 100%;
  max-height: 60vh;
  width: auto;
  height: auto;
}
.chat-body .msg-meta {
  color: var(--datalib-muted, #94a3b8);
  font-size: 0.85rem;
  margin: 0 0 0.5rem;
}
.chat-body .msg-meta a {
  color: inherit;
  text-decoration: underline;
}
/* The chat-common message header. It is a real `## ` heading because
   qmd cuts its chunks at the best nearby break point and scores an h2
   far above a blank line — so the heading is what makes every message
   start a preferred chunk boundary. On screen it should read as
   Slack's one-line "Name  time", not as a document section, hence the
   reset below. `:has()` keeps it off an h2 that came from the message
   *body* (someone pasting markdown with their own headings). */
.chat-body .msg h2:has(> .msg-author) {
  font-size: 1em;
  font-weight: 400;
  line-height: 1.3;
  margin: 0 0 0.15rem;
  padding: 0;
  border: none;
  display: flex;
  align-items: baseline;
  flex-wrap: wrap;
  gap: 0.4rem;
}
.chat-body .msg-author {
  font-weight: 600;
}
.chat-body .msg-ts {
  font-size: 0.75rem;
  font-weight: 400;
  color: var(--datalib-muted, #94a3b8);
  /* The full instant is in `title`; say so with the cursor. */
  cursor: help;
}
/* A run of adjacent tool steps, folded into one line until asked for. */
.chat-body details.tool-group {
  margin: 0.35rem 0;
}
.chat-body details.tool-group > summary {
  cursor: pointer;
  font-size: 0.8rem;
  color: var(--datalib-muted, #94a3b8);
  list-style-position: outside;
}
.chat-body details.tool-group > summary {
  padding: 0.35rem 0.6rem;
}
.chat-body details.tool-group[open] > summary {
  border-bottom: 1px solid var(--datalib-border, #d8d8d8);
}
/* Inside a group every step has the same author and near-identical
   time, so the header is there for its anchor and its copy button, not
   to be read. */
.chat-body details.tool-group .msg h2:has(> .msg-author) {
  font-size: 0.8em;
  opacity: 0.7;
}
.chat-body button.copy-uuid {
  /* The 🆔 glyph is a saturated purple that outshouts the message it
     sits next to; grey it out so it reads as a control rather than as
     content. The `copied` / `copy-failed` states below re-colour on
     purpose, so they drop the filter. */
  filter: grayscale(1);
  display: inline-flex;
  align-items: center;
  justify-content: center;
  vertical-align: baseline;
  padding: 0;
  margin: 0;
  background: transparent;
  border: none;
  color: inherit;
  font: inherit;
  /* The glyph is a control, not text: smaller than what it sits beside. */
  font-size: 0.75em;
  line-height: 1;
  cursor: pointer;
  opacity: 0.55;
}
/* Opacity only. A hover border would draw a box a different shape from
   the glyph inside it, which reads as the control changing rather than
   as it lighting up. */
.chat-body button.copy-uuid:hover {
  opacity: 1;
}
.chat-body button.copy-uuid.copied {
  color: #16a34a;
  opacity: 1;
  filter: none;
}
.chat-body button.copy-uuid.copy-failed {
  color: #dc2626;
  opacity: 1;
  filter: none;
}
</style>
