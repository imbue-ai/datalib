// The user-defined component library, client side.
//
// Cards can invoke named components ("aliases") by bare name —
// `myComponent()` — exactly like the builtin `gridView`/`documentView`.
// An alias' source is a JS expression that evaluates to a *factory* (a
// function returning a CardRender). Aliases may reference other aliases,
// so resolving one is recursive.
//
// Two jobs live here:
//   1. Keep a reactive manifest (name → content hash) by polling
//      `/api/lib`, so a card can re-render the instant an alias it uses
//      changes. Polling (not a socket) is enough for a local
//      single-user tool and needs no server-push machinery.
//   2. Resolve an alias name to its runtime factory value, evaluating
//      its source with the view libs + its own alias deps in scope,
//      memoized by content hash.
//
// `cardSource.ts` is the only consumer; `ShadowCard.vue` reads the
// manifest to decide when to recompile.

import { ref, type Ref } from "vue";
import { listLib, fetchLib } from "@/api";
import { scopeHelpers, viewLibs } from "./libs";

// name → sha256 of its current source. Reactive: cards watch this.
export const aliasManifest: Ref<Map<string, string>> = ref(new Map());

// name → gallery description, for the aliases that carry one. A
// described alias advertises itself in the new-card gallery
// (libs/galleryView.ts), which invokes it with no arguments. Kept
// separate from aliasManifest so a description edit doesn't count as
// a source change (no recompiles), and vice versa.
export const aliasDescriptions: Ref<Map<string, string>> = ref(new Map());

let pollTimer: ReturnType<typeof setInterval> | null = null;
let firstLoad: Promise<void> | null = null;
const POLL_MS = 1500;

function sameMap(a: Map<string, string>, b: Map<string, string>): boolean {
  if (a.size !== b.size) return false;
  for (const [k, v] of a) if (b.get(k) !== v) return false;
  return true;
}

async function refreshManifest(): Promise<void> {
  try {
    const entries = await listLib();
    const next = new Map(entries.map((e) => [e.name, e.hash] as const));
    const nextDesc = new Map(
      entries
        .filter((e) => (e.description ?? "").trim() !== "")
        .map((e) => [e.name, e.description!] as const),
    );
    // Same replace-only-on-change discipline as the manifest, so the
    // gallery's watcher doesn't repaint every poll tick.
    if (!sameMap(nextDesc, aliasDescriptions.value)) {
      aliasDescriptions.value = nextDesc;
    }
    // Replace only on change so we don't wake every card's watcher each
    // poll tick.
    if (!sameMap(next, aliasManifest.value)) {
      // A resolved factory value closes over the *values* of the aliases
      // it referenced. The per-name `valueCache` is keyed by that alias'
      // own hash, which doesn't change when a transitive dependency
      // does — so a changed component could otherwise keep serving a
      // value built from a stale dependency. Drop all cached values on
      // any manifest change and let the next compile rebuild them.
      // (sourceCache is keyed by content hash and stays valid.)
      valueCache.clear();
      aliasManifest.value = next;
    }
  } catch {
    // Backend blip — keep the last good manifest and try next tick.
  }
}

// Load the manifest once (awaitable) and start polling. Idempotent —
// safe to call from every card on mount.
export function ensureManifest(): Promise<void> {
  if (!firstLoad) {
    firstLoad = refreshManifest();
    pollTimer = setInterval(() => void refreshManifest(), POLL_MS);
  }
  return firstLoad;
}

// Pick a fresh alias name not currently in the manifest. The name is a
// valid JS identifier (it's injected as a bare name and invoked as
// `name()`), prefixed so it can't collide with a builtin.
export function freshAliasName(): string {
  const taken = aliasManifest.value;
  for (;;) {
    const buf = new Uint32Array(1);
    crypto.getRandomValues(buf);
    const name = `card_${buf[0].toString(36)}`;
    if (!taken.has(name) && !(name in viewLibs)) return name;
  }
}

// --- source cache + resolution ---------------------------------------------

const sourceCache = new Map<string, { hash: string; source: string }>();
const valueCache = new Map<string, { hash: string; value: unknown }>();

async function getSource(name: string, hash: string): Promise<string> {
  const c = sourceCache.get(name);
  if (c && c.hash === hash) return c.source;
  const source = await fetchLib(name);
  sourceCache.set(name, { hash, source });
  return source;
}

// Identifiers a piece of source references "freely" — every identifier
// token not immediately preceded by `.` (so `obj.foo` doesn't count as
// a reference to `foo`). Intentionally over-approximate: it scans
// across string/comment contents too, so it may flag a name that isn't
// really used. That only ever causes an extra (harmless) re-render; the
// dangerous direction — missing a real dependency — can't happen,
// because every identifier token is considered.
export function referencedIdentifiers(source: string): Set<string> {
  const ids = new Set<string>();
  const isStart = (c: string) => /[A-Za-z_$]/.test(c);
  const isPart = (c: string) => /[A-Za-z0-9_$]/.test(c);
  const n = source.length;
  let i = 0;
  while (i < n) {
    if (isStart(source[i])) {
      let j = i + 1;
      while (j < n && isPart(source[j])) j++;
      let k = i - 1;
      while (k >= 0 && /\s/.test(source[k])) k--;
      if (k < 0 || source[k] !== ".") ids.add(source.slice(i, j));
      i = j;
    } else {
      i++;
    }
  }
  return ids;
}

// Direct alias dependencies of `source`: referenced identifiers that
// are actually names in the current library. `self` (the alias whose
// source this is) is excluded — a component is never injected into its
// own scope, so a self-reference is never a real dependency; without
// this, a component that merely mentions its own name (even inside a
// string) would resolve to a bogus dependency cycle.
function directAliasDeps(source: string, self?: string): string[] {
  const m = aliasManifest.value;
  return [...referencedIdentifiers(source)].filter(
    (id) => id !== self && m.has(id),
  );
}

function evalInScope(source: string, scope: Map<string, unknown>): unknown {
  const names = [...scope.keys()];
  // new Function (not eval) so the source sees only the names we pass —
  // the view libs plus the alias deps — and JS globals.
  const fn = new Function(...names, `"use strict"; return (${source});`);
  return fn(...names.map((nm) => scope.get(nm)));
}

// Resolve one alias to its factory value. `resolving` is the current
// DFS path (for cycle detection); `closure` accumulates every alias
// touched, so the caller knows the full set to watch for changes.
async function resolveAlias(
  name: string,
  resolving: Set<string>,
  closure: Set<string>,
): Promise<unknown> {
  closure.add(name);
  const hash = aliasManifest.value.get(name);
  if (hash === undefined) {
    throw new Error(`component "${name}" is not defined`);
  }
  const cached = valueCache.get(name);
  if (cached && cached.hash === hash) return cached.value;
  if (resolving.has(name)) {
    throw new Error(`component dependency cycle at "${name}"`);
  }
  resolving.add(name);
  const source = await getSource(name, hash);
  const scope = new Map<string, unknown>([
    ...Object.entries(viewLibs),
    ...Object.entries(scopeHelpers),
  ]);
  // Sequential (not parallel) so `resolving` is exactly the ancestor
  // path: correct cycle detection over a tiny graph.
  for (const dep of directAliasDeps(source, name)) {
    scope.set(dep, await resolveAlias(dep, resolving, closure));
  }
  resolving.delete(name);
  const value = evalInScope(source, scope);
  valueCache.set(name, { hash, value });
  return value;
}

export type ResolvedScope = {
  // viewLibs + every directly-referenced alias, ready to inject.
  scope: Map<string, unknown>;
  // every alias in the transitive closure — the set to watch for change.
  closure: Set<string>;
};

// Build the scope a card's source should be evaluated in: the view libs
// plus each alias it references (resolved to its factory value). Also
// returns the transitive closure so the host knows what to watch.
export async function resolveScopeFor(source: string): Promise<ResolvedScope> {
  await ensureManifest();
  const scope = new Map<string, unknown>([
    ...Object.entries(viewLibs),
    ...Object.entries(scopeHelpers),
  ]);
  const closure = new Set<string>();
  const resolving = new Set<string>();
  for (const dep of directAliasDeps(source)) {
    scope.set(dep, await resolveAlias(dep, resolving, closure));
  }
  return { scope, closure };
}
