// Turns card source — the JS expression shown in a column's header,
// like `gridView()` or `documentView("abcd…")` — into a runnable
// CardRender. The expression is evaluated with the view factories
// (ViewLibs) plus any user-defined component aliases it references in
// scope, so card source is plain JS that calls them; it has no implicit
// access to app internals beyond what those factories close over.
//
// Resolving aliases means fetching their source, so compilation is
// async. The returned `deps` is the transitive set of aliases the card
// touched — the host re-renders when any of them changes (see
// aliasRegistry.ts and ShadowCard.vue).
import { resolveScopeFor } from "./aliasRegistry";
import type { CardRender } from "./types";

export type CompiledCard = {
  render: CardRender;
  deps: Set<string>;
};

export async function compileCardSource(source: string): Promise<CompiledCard> {
  const { scope, closure } = await resolveScopeFor(source);
  const names = [...scope.keys()];
  // `new Function` (not eval) so the source only sees the names we pass
  // in — view libs and referenced aliases — plus globals.
  const factory = new Function(...names, `"use strict"; return (${source});`);
  const render = factory(...names.map((n) => scope.get(n)));
  if (typeof render !== "function") {
    throw new Error(
      `card source must evaluate to a render function, got ${typeof render}`,
    );
  }
  return { render: render as CardRender, deps: closure };
}
