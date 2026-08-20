// Custom components, client side.
//
// One mechanism, and the server's filesystem is the source of truth.
// `GET /api/frontend` reports one **namespace** per directory under
// `system/frontend/`: `user` for components a person or an agent wrote,
// and one per applet for the ones an applet wrote. Nothing here can
// tell the two apart, and nothing here needs to.
//
// A component is reached as `comp.<namespace>.<name>` — namespaced, not
// flat. That is what lets two applet instances both offer `channels`
// without competing for a name, and it means a user component can be
// called `gridView` without shadowing the builtin of that name.
//
// Loading leans on the browser. Component code is fetched from
// `/modules/<sha256>`, and the browser keeps at most one module
// instance per resolved URL — so byte-identical components in two
// namespaces are evaluated once, for free. There is deliberately no
// module cache here; the one cache is over the *namespace objects*
// built around those modules, which the platform knows nothing about.

import { ref, type Ref } from "vue";
import { fetchFrontend, type FrontendView, type Meta } from "@/api";

// namespace → name → metadata, as last reported.
export const frontendManifest: Ref<Map<string, Map<string, Meta>>> = ref(new Map());

// applet id → why its write failed. A configured applet whose namespace
// silently vanished would look like a config that never saved.
export const frontendErrors: Ref<Map<string, string>> = ref(new Map());

let firstLoad: Promise<void> | null = null;
const POLL_MS = 4000;

function sameManifest(
  a: Map<string, Map<string, Meta>>,
  b: Map<string, Map<string, Meta>>,
): boolean {
  if (a.size !== b.size) return false;
  for (const [ns, entries] of a) {
    const other = b.get(ns);
    if (!other || other.size !== entries.size) return false;
    for (const [name, meta] of entries) {
      if (JSON.stringify(other.get(name)) !== JSON.stringify(meta)) return false;
    }
  }
  return true;
}

async function refresh(): Promise<void> {
  let view: FrontendView;
  try {
    view = await fetchFrontend();
  } catch {
    // Backend blip — keep the last good state and try next tick.
    return;
  }
  const next = new Map<string, Map<string, Meta>>();
  for (const [ns, nsView] of Object.entries(view.namespaces ?? {})) {
    next.set(ns, new Map(Object.entries(nsView.entries ?? {})));
  }
  if (!sameManifest(next, frontendManifest.value)) {
    // A namespace object closes over the modules it resolved, so drop
    // them whenever anything moves and let the next compile rebuild.
    // (The modules themselves stay cached by the browser.)
    nsCache.clear();
    frontendManifest.value = next;
  }
  const errs = new Map(Object.entries(view.applet_errors ?? {}));
  if (
    errs.size !== frontendErrors.value.size ||
    [...errs].some(([k, v]) => frontendErrors.value.get(k) !== v)
  ) {
    frontendErrors.value = errs;
  }
}

/// Load the manifest once (awaitable) and start polling. Idempotent.
export function ensureFrontend(): Promise<void> {
  if (!firstLoad) {
    firstLoad = refresh();
    setInterval(() => void refresh(), POLL_MS);
  }
  return firstLoad;
}

// namespace → the object hung off `comp`. Dropped wholesale whenever
// the manifest moves (see refresh above).
const nsCache = new Map<string, Record<string, unknown>>();

/// Follow a rename chain (a→b→c) to its terminus within one namespace;
/// null when `name` was never renamed. Guards against cycles — the
/// tombstones are files on disk and are hand-editable.
export function followRenames(ns: string, name: string): string | null {
  const entries = frontendManifest.value.get(ns);
  if (!entries) return null;
  let cur = name;
  const seen = new Set<string>();
  for (;;) {
    const meta = entries.get(cur);
    if (!meta || !("renamed_to" in meta)) break;
    if (seen.has(cur)) break;
    seen.add(cur);
    cur = meta.renamed_to;
  }
  return cur === name ? null : cur;
}

/// Build one namespace's object: each component name mapped to the
/// module's default export, which is the factory card source calls.
///
/// A component that fails to load becomes a property that throws when
/// called, so the failure surfaces as *that card's* error rather than
/// breaking every card which merely mentions the namespace.
async function resolveNamespace(ns: string): Promise<Record<string, unknown>> {
  const cached = nsCache.get(ns);
  if (cached) return cached;
  const entries = frontendManifest.value.get(ns);
  if (!entries) {
    const err = frontendErrors.value.get(ns);
    throw new Error(
      err ? `namespace "${ns}" failed to load: ${err}` : `no namespace "${ns}"`,
    );
  }
  const obj: Record<string, unknown> = {};
  let complete = true;
  for (const [name, meta] of entries) {
    if ("renamed_to" in meta) continue;
    try {
      const mod = await import(/* @vite-ignore */ `/modules/${meta.component_hash}`);
      const factory = mod.default;
      if (typeof factory !== "function") {
        throw new Error("module has no default-exported factory");
      }
      obj[name] = factory;
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      complete = false;
      obj[name] = () => {
        throw new Error(`component "comp.${ns}.${name}" failed to load: ${msg}`);
      };
    }
  }
  // Only cache a namespace whose components all loaded. Hashes are
  // content-addressed and so never move on their own, and the cache is
  // invalidated only when one does — caching a failure stub would make
  // one network blip permanent for the session, with no path back.
  if (complete) nsCache.set(ns, obj);
  return obj;
}

/// The `comp.<ns>.<name>` references a piece of card source makes.
///
/// A deliberately over-approximating scan, in the same spirit as the
/// identifier scanner: it reads through strings and comments, so it may
/// resolve a namespace that is only mentioned. That costs an extra
/// import; missing a real reference would break the card, so the bias
/// runs this way on purpose.
///
/// Only the dotted form is recognized. `comp["user"]["x"]` is legal
/// JavaScript and will not be pre-resolved — the gallery always writes
/// the dotted form, and a hand-written card can too.
export function referencedNamespaces(source: string): Set<string> {
  const out = new Set<string>();
  const re = /\bcomp\s*\.\s*([A-Za-z_$][A-Za-z0-9_$]*)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(source)) !== null) out.add(m[1]);
  return out;
}

/// Build the `comp` object a card's source should see: one property per
/// namespace it references. Namespaces it does not mention are not
/// loaded, so a card costs only the components it actually uses.
export async function resolveCompScope(
  source: string,
): Promise<Record<string, unknown>> {
  await ensureFrontend();
  const comp: Record<string, unknown> = {};
  for (const ns of referencedNamespaces(source)) {
    if (!frontendManifest.value.has(ns)) continue;
    comp[ns] = await resolveNamespace(ns);
  }
  return comp;
}

/// The card source a gallery entry expands to: the fully qualified name
/// plus its stored arguments, serialized as JSON literals.
///
/// This is the whole reason arguments are *data* in the metadata rather
/// than a pre-rendered string — the store holds what to pass, and the
/// one place that knows how to spell a call builds it.
export function gallerySource(
  ns: string,
  name: string,
  args: unknown[] = [],
): string {
  const call = args.map((a) => JSON.stringify(a)).join(", ");
  return `comp.${ns}.${name}(${call})`;
}

/// Pick a name not currently taken in the `user` namespace. Valid as a
/// JavaScript identifier, since it is written as `comp.user.<name>`,
/// and prefixed so it cannot look like something a person chose.
export function freshUserName(): string {
  const taken = frontendManifest.value.get(USER_NAMESPACE) ?? new Map();
  for (;;) {
    const buf = new Uint32Array(1);
    crypto.getRandomValues(buf);
    const name = `card_${buf[0].toString(36)}`;
    if (!taken.has(name)) return name;
  }
}

/// Fold a just-written `user` component into the local manifest without
/// waiting for the next poll.
///
/// The caller has the authoritative name and hash straight from the PUT
/// response. Without this, a card repointed at the new component
/// compiles against a manifest that does not know it yet — a blank or
/// error flash until the poll lands.
export function noteUserComponent(name: string, meta: Meta): void {
  const next = new Map(frontendManifest.value);
  const user = new Map(next.get(USER_NAMESPACE) ?? []);
  user.set(name, meta);
  next.set(USER_NAMESPACE, user);
  // A namespace object caches the components it resolved, so it has to
  // be rebuilt around the new one.
  nsCache.delete(USER_NAMESPACE);
  frontendManifest.value = next;
}

/// The namespace holding hand- and agent-authored components. The one
/// namespace a refresh never deletes, and therefore the only one worth
/// handing to an agent to edit.
export const USER_NAMESPACE = "user";
