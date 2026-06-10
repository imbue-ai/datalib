// Turns card source — the JS expression shown in a column's header,
// like `gridView()` or `documentView("abcd…")` — into a runnable
// CardRender. The expression is evaluated with the view factories
// (ViewLibs) as its only names in scope, so card source is plain JS
// that calls them; it has no implicit access to app internals beyond
// what the factories close over.
import { viewLibs } from "./libs";
import type { CardRender } from "./types";

export function compileCardSource(source: string): CardRender {
  const names = Object.keys(viewLibs) as (keyof typeof viewLibs)[];
  // `new Function` (not eval) so the source only sees the factory
  // names we pass in, plus globals — same trust model as the v1
  // CardColumn sandbox, minus the iframe.
  const factory = new Function(
    ...names,
    `"use strict"; return (${source});`,
  );
  const render = factory(...names.map((n) => viewLibs[n]));
  if (typeof render !== "function") {
    throw new Error(
      `card source must evaluate to a render function, got ${typeof render}`,
    );
  }
  return render as CardRender;
}
