// Card source: what a card's header, a saved layout and a URL all hold.
// It is one call of a view or component the app already has, with JSON5
// values, like `gridView({ q: "picard" })` or `comp.user.tetris()`.
// Nothing else parses, so nothing a link or a layout carries can become
// code (docs/dev/cards.md § "Card source").
import JSON5 from "json5";

export type CardCall = {
  /** `gridView`, or `comp.<namespace>.<name>` for a component. */
  name: string;
  args: unknown[];
};

export class CardSourceError extends Error {}

const CALL = /^\s*([A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)*)\s*\(([\s\S]*)\)\s*$/;

export function parseCardSource(source: string): CardCall {
  const m = CALL.exec(source);
  if (!m) {
    throw new CardSourceError(
      'card source is a view or component called with values, like gridView({ q: "picard" })',
    );
  }
  const [, name, inner] = m;
  let args: unknown;
  try {
    // The brackets make the argument list one JSON5 array, so a stray
    // `]` or `)` inside it fails the parse rather than ending the list.
    args = JSON5.parse(`[${inner}]`);
  } catch (e) {
    throw new CardSourceError(
      `the arguments to ${name} are not JSON5 values: ${(e as Error).message}`,
    );
  }
  return { name, args: args as unknown[] };
}

export function formatCardCall(name: string, args: unknown[] = []): string {
  return `${name}(${args.map((a) => JSON.stringify(a)).join(", ")})`;
}

/// A component's name split into its namespace and name, or null when
/// `name` is not `comp.<namespace>.<name>`.
export function componentName(name: string): { ns: string; name: string } | null {
  const parts = name.split(".");
  if (parts.length !== 3 || parts[0] !== "comp") return null;
  return { ns: parts[1], name: parts[2] };
}

/// An own property of `table` and a function, or null. Own properties
/// only: `constructor`, `toString` and `__proto__` must reach nothing.
export function lookupFunction(
  table: object,
  key: string,
): ((...args: unknown[]) => unknown) | null {
  if (!Object.hasOwn(table, key)) return null;
  const value = (table as Record<string, unknown>)[key];
  return typeof value === "function" ? (value as (...args: unknown[]) => unknown) : null;
}
