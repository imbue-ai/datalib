// Turns card source (`gridView()`, `comp.user.tetris()`) into a runnable
// CardRender: parse the call, look its name up, call it with the values.
// There is no evaluator: card source cannot say anything else.
import { viewLibs } from "./libs";
import { resolveComponent } from "./frontendRegistry";
import { CardSourceError, componentName, lookupFunction, parseCardSource } from "./cardCall";
import { withDateChips } from "./appletDates";
import type { CardRender } from "./types";

export type CompiledCard = {
  render: CardRender;
};

type Factory = (...args: unknown[]) => unknown;

/// The function a name calls, and whether it is an applet's component
/// rather than one of the app's own views.
async function resolveFactory(name: string): Promise<{ factory: Factory; applet: boolean }> {
  const builtin = lookupFunction(viewLibs, name);
  if (builtin) return { factory: builtin, applet: false };
  const comp = componentName(name);
  const factory = comp ? await resolveComponent(comp.ns, comp.name) : null;
  if (!factory) throw new CardSourceError(`no view or component named ${name}`);
  return { factory, applet: true };
}

export async function compileCardSource(source: string): Promise<CompiledCard> {
  const call = parseCardSource(source);
  const { factory, applet } = await resolveFactory(call.name);
  const render = factory(...call.args);
  if (typeof render !== "function") {
    throw new Error(`${call.name} must return a render function, got ${typeof render}`);
  }
  // The app's own views draw their chips; an applet's card has only
  // its `<time>` elements, which the app draws.
  return { render: applet ? withDateChips(render as CardRender) : (render as CardRender) };
}
