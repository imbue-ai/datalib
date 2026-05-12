// Producer-side helpers for `FeedbackContext` — the shape the HTTP layer
// stores in `feedback.context_json` (see schemas/feedback.schema.json).
//
// Two responsibilities:
//   1. Walk the DOM from a right-clicked element up to the nearest
//      `[data-feedback-root]` (or `<body>`) so a future maintainer can
//      reconstruct what the user was pointing at even if the DOM
//      structure shifts.
//   2. Build the typed per-surface `payload` for each call site — the
//      surface discriminator + payload shape mirror the
//      x-tagged-union in the schema.
//
// Read-back is out of scope for v0 — these types describe what we
// *write*. The codegen'd FeedbackContext type lives at
// //schemas:feedback_ts; once we wire it into the Vite build, this file
// will import that union instead of redeclaring it. For now we keep a
// shape-compatible hand-rolled mirror so the UI compiles standalone.

export type FeedbackSurface =
  | "grid_cell"
  | "grid_row"
  | "preview_message"
  | "preview_selection"
  | "page_header"
  | "filter_chip"
  | "column_header";

export interface DomPathStep {
  tag: string;
  id?: string | null;
  classes?: string[] | null;
  data?: Record<string, string> | null;
}

export interface FeedbackSurfaceGridCell {
  column: string;
  row_uuids: string[];
  cell_value?: string | null;
}
export interface FeedbackSurfaceGridRow {
  row_uuids: string[];
}
export interface FeedbackSurfacePreviewMessage {
  conversation_uuid: string;
  message_uuid: string;
  message_index: number;
}
export interface FeedbackSurfacePreviewSelection {
  conversation_uuid: string;
  start_message_uuid: string;
  end_message_uuid: string;
  selected_text: string;
}
export interface FeedbackSurfacePageHeader {
  entity_kind: "conversation";
  entity_uuid: string;
}
export interface FeedbackSurfaceFilterChip {
  key: string;
  value: string;
}
export interface FeedbackSurfaceColumnHeader {
  key: string;
}

export type FeedbackPayload =
  | FeedbackSurfaceGridCell
  | FeedbackSurfaceGridRow
  | FeedbackSurfacePreviewMessage
  | FeedbackSurfacePreviewSelection
  | FeedbackSurfacePageHeader
  | FeedbackSurfaceFilterChip
  | FeedbackSurfaceColumnHeader;

export interface FeedbackContext {
  url: string;
  surface: FeedbackSurface;
  dom_path_breadcrumb: DomPathStep[];
  dom_path_selector: string;
  target_uuids: string[];
  payload: FeedbackPayload;
}

const MAX_BREADCRUMB_DEPTH = 25;

/** Walk from `el` upwards to the nearest `[data-feedback-root]` (or
 *  `<body>`), recording one DomPathStep per ancestor. Hard-capped at
 *  MAX_BREADCRUMB_DEPTH so a runaway DOM can't blow up the payload. */
export function buildDomPath(el: Element | null): DomPathStep[] {
  const out: DomPathStep[] = [];
  let cur: Element | null = el;
  let depth = 0;
  while (cur && depth < MAX_BREADCRUMB_DEPTH) {
    out.push(stepFor(cur));
    if (cur.hasAttribute("data-feedback-root") || cur.tagName.toLowerCase() === "body") {
      break;
    }
    cur = cur.parentElement;
    depth++;
  }
  return out;
}

function stepFor(el: Element): DomPathStep {
  const tag = el.tagName.toLowerCase();
  const id = el.id || null;
  const classList = el.classList.length > 0 ? Array.from(el.classList) : null;
  const dataMap: Record<string, string> = {};
  // `data-*` only — record what's reconstructable. We do not capture
  // attributes like style/aria-* that don't help identify the surface.
  for (const attr of Array.from(el.attributes)) {
    if (attr.name.startsWith("data-") && attr.value) {
      dataMap[attr.name] = attr.value;
    }
  }
  const data = Object.keys(dataMap).length > 0 ? dataMap : null;
  return { tag, id, classes: classList, data };
}

/** Flatten a breadcrumb into a CSS-like selector. Lossy on purpose: we
 *  keep `data-*` out so the result stays a one-glance summary; the
 *  full data lives in `dom_path_breadcrumb`. */
export function flattenSelector(steps: DomPathStep[]): string {
  return steps
    .map((s) => {
      let acc = s.tag;
      if (s.id) acc += `#${s.id}`;
      if (s.classes && s.classes.length > 0) {
        acc += "." + s.classes.join(".");
      }
      return acc;
    })
    .reverse()
    .join(" > ");
}

/** Capture the current text selection if it sits inside a chat preview
 *  pane. Returns null when there's no selection or when the selection
 *  isn't anchored to message wrappers. */
export interface SelectionCapture {
  conversation_uuid: string;
  start_message_uuid: string;
  end_message_uuid: string;
  selected_text: string;
}
export function capturePreviewSelection(): SelectionCapture | null {
  const sel = typeof window !== "undefined" ? window.getSelection() : null;
  if (!sel || sel.isCollapsed) return null;
  const raw = sel.toString();
  const trimmed = raw.replace(/^[\s]+|[\s]+$/g, "");
  if (!trimmed) return null;
  const anchor = sel.anchorNode instanceof Element ? sel.anchorNode : sel.anchorNode?.parentElement;
  const focus = sel.focusNode instanceof Element ? sel.focusNode : sel.focusNode?.parentElement;
  const start = anchor ? messageAncestor(anchor) : null;
  const end = focus ? messageAncestor(focus) : null;
  if (!start || !end) return null;
  const conv = conversationAncestor(start) ?? conversationAncestor(end);
  if (!conv) return null;
  return {
    conversation_uuid: conv,
    start_message_uuid: start,
    end_message_uuid: end,
    selected_text: trimmed,
  };
}

/** Look for the closest `<div id="m-{uuid}" data-msg-index="…">` ancestor —
 *  the wrappers ChatBody.vue emits around each message — and return its
 *  UUID portion (the `m-` prefix is stripped). */
export function messageAncestor(el: Element | null | undefined): string | null {
  let cur: Element | null = el ?? null;
  while (cur) {
    if (cur.id && cur.id.startsWith("m-") && cur.hasAttribute("data-msg-index")) {
      return cur.id.slice(2);
    }
    cur = cur.parentElement;
  }
  return null;
}

/** Walk up looking for an element carrying `data-conversation-uuid`. The
 *  ChatPreviewPane sets this on its root so any descendant can find the
 *  thread without prop-drilling. */
export function conversationAncestor(messageUuid: string | Element): string | null {
  // Allow callers to hand either an element or a message UUID. Looking up
  // by element is more robust (works for page-header context where there
  // is no message), so prefer that path.
  if (typeof messageUuid === "string") {
    const el = document.getElementById(`m-${messageUuid}`);
    if (!el) return null;
    return conversationAncestor(el);
  }
  let cur: Element | null = messageUuid;
  while (cur) {
    const v = cur.getAttribute?.("data-conversation-uuid");
    if (v) return v;
    cur = cur.parentElement;
  }
  return null;
}

/** Build a `FeedbackContext` from the raw inputs every call site can
 *  provide. The `payload` already carries surface-specific shape, so this
 *  function just stamps the universal fields (url + DOM breadcrumb + the
 *  surface discriminator) around it. */
export function buildContext(args: {
  surface: FeedbackSurface;
  anchor: Element | null;
  targetUuids: string[];
  payload: FeedbackPayload;
}): FeedbackContext {
  const breadcrumb = buildDomPath(args.anchor);
  return {
    url: typeof window !== "undefined" ? window.location.href : "",
    surface: args.surface,
    dom_path_breadcrumb: breadcrumb,
    dom_path_selector: flattenSelector(breadcrumb),
    target_uuids: args.targetUuids,
    payload: args.payload,
  };
}
