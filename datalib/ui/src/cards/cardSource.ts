// Turns card source — the JS expression shown in a card's header, like
// `gridView()` or `comp.user.tetris()` — into a runnable CardRender.
//
// The expression is evaluated with the builtin view factories in scope,
// plus a single `comp` object holding every custom component namespace
// the source refers to. Card source is plain JS that calls them; it has
// no implicit access to app internals beyond what those close over.
//
// Resolving `comp` means importing component modules, so compilation is
// async.
import { viewLibs } from "./libs";
import { resolveCompScope } from "./frontendRegistry";
import type { CardRender } from "./types";

export type CompiledCard = {
  render: CardRender;
};

export async function compileCardSource(source: string): Promise<CompiledCard> {
  const scope = new Map<string, unknown>(Object.entries(viewLibs));
  // One name for every custom component, whoever wrote it:
  // `comp.<namespace>.<name>`. Namespacing is what lets two applet
  // instances both export `channels`, and what lets a user component be
  // called `gridView` without shadowing the builtin.
  scope.set("comp", await resolveCompScope(source));
  const names = [...scope.keys()];
  // `new Function` (not eval) so the source only sees the names we pass
  // in — the view libs and `comp` — plus globals.
  const factory = new Function(...names, `"use strict"; return (${source});`);
  const render = factory(...names.map((n) => scope.get(n)));
  if (typeof render !== "function") {
    throw new Error(
      `card source must evaluate to a render function, got ${typeof render}`,
    );
  }
  return { render: render as CardRender };
}
