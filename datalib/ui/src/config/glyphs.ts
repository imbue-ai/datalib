// Monochrome UI glyphs: 24×24 `path` data, drawn in `currentColor`.
//
// These are Material Design Icons (Apache-2.0), vendored as raw path
// data rather than pulled in as a dependency. The whole set is ~2500
// icons and a build-time integration for the dozen we use; the paths
// themselves are three lines each and cost nothing to carry. Every
// entry below is verbatim from
// `google/material-design-icons/src/<category>/<name>/materialicons/24px.svg`,
// with the transparent 24×24 bounding rect that file also carries
// dropped — it is layout padding for the font, not part of the mark.
//
// Distinct from `icons.ts`, which maps a *service* to its brand mark.
// A brand mark is a picture of a company and is used nominatively; a
// glyph here is a picture of a verb and is recolored freely.

/// Icons naming a step's role in the pipeline (the Step column), plus
/// the two action buttons that switch on run state.
///
/// The metaphors, since a glyph is only as good as the word behind it:
/// a fetch *syncs* with something remote; a render makes a document you
/// can read; an index is a card catalog over those documents; an applet
/// is a small app the gateway hosts; anything else is an arbitrary
/// command, which is a terminal.
export const STEP_GLYPHS = {
  // notification/sync — two arrows chasing each other round a circle.
  fetch:
    "M12 4V1L8 5l4 4V6c3.31 0 6 2.69 6 6 0 1.01-.25 1.97-.7 2.8l1.46 1.46C19.54 15.03 20 13.57 20 12c0-4.42-3.58-8-8-8zm0 14c-3.31 0-6-2.69-6-6 0-1.01.25-1.97.7-2.8L5.24 7.74C4.46 8.97 4 10.43 4 12c0 4.42 3.58 8 8 8v3l4-4-4-4v3z",
  // communication/import_contacts — an open book.
  render:
    "M17.5,4.5c-1.95,0-4.05,0.4-5.5,1.5c-1.45-1.1-3.55-1.5-5.5-1.5S2.45,4.9,1,6v14.65c0,0.65,0.73,0.45,0.75,0.45 C3.1,20.45,5.05,20,6.5,20c1.95,0,4.05,0.4,5.5,1.5c1.35-0.85,3.8-1.5,5.5-1.5c1.65,0,3.35,0.3,4.75,1.05 C22.66,21.26,23,20.86,23,20.6V6C21.51,4.88,19.37,4.5,17.5,4.5z M21,18.5c-1.1-0.35-2.3-0.5-3.5-0.5c-1.7,0-4.15,0.65-5.5,1.5V8 c1.35-0.85,3.8-1.5,5.5-1.5c1.2,0,2.4,0.15,3.5,0.5V18.5z",
  // av/library_books — stacked ruled cards: a card catalog.
  index:
    "M4 6H2v14c0 1.1.9 2 2 2h14v-2H4V6zm16-4H8c-1.1 0-2 .9-2 2v12c0 1.1.9 2 2 2h12c1.1 0 2-.9 2-2V4c0-1.1-.9-2-2-2zm-1 9H9V9h10v2zm-4 4H9v-2h6v2zm4-8H9V5h10v2z",
  // device/widgets — the generic "app / component" mark.
  applet: "M13 13v8h8v-8h-8zM3 21h8v-8H3v8zM3 3v8h8V3H3zm13.66-1.31L11 7.34 16.66 13l5.66-5.66-5.66-5.65z",
  // action/terminal — a shell prompt, for a step that is just a command.
  other:
    "M20,4H4C2.89,4,2,4.9,2,6v12c0,1.1,0.89,2,2,2h16c1.1,0,2-0.9,2-2V6C22,4.9,21.11,4,20,4z M20,18H4V8h16V18z M18,17h-6v-2 h6V17z M7.5,17l-1.41-1.41L8.67,13l-2.59-2.59L7.5,9l4,4L7.5,17z",
} as const;

/// Icons for the Status column.
///
/// Keyed on the vocabulary `Manager2View` normalizes to, which is the
/// runner's own (`succeeded` / `skipped_up_to_date` / `blocked` /
/// `failed`) plus the four states only a reader can know: `running`,
/// `queued`, `interrupted`, and `never_run`.
///
/// `running` is deliberately absent — a still frame cannot say "still
/// going", so that one state is a CSS spinner rather than a path.
export const STATUS_GLYPHS: Record<string, string> = {
  // action/check_circle
  succeeded: "M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm-2 15-5-5 1.41-1.41L10 14.17l7.59-7.59L19 8l-9 9z",
  // action/done_all — checked *and* already current, which is two ticks.
  skipped_up_to_date:
    "M18 7l-1.41-1.41-6.34 6.34 1.41 1.41L18 7zm4.24-1.41L11.66 16.17 7.48 12l-1.41 1.41L11.66 19l12-12-1.42-1.41zM.41 13.41L6 19l1.41-1.41L1.83 12 .41 13.41z",
  // alert/error
  failed: "M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm1 15h-2v-2h2v2zm0-4h-2V7h2v6z",
  // content/block — an upstream step failed, so this was never invoked.
  blocked:
    "M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zM4 12c0-4.42 3.58-8 8-8 1.85 0 3.55.63 4.9 1.69L5.69 16.9C4.63 15.55 4 13.85 4 12zm8 8c-1.85 0-3.55-.63-4.9-1.69L18.31 7.1C19.37 8.45 20 10.15 20 12c0 4.42-3.58 8-8 8z",
  // alert/warning — a run that died is not a reported failure, but it
  // is not a success either.
  interrupted: "M1 21h22L12 2 1 21zm12-3h-2v-2h2v2zm0-4h-2v-4h2v4z",
  // action/hourglass_empty — due to run, and not started.
  //
  // An hourglass rather than the three-dot `pending`: the dots say
  // "something is happening slowly", which is what the spinner beside
  // it already means. This state is the opposite — nothing is happening
  // to this step *yet*, and the question a reader has is what it is
  // behind. The tooltip answers that by name.
  queued:
    "M6 2v6h.01L6 8.01 10 12l-4 4 .01.01H6V22h12v-5.99h-.01L18 16l-4-4 4-3.99-.01-.01H18V2H6zm10 14.5V20H8v-3.5l4-4 4 4zm-4-5l-4-4V4h8v3.5l-4 4z",
  // content/remove_circle_outline — an empty slot, not a bad outcome.
  never_run:
    "M7 11v2h10v-2H7zm5-9C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm0 18c-4.41 0-8-3.59-8-8s3.59-8 8-8 8 3.59 8 8-3.59 8-8 8z",
};

/// Build a `<svg>` carrying one path, sized for a table cell.
///
/// `label` is the accessible name. Every use of these glyphs replaced a
/// column that used to spell the word out, so the word has to survive
/// somewhere a screen reader can reach.
///
/// Deliberately NOT an SVG `<title>` as well. A `<title>` renders as a
/// native browser tooltip, and the cell wrapper already sets one — a
/// fuller one, carrying the failure message or what a queued row is
/// waiting for. Two nested tooltips means the inner, shorter one wins
/// on hover, which is how the interesting half stayed invisible.
export function glyphSvg(path: string, label: string, size = 16): SVGSVGElement {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("width", String(size));
  svg.setAttribute("height", String(size));
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", label);
  const p = document.createElementNS("http://www.w3.org/2000/svg", "path");
  p.setAttribute("d", path);
  p.setAttribute("fill", "currentColor");
  svg.appendChild(p);
  return svg;
}
