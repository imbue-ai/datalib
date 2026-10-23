// A document body to the HTML the card mounts: markdown-it with the
// renderers' HTML let through, then the sanitizer. Its own module so the
// document card can render a body without mounting it — to ask the
// server which of its remote references may load before the first
// paint — and the chat body renders it the same way.
import MarkdownIt from "markdown-it";
import hljs from "highlight.js";
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

/// The body as HTML for the page: rendered, then sanitized, because the
/// body is whatever the source sent and `html: true` lets it through as
/// HTML.
export function renderDocument(body: string, options: SanitizeOptions = {}): Sanitized {
  return sanitizeRenderedHtml(md.render(body), options);
}
