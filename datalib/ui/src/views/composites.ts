// Composites: container subtrees kept under a name, to be opened again
// as a whole. The built-in ones ship with the app; the person's own are
// kept in the library (`/api/ui/state/composites`).
import { ref } from "vue";
import { fetchUiState, putUiState } from "@/api";
import { makeBox, makeCard, parseComposites, type BoxNode } from "@/views/containerTree";

const STATE_NAME = "composites";

// The Dashboard: its sections as a Page, each as tall as its content,
// solidified, so a card opened from it gets a tab of its own.
const DASHBOARD: BoxNode = makeBox(
  "dashboard",
  "page",
  [
    "syncStatusView()",
    "needsYouView()",
    "libraryView()",
    "sourcesOverviewView()",
    "latestActivityView()",
  ].map((source, i) => makeCard(`dashboard-${i}`, source)),
  { solidified: true, name: "Dashboard", template: "Dashboard" },
);

export const BUILTIN_COMPOSITES: Record<string, BoxNode> = { Dashboard: DASHBOARD };

// What the new-card gallery says about a built-in composite.
const BUILTIN_INFO: Record<string, { description: string; icon: string }> = {
  Dashboard: {
    description:
      "What needs you, how big your library is, each source's state, and the newest documents.",
    icon: "dashboard",
  },
};

// The composites the new-card gallery offers: the built-in ones first,
// then the person's own.
export function galleryComposites(): { name: string; description: string; icon: string }[] {
  const builtin = Object.keys(BUILTIN_COMPOSITES).map((name) => ({
    name,
    ...(BUILTIN_INFO[name] ?? { description: "", icon: "dashboard" }),
  }));
  const saved = Object.keys(savedComposites.value)
    .filter((name) => !isBuiltinComposite(name))
    .map((name) => ({ name, description: "A composite you saved.", icon: "dashboard" }));
  return [...builtin, ...saved];
}

export const savedComposites = ref<Record<string, BoxNode>>({});

export async function loadComposites() {
  try {
    savedComposites.value = parseComposites(await fetchUiState(STATE_NAME));
  } catch (e) {
    console.warn("could not load the saved composites", e);
  }
}

export function composite(name: string): BoxNode | undefined {
  return isBuiltinComposite(name) ? BUILTIN_COMPOSITES[name] : savedComposites.value[name];
}

export function isBuiltinComposite(name: string): boolean {
  return Object.hasOwn(BUILTIN_COMPOSITES, name);
}

// Keep `box` as the composite `name`, replacing a saved one of that
// name. A built-in name is the caller's to refuse (isBuiltinComposite).
export async function saveComposite(name: string, box: BoxNode): Promise<void> {
  const next = {
    ...savedComposites.value,
    [name]: { ...box, name, template: name, basis: null, openedBy: null, pinned: false },
  };
  await putUiState(STATE_NAME, next);
  savedComposites.value = next;
}
