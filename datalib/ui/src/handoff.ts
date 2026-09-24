// Hand work off to a coding agent via a copy-pasteable "wayfinder"
// prompt. The card flows are layout-agnostic: everything works through
// the card's HostCommands (setSource), so the miller, tree and tiling
// layouts all get it for free — nothing here knows which layout is
// active.
import { ref, watch } from "vue";
import { freshUserName, noteUserComponent, USER_NAMESPACE } from "@/cards/frontendRegistry";
import { encodeColumns } from "@/router/columns";
import { healthSnapshot, putLib } from "@/api";
import { pushToast } from "@/toasts";
import type { HostCommands } from "@/cards/types";

// The placeholder the new alias points at until an agent fills it in: a
// factory delegating to the agentSeedView builtin, which renders the
// hand-off instructions in the card body. A single expression so it
// satisfies the alias contract. The alias' own name appears only inside
// a string literal, which the dependency scanner deliberately tolerates
// (a component is never injected into its own scope).
function seedSource(name: string): string {
  return `() => agentSeedView(${JSON.stringify(name)})`;
}

// ---- wayfinder text --------------------------------------------------------

// Every wayfinder hands an agent a set of curl-able URLs, and the API
// requires the server's per-process token (see
// datalib/backend/http/src/auth.rs). The browser holds that token as an
// HttpOnly cookie it can't read, and we wouldn't paste it into an agent
// prompt anyway — a secret in a prompt ends up in transcripts and logs.
// So point the agent at the file instead: it runs on this machine, as
// this user, and can re-read it whenever it needs a fresh one.
function authLines(): string[] {
  const tokenFile = healthSnapshot()?.token_file ?? "<data root>/system/api-token";
  return [
    `The API needs the server's token on every call. It changes each time`,
    `the server restarts, so read it fresh:`,
    `  TOKEN=$(cat ${JSON.stringify(tokenFile)})`,
    `and send it on every request below:`,
    `  -H "Authorization: Bearer $TOKEN"`,
    ``,
  ];
}

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
    `  node datalib/ui/scripts/render.mjs '${cardUrl}' --out /tmp/card.png \\`,
    `    --token "$TOKEN"`,
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
    `Build a datalib card by defining the component \`${name}\`.`,
    ``,
    `Read the guide first: ${origin}/agent/cards.md (no token needed)`,
    ``,
    ...authLines(),
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
    `Modify the datalib component \`${name}\` — it renders a card the`,
    `user is looking at right now.`,
    ``,
    `Read the guide first: ${origin}/agent/cards.md (no token needed)`,
    ``,
    ...authLines(),
    `Fetch the current source:`,
    `  GET ${origin}/api/lib/${name}`,
    `Save the modified factory with:`,
    `  PUT ${origin}/api/lib/${name}   (JSON body {"source": "<factory source>"})`,
    ``,
    ...wayfinderTail(cardUrl),
  ].join("\n");
}

// ---- instructions dialog store ---------------------------------------------

export type AgentHandoff = {
  kind: "modify";
  // Shown under the dialog title: the component name.
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
// devMode.
function persistedFlag(key: string) {
  const flag = ref(localStorage.getItem(key) === "1");
  watch(flag, (on) => {
    localStorage.setItem(key, on ? "1" : "0");
  });
  return flag;
}
export const skipModifyInstructions = persistedFlag("datalib-agent-skip-card-instructions");

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
  const name = freshUserName();
  const source = seedSource(name);
  let entry;
  try {
    entry = await putLib(name, source);
  } catch (e) {
    pushToast(`could not create component: ${(e as Error).message}`);
    return;
  }
  // Register it locally before repointing the card, so the first
  // compile doesn't blank-flash waiting for the manifest to catch up.
  noteUserComponent(name, entry.meta);
  host.setSource(`comp.${USER_NAMESPACE}.${name}()`);
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
export function modifyComponentWithAgent(name: string, cardSource: string, state: string): void {
  handOff(
    {
      kind: "modify",
      subject: name,
      wayfinder: modifyWayfinder(name, cardSource, state),
    },
    skipModifyInstructions.value,
  );
}
