// A document body to the HTML the card mounts: markdown-it with the
// renderers' HTML let through, then the sanitizer. Its own module so the
// document card can render a body without mounting it — to ask the
// server which of its remote references may load before the first
// paint — and the chat body renders it the same way.
//
// A body comes as the renderer's pieces and is rendered one message at a
// time, so that markup one message leaves open cannot reach the next:
// docs/dev/data_architecture_parse_and_render.md §"One section cannot
// reach the next". A message in a box comes as the box's class and what
// is inside it, and the box is drawn here.
import MarkdownIt from "markdown-it";
import hljs from "highlight.js";
import type { BodyPiece } from "@/api";
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

/// A document's markdown whole: its pieces joined, each box drawn as
/// render draws it (`section::draw_box`).
export function bodyOf(doc: { pieces: readonly BodyPiece[] }): string {
  return doc.pieces
    .map((p) =>
      p.uuid !== null && p.class ? `${boxOpen(p.uuid, p.class)}\n\n${p.md}</div>\n\n` : p.md,
    )
    .join("");
}

const attr = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

/// The opening tag of a message's box (`section::box_open`).
function boxOpen(uuid: string, cls: string): string {
  return `<div id="m-${attr(uuid)}" data-section-uuid="${attr(uuid)}" class="${attr(cls)}">`;
}

/// The body as HTML for the page: rendered, then sanitized, because the
/// body is whatever the source sent and `html: true` lets it through as
/// HTML. A string is markdown with no messages marked in it, rendered
/// whole.
export function renderDocument(
  body: readonly BodyPiece[] | string,
  options: RenderOptions = {},
): Sanitized {
  // Fresh each render, so a body cannot carry one forward.
  const signed = { ...options, chipMark: crypto.randomUUID() };
  return typeof body === "string" ? renderWhole(body, signed) : renderPieces(body, signed);
}

function renderWhole(body: string, options: Signed, messageHeader = false): Sanitized {
  return sanitizeRenderedHtml(
    md.render(body, { chipMark: options.chipMark, messageHeader }),
    options,
  );
}

/// The pieces of the page's frame — title, `<details>` openers and
/// closers — are rendered together, so a `<details>` opened in one closes
/// in a later one, with a placeholder for each message; each message is
/// rendered alone and put in its place.
function renderPieces(pieces: readonly BodyPiece[], options: Signed): Sanitized {
  const slot = crypto.randomUUID();
  const frameMd = pieces
    .map((p, i) => (p.uuid === null ? p.md : `\n<div data-slot="${slot}-${i}"></div>\n\n`))
    .join("");
  const frame = renderWhole(frameMd, options);
  const drawn = new Map<number, Sanitized>();
  pieces.forEach((p, i) => {
    if (p.uuid !== null) drawn.set(i, renderMessage({ ...p, uuid: p.uuid }, options));
  });
  let placed = 0;
  const html = frame.html.replace(
    new RegExp(`<div data-slot="${slot}-(\\d+)"></div>`, "g"),
    (_, i: string) => {
      placed += 1;
      return drawn.get(Number(i))?.html ?? "";
    },
  );
  if (placed !== drawn.size) {
    // A frame piece swallowed a placeholder (an unclosed comment, say):
    // every piece alone keeps every message, without the folds.
    console.error(`placed ${placed} of ${drawn.size} messages; drawing each piece alone`);
    const alone = pieces.map((p, i) => drawn.get(i) ?? renderWhole(p.md, options));
    return { html: alone.map((a) => a.html).join(""), remote: alone.flatMap((a) => a.remote) };
  }
  const remote = [...frame.remote, ...[...drawn.values()].flatMap((d) => d.remote)];
  return { html, remote };
}

/// One message, as a fragment of its own. One in a box has the box drawn
/// around it here, so nothing inside can close the box early; the box is
/// then the outermost element naming the message, which is what answers to
/// its id (`sectionEl` in `chatSections.js`), whatever the message holds.
function renderMessage(piece: BodyPiece & { uuid: string }, options: Signed): Sanitized {
  if (!piece.class) return renderWhole(piece.md, options);
  const inside = renderWhole(piece.md, options, true);
  return {
    html: `${boxOpen(piece.uuid, piece.class)}${inside.html}</div>`,
    remote: inside.remote,
  };
}
