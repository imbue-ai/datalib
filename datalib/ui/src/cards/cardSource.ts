// Turns card source (`gridView()`, `comp.user.tetris()`) into a runnable
// CardRender: parse the call, look its name up, call it with the values.
// There is no evaluator: card source cannot say anything else.
import { viewLibs } from "./libs";
import { resolveComponent } from "./frontendRegistry";
import { CardSourceError, componentName, lookupFunction, parseCardSource } from "./cardCall";
import type { CardRender } from "./types";

export type CompiledCard = {
  render: CardRender;
};

async function resolveFactory(name: string): Promise<(...args: unknown[]) => unknown> {
  const builtin = lookupFunction(viewLibs, name);
  if (builtin) return builtin;
  const comp = componentName(name);
  const factory = comp ? await resolveComponent(comp.ns, comp.name) : null;
  if (!factory) throw new CardSourceError(`no view or component named ${name}`);
  return factory;
}

export async function compileCardSource(source: string): Promise<CompiledCard> {
  const call = parseCardSource(source);
  const factory = await resolveFactory(call.name);
  const render = factory(...call.args);
  if (typeof render !== "function") {
    throw new Error(`${call.name} must return a render function, got ${typeof render}`);
  }
  return { render: render as CardRender };
}
