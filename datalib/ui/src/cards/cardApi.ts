// api.ts, bound to one card: every call runs in that card's scope
// (cardScope.ts), so its requests say which card made them. A card's
// components take it from `useApi()`; outside a card that is api.ts as is.
import { inject, type InjectionKey } from "vue";
import * as api from "@/api";
import { inCard } from "./cardScope";
import type { CardCtx } from "./types";

export type Api = typeof api;

export const CARD_CTX: InjectionKey<CardCtx> = Symbol("card ctx");

const bound = new WeakMap<CardCtx, Api>();

export function cardApi(ctx: CardCtx): Api {
  let out = bound.get(ctx);
  if (!out) {
    const entries = Object.entries(api).map(([name, value]) => [
      name,
      typeof value === "function"
        ? (...args: unknown[]) =>
            inCard({ id: ctx.cardId, type: ctx.cardType }, () =>
              (value as (...a: unknown[]) => unknown)(...args),
            )
        : value,
    ]);
    out = Object.fromEntries(entries) as Api;
    bound.set(ctx, out);
  }
  return out;
}

export function useApi(): Api {
  const ctx = inject(CARD_CTX, null);
  return ctx ? cardApi(ctx) : api;
}
