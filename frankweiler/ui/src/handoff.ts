// Hand work off to a coding agent via a copy-pasteable "wayfinder"
// prompt. The card flows are layout-agnostic: everything works through
// the card's HostCommands (setSource), so the miller, tree and tiling
// layouts all get it for free — nothing here knows which layout is
// active.
//
// Three flows share this module:
//   - create (the gallery's "build a component with an agent" entry):
//     mint a fresh component alias seeded with the in-card hand-off
//     instructions (`agentSeedView`, which builds a wayfinder asking
//     the agent to DEFINE the alias) and repoint the card at `alias()`
//     via the host. No dialog: the card body IS the instructions until
//     the agent's first save replaces the component.
//   - modify (the 🤖 button on any card backed by a user component):
//     build a wayfinder that asks the agent to MODIFY the existing
//     alias, and either show the instructions or — once the user has
//     opted out of them — copy the wayfinder straight to the clipboard.
//   - config (the 🤖 button on the Manage tab's config editor): same
//     shape as modify, but the wayfinder targets `<root>/config.yaml`
//     through GET/PUT /api/config instead of a component alias.
//
// As the agent re-saves the alias, the card live-reloads (see
// ShadowCard's manifest watcher); the config editor polls the backend
// and reloads the same way (SourcesView).
import { ref, watch } from "vue";
import { freshAliasName, noteAlias } from "@/cards/aliasRegistry";
import { encodeColumns } from "@/router/columns";
import { putLib } from "@/api";
import { pushToast } from "@/toasts";
import type { HostCommands } from "@/cards/types";

// The placeholder the new alias points at until an agent fills it in: a
// factory delegating to the agentSeedView builtin, which renders the
// hand-off instructions in the card body. A single expression so it
// satisfies the alias contract. The alias' own name appears only inside
// a string literal, which the dependency scanner deliberately tolerates
// (see aliasRegistry.directAliasDeps' self exclusion).
function seedSource(name: string): string {
  return `() => agentSeedView(${JSON.stringify(name)})`;
}

// ---- wayfinder text --------------------------------------------------------

// The parts every wayfinder ends with: the source contract, the
// live-reload/preview loop, and the lead-in for the user's own request.
function wayfinderTail(cardUrl: string): string[] {
  return [
    `The "source" must be ONE JavaScript expression that evaluates to a`,
    `factory — e.g. (args) => (root, ctx) => { …; return () => {}; }. Do`,
    `NOT add a trailing semicolon, statements, or import/export; it is`,
    `evaluated as \`return (<source>)\`.`,
    ``,
    `The card live-reloads on every PUT. Preview it headlessly with:`,
    `  node frankweiler/ui/scripts/render.mjs '${cardUrl}' --out /tmp/card.png`,
    ``,
    `This is the user's request:`,
    ``,
  ];
}

// Exported for agentSeedView, which rebuilds the wayfinder on every
// render so it always carries the origin the card is viewed on.
export function createWayfinder(name: string): string {
  const origin = window.location.origin;
  // The card's standalone (single-column) URL, for the headless preview.
  // It's a miller URL regardless of the active layout — that's the
  // canonical "open this card alone" address an agent can render.
  const cardUrl = origin + encodeColumns([{ code: `${name}()`, state: "" }]);
  return [
    `Build a frankweiler card by defining the component \`${name}\`.`,
    ``,
    `Read the guide first: ${origin}/agent/cards.md`,
    ``,
    `Save your factory with:`,
    `  PUT ${origin}/api/lib/${name}   (JSON body {"source": "<factory source>"})`,
    ``,
    ...wayfinderTail(cardUrl),
  ].join("\n");
}

function modifyWayfinder(name: string, cardSource: string, state: string): string {
  const origin = window.location.origin;
  // Preview the card as the user sees it: current source and state, not
  // the bare `name()` invocation.
  const cardUrl = origin + encodeColumns([{ code: cardSource, state }]);
  return [
    `Modify the frankweiler component \`${name}\` — it renders a card the`,
    `user is looking at right now.`,
    ``,
    `Read the guide first: ${origin}/agent/cards.md`,
    ``,
    `Fetch the current source:`,
    `  GET ${origin}/api/lib/${name}`,
    `Save the modified factory with:`,
    `  PUT ${origin}/api/lib/${name}   (JSON body {"source": "<factory source>"})`,
    ``,
    ...wayfinderTail(cardUrl),
  ].join("\n");
}

function configWayfinder(configPath: string): string {
  const origin = window.location.origin;
  return [
    `Modify the frankweiler data-source config — the user has its editor`,
    `open in the Manage tab right now.`,
    ``,
    `Read the guide first: ${origin}/agent/config.md`,
    ``,
    `Fetch the current config:`,
    `  GET ${origin}/api/config   → {"yaml": "<current text>", …}`,
    `Save the modified config with:`,
    `  PUT ${origin}/api/config   (JSON body {"yaml": "<full new text>"})`,
    ``,
    `PUT validates with the real config loader before writing anything;`,
    `an invalid config comes back as {"ok": false, "error": "…"} and the`,
    `file is left untouched — fix and re-PUT. The file on disk is`,
    `${configPath} if you want to look at it directly, but save through`,
    `the PUT so validation runs. The user's editor reloads automatically`,
    `after every successful save.`,
    ``,
    `A step's \`command:\` can run any program, including new ones you`,
    `write. Install such a program (binary or symlink) into`,
    `~/.datalib/bin — that dir is prepended to PATH when the pipeline`,
    `runs. Details in the guide.`,
    ``,
    `This is the user's request:`,
    ``,
  ].join("\n");
}

// ---- instructions dialog store ---------------------------------------------

export type AgentHandoff = {
  kind: "modify" | "config";
  // Shown under the dialog title: the component name, or the config
  // file path for the config kind.
  subject: string;
  wayfinder: string;
};

// The hand-off whose instructions are currently shown; null = closed.
// Rendered by AgentHandoffModal (mounted once in App). A plain
// module-level ref, same pattern as toasts.ts, so card libs (vanilla
// DOM, no Vue tree) can open it too.
export const pendingHandoff = ref<AgentHandoff | null>(null);

export function dismissHandoff(): void {
  pendingHandoff.value = null;
}

// "Don't show the instructions again": once set, the corresponding 🤖
// button copies the wayfinder immediately. Persisted per browser like
// devMode; one flag per surface (cards / config editor) so opting out
// on one doesn't silently mute the other.
function persistedFlag(key: string) {
  const flag = ref(localStorage.getItem(key) === "1");
  watch(flag, (on) => {
    localStorage.setItem(key, on ? "1" : "0");
  });
  return flag;
}
export const skipModifyInstructions = persistedFlag(
  "fw-agent-skip-card-instructions",
);
export const skipConfigInstructions = persistedFlag(
  "fw-agent-skip-config-instructions",
);

// Copy a wayfinder to the clipboard. Clipboard can be blocked (insecure
// origin / no focus); fall back to showing the text so the user can
// copy by hand. Returns whether the copy landed.
export async function copyWayfinder(wayfinder: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(wayfinder);
    return true;
  } catch {
    pushToast(`couldn't copy — here's the prompt:\n${wayfinder}`, "warn");
    return false;
  }
}

// ---- entry points ----------------------------------------------------------

// The gallery's "build a component with an agent" entry: mint a fresh
// alias seeded with the in-card instructions and repoint the card at
// it — the card body walks the user through the hand-off from there.
export async function createComponentWithAgent(host: HostCommands): Promise<void> {
  const name = freshAliasName();
  const source = seedSource(name);
  let hash: string;
  try {
    hash = (await putLib(name, source)).hash;
  } catch (e) {
    pushToast(`could not create component: ${(e as Error).message}`);
    return;
  }
  // Register the new alias locally before repointing the card, so the
  // first compile doesn't blank-flash waiting for the manifest poll.
  noteAlias(name, hash, source);
  host.setSource(`${name}()`);
}

// Shared modify-flavored entry: show the instructions, unless the
// surface's skip flag says to copy the wayfinder immediately.
function handOff(handoff: AgentHandoff, skip: boolean): void {
  if (!skip) {
    pendingHandoff.value = handoff;
    return;
  }
  void copyWayfinder(handoff.wayfinder).then((ok) => {
    if (ok) {
      pushToast("prompt copied — paste it into your agent, then add your request", "info");
    }
  });
}

// The 🤖 button on a card backed by the user component `name`: hand the
// existing component to an agent for modification.
export function modifyComponentWithAgent(
  name: string,
  cardSource: string,
  state: string,
): void {
  handOff(
    {
      kind: "modify",
      subject: name,
      wayfinder: modifyWayfinder(name, cardSource, state),
    },
    skipModifyInstructions.value,
  );
}

// The 🤖 button on the Manage tab's config editor: hand the config file
// to an agent for modification.
export function modifyConfigWithAgent(configPath: string): void {
  handOff(
    {
      kind: "config",
      subject: configPath,
      wayfinder: configWayfinder(configPath),
    },
    skipConfigInstructions.value,
  );
}
