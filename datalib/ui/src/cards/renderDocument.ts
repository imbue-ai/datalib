// A document body to the HTML the card mounts: markdown-it with the
// renderers' HTML let through, then the sanitizer. Its own module so the
// document card can render a body without mounting it — to ask the
// server which of its remote references may load before the first
// paint — and the chat body renders it the same way.
//
// A body comes as the renderer's pieces and every piece is rendered alone,
// so that markup one leaves open cannot reach the next:
// docs/dev/data_architecture_parse_and_render.md §"One section cannot
// reach the next". A message in a box comes as the box's class and what
// is inside it, and a fold as a node around its pieces; both are drawn
// here.
import MarkdownIt from "markdown-it";
import hljs from "highlight.js";
import type { BodyFold, BodyNode, BodyPiece } from "@/api";
import { chipLinks } from "./chipLinks";
import { sanitizeRenderedHtml, type Sanitized, type SanitizeOptions } from "./sanitize";

function highlight(code: string, lang: string): string {
  if (lang && hljs.getLanguage(lang)) {
    try {
      return hljs.highlight(code, { language: lang }).value;
    } catch {
      /* fall through to escape */
    }
  }
  return code.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

const md = new MarkdownIt({
  html: true,
  linkify: true,
  breaks: false,
  highlight,
});
md.use(chipLinks);

export type RenderOptions = Pick<SanitizeOptions, "accept" | "context">;

type Signed = SanitizeOptions & { chipMark: string };

/// A document's markdown whole: its pieces joined, each box and fold drawn
/// as render draws it (`section::Node::draw`).
export function bodyOf(doc: { pieces: readonly BodyNode[] }): string {
  return doc.pieces.map(drawn).join("");
}

function drawn(node: BodyNode): string {
  if (isFold(node)) {
    return `${foldOpen(node)}\n<summary>${node.summary}</summary>\n\n${node.pieces.map(drawn).join("")}</details>\n\n`;
  }
  return node.uuid !== null && node.class
    ? `${boxOpen(node.uuid, node.class)}\n\n${node.md}</div>\n\n`
    : node.md;
}

export function isFold(node: BodyNode): node is BodyFold {
  return "fold" in node;
}

const attr = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

/// The opening tag of a message's box (`section::box_open`).
function boxOpen(uuid: string, cls: string): string {
  return `<div id="m-${attr(uuid)}" data-section-uuid="${attr(uuid)}" class="${attr(cls)}">`;
}

/// The opening tag of a fold (`section::draw_fold_open`).
function foldOpen(fold: BodyFold): string {
  return `<details class="${attr(fold.fold)}">`;
}

/// The body as HTML for the page: rendered, then sanitized, because the
/// body is whatever the source sent and `html: true` lets it through as
/// HTML. A string is markdown with no messages marked in it, rendered
/// whole.
export function renderDocument(
  body: readonly BodyNode[] | string,
  options: RenderOptions = {},
): Sanitized {
  // Fresh each render, so a body cannot carry one forward.
  const signed = { ...options, chipMark: crypto.randomUUID() };
  return typeof body === "string" ? renderWhole(body, signed) : renderNodes(body, signed);
}

function renderWhole(body: string, options: Signed, messageHeader = false): Sanitized {
  return sanitizeRenderedHtml(
    md.render(body, { chipMark: options.chipMark, messageHeader }),
    options,
  );
}

function renderNodes(nodes: readonly BodyNode[], options: Signed): Sanitized {
  const drawn = nodes.map((n) => (isFold(n) ? renderFold(n, options) : renderPiece(n, options)));
  return { html: drawn.map((d) => d.html).join(""), remote: drawn.flatMap((d) => d.remote) };
}

/// A fold, drawn here around its pieces, so no piece opens or closes it;
/// its summary is the HTML the renderer escaped, sanitized like any other.
function renderFold(fold: BodyFold, options: Signed): Sanitized {
  const summary = sanitizeRenderedHtml(fold.summary, options);
  const inside = renderNodes(fold.pieces, options);
  return {
    html: `${foldOpen(fold)}<summary>${summary.html}</summary>${inside.html}</details>`,
    remote: [...summary.remote, ...inside.remote],
  };
}

/// One piece, as a fragment of its own. A message in a box has the box
/// drawn around it here, so nothing inside can close the box early; the
/// box is then the outermost element naming the message, which is what
/// answers to its id (`sectionEl` in `chatSections.js`), whatever the
/// message holds.
function renderPiece(piece: BodyPiece, options: Signed): Sanitized {
  if (piece.uuid === null || !piece.class) return renderWhole(piece.md, options);
  const inside = renderWhole(piece.md, options, true);
  return {
    html: `${boxOpen(piece.uuid, piece.class)}${inside.html}</div>`,
    remote: inside.remote,
  };
}
