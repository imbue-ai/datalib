// A `.ce.vue` component keeps its styles as strings for a card's shadow
// root (vueCard.ts). One drawn in the app's chrome, outside every card,
// needs them in the document instead: this puts each in once.
import type { Component } from "vue";

const adopted = new Set<string>();

export function adoptStyles(...components: Component[]): void {
  for (const c of components) {
    for (const css of (c as { styles?: string[] }).styles ?? []) {
      if (adopted.has(css)) continue;
      const style = document.createElement("style");
      style.textContent = css;
      document.head.append(style);
      adopted.add(css);
    }
  }
}
