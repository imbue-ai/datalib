import type { Bus, BusHandler } from "./types";

export function createBus(): Bus {
  const subs = new Map<string, Set<BusHandler>>();
  return {
    publish(topic, payload, opts) {
      const set = subs.get(topic);
      if (!set) return;
      const meta = { from: opts?.from };
      // Copy before iterating: a handler may unsubscribe itself.
      for (const h of Array.from(set)) {
        try {
          h(payload, meta);
        } catch (e) {
          console.error("[bus]", topic, e);
        }
      }
    },
    subscribe(topic, handler) {
      let set = subs.get(topic);
      if (!set) {
        set = new Set();
        subs.set(topic, set);
      }
      set.add(handler);
      return () => {
        const s = subs.get(topic);
        if (!s) return;
        s.delete(handler);
        if (s.size === 0) subs.delete(topic);
      };
    },
  };
}
