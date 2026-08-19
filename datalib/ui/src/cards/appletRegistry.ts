// Applet components, client side.
//
// An applet is a config-declared server that contributes both card
// components and the endpoints behind them (see
// datalib/backend/http/src/applets.rs). This module turns what
// `GET /api/applets` reports into names card source can call:
//
//     slack_work.channels("slack_work")
//
// The applet's **id** is the namespace. That is the whole answer to
// the name-collision problem: two instances of one command both want
// to be called "channels", and under a global namespace one of them
// would have to lose. Scoping by id means component names only have
// to be unique inside a single applet's own manifest, which one author
// controls — a cross-applet collision cannot be expressed, so nothing
// here arbitrates one.
//
// Module loading leans on the browser rather than reimplementing it.
// Components are fetched from the flat, content-addressed store at
// `/modules/<sha256>`, and the browser keeps at most one module
// instance per resolved URL — so two instances reporting the same hash
// share one evaluated module for free, and instances on drifted builds
// report different hashes and correctly get different code. There is
// no module cache in this file; a hash-keyed one would duplicate the
// module registry the platform already maintains. The one cache here
// memoizes the *namespace objects* built around those modules, which
// the platform knows nothing about.

import { ref, type Ref } from "vue";
import { fetchApplets, type AppletEntry, type AppletGalleryEntry } from "@/api";

// id → { componentName → module hash }, as last reported.
export const appletManifest: Ref<Map<string, Map<string, string>>> = ref(new Map());

// Gallery snippets contributed by applets, flattened in config order.
// These are full card sources, not names: the applet generated them
// knowing its own id, which is what lets one component appear several
// times with different arguments.
export const appletGallery: Ref<AppletGalleryEntry[]> = ref([]);

// id → discovery error, for applets that are configured but broken. A
// configured applet that silently vanished would look like a config
// that never saved, so these stay visible.
export const appletErrors: Ref<Map<string, string>> = ref(new Map());

let firstLoad: Promise<void> | null = null;
let pollTimer: ReturnType<typeof setInterval> | null = null;
const POLL_MS = 4000;

function sameManifest(
  a: Map<string, Map<string, string>>,
  b: Map<string, Map<string, string>>,
): boolean {
  if (a.size !== b.size) return false;
  for (const [id, comps] of a) {
    const other = b.get(id);
    if (!other || other.size !== comps.size) return false;
    for (const [name, hash] of comps) if (other.get(name) !== hash) return false;
  }
  return true;
}

function sameGallery(a: AppletGalleryEntry[], b: AppletGalleryEntry[]): boolean {
  if (a.length !== b.length) return false;
  return a.every(
    (x, i) =>
      x.source === b[i].source &&
      x.title === b[i].title &&
      x.description === b[i].description,
  );
}

async function refresh(): Promise<void> {
  let entries: AppletEntry[];
  try {
    entries = await fetchApplets();
  } catch {
    // Backend blip — keep the last good state and try next tick.
    return;
  }
  const nextManifest = new Map<string, Map<string, string>>();
  const nextGallery: AppletGalleryEntry[] = [];
  const nextErrors = new Map<string, string>();
  for (const e of entries) {
    nextManifest.set(e.id, new Map(Object.entries(e.components ?? {})));
    for (const g of e.gallery ?? []) nextGallery.push(g);
    if (e.error) nextErrors.set(e.id, e.error);
  }
  // Replace only on change, so watchers don't repaint every poll tick.
  if (!sameManifest(nextManifest, appletManifest.value)) {
    // A namespace object closes over the module values it resolved, so
    // drop them whenever any hash moves and let the next compile
    // rebuild. (The modules themselves stay cached by the browser.)
    namespaceCache.clear();
    appletManifest.value = nextManifest;
  }
  if (!sameGallery(nextGallery, appletGallery.value)) {
    appletGallery.value = nextGallery;
  }
  if (
    nextErrors.size !== appletErrors.value.size ||
    [...nextErrors].some(([k, v]) => appletErrors.value.get(k) !== v)
  ) {
    appletErrors.value = nextErrors;
  }
}

/// Load the applet list once (awaitable) and start polling. Idempotent.
export function ensureApplets(): Promise<void> {
  if (!firstLoad) {
    firstLoad = refresh();
    pollTimer = setInterval(() => void refresh(), POLL_MS);
  }
  return firstLoad;
}

// id → the object injected into card scope. Keyed by id and dropped
// wholesale when any hash changes (see refresh above).
const namespaceCache = new Map<string, Record<string, unknown>>();

/// Resolve one applet id to the object card source sees.
///
/// Each component becomes a property holding the module's default
/// export — the factory itself, unwrapped. A component that fails to
/// load becomes a property that throws when called, so the failure
/// surfaces as this card's error rather than breaking every card that
/// merely mentions the applet.
async function resolveApplet(id: string): Promise<Record<string, unknown>> {
  const cached = namespaceCache.get(id);
  if (cached) return cached;
  const components = appletManifest.value.get(id);
  if (!components) {
    const err = appletErrors.value.get(id);
    throw new Error(
      err
        ? `applet "${id}" failed to load: ${err}`
        : `applet "${id}" is not configured`,
    );
  }
  const ns: Record<string, unknown> = {};
  let complete = true;
  for (const [name, hash] of components) {
    try {
      const mod = await import(/* @vite-ignore */ `/modules/${hash}`);
      const factory = mod.default;
      if (typeof factory !== "function") {
        throw new Error("module has no default-exported factory");
      }
      ns[name] = factory;
    } catch (e) {
      // A load failure becomes a throwing stub so it surfaces as this
      // card's error rather than breaking every card that merely
      // mentions the applet.
      const msg = e instanceof Error ? e.message : String(e);
      complete = false;
      ns[name] = () => {
        throw new Error(`component "${id}.${name}" failed to load: ${msg}`);
      };
    }
  }
  // Only cache a namespace whose components all loaded. Module hashes
  // are content-addressed and so never move on their own, and the
  // cache is invalidated only when one does — caching a stub would
  // therefore make one network blip permanent for the session, with no
  // path back. Leaving it uncached costs a retry per re-render, which
  // is exactly the behaviour a transient failure should have.
  if (complete) namespaceCache.set(id, ns);
  return ns;
}

/// The applet ids a piece of card source refers to. Same
/// over-approximating token scan the alias registry uses: it may flag a
/// name mentioned inside a string, which only costs an extra resolve.
export function referencedApplets(ids: Iterable<string>, referenced: Set<string>): string[] {
  const out: string[] = [];
  for (const id of ids) if (referenced.has(id)) out.push(id);
  return out;
}

/// Build the applet part of a card's evaluation scope: one entry per
/// referenced applet id, mapping to its namespace object.
export async function resolveAppletScope(
  referenced: Set<string>,
): Promise<Map<string, unknown>> {
  await ensureApplets();
  const scope = new Map<string, unknown>();
  for (const id of referencedApplets(appletManifest.value.keys(), referenced)) {
    scope.set(id, await resolveApplet(id));
  }
  return scope;
}
