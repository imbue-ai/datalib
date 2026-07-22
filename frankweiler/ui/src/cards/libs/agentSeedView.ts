// Builtin view: the body of a freshly minted, agent-bound component.
//
// The gallery's "new component, built by an agent" entry seeds the new
// alias with `() => agentSeedView("<name>")` (see handoff.ts), so until
// the agent overwrites the alias, the card itself shows the hand-off
// instructions — an ordered list with the copy-the-prompt button as
// step 1. Living in the card body (not a popup) means the instructions
// survive reloads, travel with the card's URL, and disappear exactly
// when they're obsolete: the agent's first save replaces the alias and
// the card re-renders into the real component.
//
// The wayfinder is rebuilt on every render (not baked into the stored
// seed) so it always carries the origin the card is being viewed on.
import type { CardRender } from "../types";
import { copyWayfinder, createWayfinder } from "@/handoff";

export function agentSeedView(name: string): CardRender {
  return (root, ctx) => {
    ctx.setTitle("New component");
    const style = document.createElement("style");
    style.textContent = `
      .as { font: 13px/1.5 system-ui, -apple-system, sans-serif; color: var(--fw-fg, inherit); padding: 16px; max-width: 34rem; }
      .as-title { font-weight: 600; font-size: 14px; }
      .as-name { opacity: .6; font: 11px/1.4 ui-monospace, Menlo, monospace; margin: 2px 0 10px; }
      .as-steps { margin: 0; padding-left: 1.4em; display: flex; flex-direction: column; gap: 8px; }
      .as-copy {
        margin-left: .3em; padding: 2px 10px; cursor: pointer; font-size: 12px;
        border: 1px solid var(--fw-accent, #4060c0); border-radius: 4px;
        background: var(--fw-accent, #4060c0); color: var(--fw-bg, #fff);
      }
    `;
    root.appendChild(style);

    const wrap = document.createElement("div");
    wrap.className = "as";
    root.appendChild(wrap);

    const title = document.createElement("div");
    title.className = "as-title";
    title.textContent = "Build this card with a coding agent";
    const nameLine = document.createElement("div");
    nameLine.className = "as-name";
    nameLine.textContent = `component ${name}`;
    wrap.append(title, nameLine);

    const steps = document.createElement("ol");
    steps.className = "as-steps";
    wrap.appendChild(steps);

    const step1 = document.createElement("li");
    step1.append("Copy the agent prompt:");
    const copyBtn = document.createElement("button");
    copyBtn.className = "as-copy";
    copyBtn.textContent = "copy prompt";
    let flashTimer: ReturnType<typeof setTimeout> | null = null;
    copyBtn.addEventListener("click", () => {
      void copyWayfinder(createWayfinder(name)).then((ok) => {
        if (!ok) return;
        copyBtn.textContent = "copied ✓";
        if (flashTimer) clearTimeout(flashTimer);
        flashTimer = setTimeout(() => {
          copyBtn.textContent = "copy prompt";
        }, 1500);
      });
    });
    step1.appendChild(copyBtn);

    const step2 = document.createElement("li");
    step2.textContent =
      "Paste it into a coding agent, followed by what the card should show.";
    const step3 = document.createElement("li");
    step3.textContent =
      "Keep this card open — it re-renders every time the agent saves the component.";
    steps.append(step1, step2, step3);

    return () => {
      if (flashTimer) clearTimeout(flashTimer);
    };
  };
}
