// A document body to the HTML the card mounts: markdown-it with the
// renderers' HTML let through, then the sanitizer. Its own module so the
// document card can render a body without mounting it — to ask the
// server which of its remote references may load before the first
// paint — and the chat body renders it the same way.
//
// A body that comes with its sections is rendered one message at a
// time, so that markup one message leaves open cannot reach the next:
// docs/dev/data_architecture_parse_and_render.md §"One section cannot
// reach the next".
import MarkdownIt from "markdown-it";
import hljs from "highlight.js";
import type { BodySection } from "@/api";
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

export type RenderOptions = Pick<SanitizeOptions, "accept" | "context"> & {
  /** Where each section of the body lies, as the document route sends
   *  it. Absent: the body is rendered whole. */
  sections?: readonly BodySection[] | null;
};

type Piece = { uuid: string | null; md: string };
type Signed = SanitizeOptions & { chipMark: string };

/// The body as HTML for the page: rendered, then sanitized, because the
/// body is whatever the source sent and `html: true` lets it through as
/// HTML.
export function renderDocument(body: string, options: RenderOptions = {}): Sanitized {
  const { sections, ...rest } = options;
  // Fresh each render, so a body cannot carry one forward.
  const signed = { ...rest, chipMark: crypto.randomUUID() };
  const pieces = sections ? cut(body, sections) : null;
  return pieces ? renderPieces(pieces, signed) : renderWhole(body, signed);
}

function renderWhole(body: string, options: Signed, messageHeader = false): Sanitized {
  return sanitizeRenderedHtml(
    md.render(body, { chipMark: options.chipMark, messageHeader }),
    options,
  );
}

function cut(body: string, sections: readonly BodySection[]): Piece[] | null {
  const total = sections.reduce((n, s) => n + s.len, 0);
  if (total !== body.length) {
    // The server checked the layout against the file; this is a bug.
    console.error(`sections cover ${total} of the body's ${body.length} characters`);
    return null;
  }
  let at = 0;
  return sections.map((s) => ({ uuid: s.uuid, md: body.slice(at, (at += s.len)) }));
}

/// The pieces of the page's frame — title, `<details>` openers and
/// closers — are rendered together, so a `<details>` opened in one closes
/// in a later one, with a placeholder for each message; each message is
/// rendered alone and put in its place.
function renderPieces(pieces: Piece[], options: Signed): Sanitized {
  const messages = new Set(pieces.flatMap((p) => (p.uuid === null ? [] : [p.uuid])));
  const slot = crypto.randomUUID();
  const frameMd = pieces
    .map((p, i) => (p.uuid === null ? p.md : `\n<div data-slot="${slot}-${i}"></div>\n\n`))
    .join("");
  const frame = renderWhole(frameMd, options);
  const drawn = new Map<number, Sanitized>();
  pieces.forEach((p, i) => {
    if (p.uuid !== null) drawn.set(i, renderMessage(p.md, p.uuid, messages, options));
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
    const alone = pieces.map(
      (p, i) => drawn.get(i) ?? renderWhole(p.md, { ...options, reservedSections: messages }),
    );
    return { html: alone.map((a) => a.html).join(""), remote: alone.flatMap((a) => a.remote) };
  }
  const remote = [...frame.remote, ...[...drawn.values()].flatMap((d) => d.remote)];
  return { html, remote };
}

/// The renderer's wrapper: its own line opening the piece, its `</div>`
/// closing it.
const WRAPPER = /^(<div\b[^>\n]*>)\n([\s\S]*)\n<\/div>\s*$/;
const DRAWN_WRAPPER = /^<div\b[^>]*\sdata-section-uuid="([^"]*)"[^>]*><\/div>$/;

/// One message, as a fragment of its own. A piece in the wrapper's shape
/// has its inside rendered as a further fragment, so a `</div>` in the
/// message cannot close the wrapper early; only the wrapper answers to
/// the message's id.
function renderMessage(
  text: string,
  uuid: string,
  messages: ReadonlySet<string>,
  options: Signed,
): Sanitized {
  const parts = WRAPPER.exec(text);
  if (parts) {
    const shell = sanitizeRenderedHtml(`${parts[1]}</div>`, options).html;
    if (DRAWN_WRAPPER.exec(shell)?.[1] === uuid) {
      const inside = renderWhole(parts[2], { ...options, reservedSections: messages }, true);
      return {
        html: shell.slice(0, -"</div>".length) + inside.html + "</div>",
        remote: inside.remote,
      };
    }
  }
  const others = new Set([...messages].filter((m) => m !== uuid));
  return renderWhole(text, { ...options, reservedSections: others });
}
