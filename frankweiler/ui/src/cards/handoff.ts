// Hand a card off to a coding agent. Layout-agnostic: it works through
// the card's HostCommands (setSource), so both the miller and tree
// layouts get it for free — nothing here knows which layout is active.
//
// It mints a fresh component alias, seeds it with minimal demo code,
// repoints the card at `alias()` via the host, and copies a wayfinder
// snippet to the clipboard for the user to paste into an agent. As the
// agent re-saves the alias, the card live-reloads (see ShadowCard's
// manifest watcher).
import { freshAliasName } from "./aliasRegistry";
import { encodeColumns } from "@/router/columns";
import { putLib } from "@/api";
import { pushToast } from "@/toasts";
import type { HostCommands } from "./types";

// A minimal placeholder component the new alias points at until an agent
// fills it in. A single expression (factory) so it satisfies the alias
// contract; deliberately self-contained and free of the alias' own name
// (a self-reference would resolve to a dependency cycle).
function demoSeed(): string {
  return `(args) => (root) => {
  const el = document.createElement("div");
  el.style.cssText = "padding:16px;font:13px ui-monospace,monospace;opacity:.6;color:var(--fw-fg,inherit)";
  el.textContent = "new component — point an agent at this card to build it";
  root.appendChild(el);
  return () => {};
}`;
}

export async function handOffToAgent(host: HostCommands): Promise<void> {
  const name = freshAliasName();
  // Always seed with minimal demo code — the hand-off starts the agent
  // from a clean component, not a copy of whatever was here before.
  try {
    await putLib(name, demoSeed());
  } catch (e) {
    pushToast(`could not create component: ${(e as Error).message}`);
    return;
  }
  host.setSource(`${name}()`);

  const origin = window.location.origin;
  // The card's standalone (single-column) URL, for the headless preview.
  // It's a miller URL regardless of the active layout — that's the
  // canonical "open this card alone" address an agent can render.
  const cardUrl = origin + encodeColumns([{ code: `${name}()`, state: "" }]);
  const wayfinder = [
    `Build a frankweiler card by defining the component \`${name}\`.`,
    ``,
    `Read the guide first: ${origin}/agent.md`,
    ``,
    `Save your factory with:`,
    `  PUT ${origin}/api/lib/${name}   (JSON body {"source": "<factory source>"})`,
    ``,
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
  ].join("\n");
  try {
    await navigator.clipboard.writeText(wayfinder);
    pushToast(`component ${name} created — wayfinder copied to clipboard`, "info");
  } catch {
    // Clipboard can be blocked (insecure origin / no focus); show it so
    // the user can copy by hand.
    pushToast(`component ${name} created. Wayfinder:\n${wayfinder}`, "warn");
  }
}
