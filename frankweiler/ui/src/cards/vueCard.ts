// Adapter from a Vue component to the CardRender contract. Cards are
// authored as ordinary Vue SFCs named `*.ce.vue` — the suffix makes
// @vitejs/plugin-vue compile them in custom-element mode, which
// attaches their `<style>` blocks as `component.styles` (an array of
// CSS strings) instead of injecting them into document head. The
// adapter drops those strings into a <style> inside the shadow root,
// since head styles don't pierce the shadow boundary. Components used
// as children of a card must also be `.ce.vue` and listed in
// `styleSources` so their CSS lands in the root too.
//
// Each card runs as its own Vue app; the CardCtx arrives as a `ctx`
// prop. Teardown is app.unmount().
import { createApp, h, type Component } from "vue";
import type { CardRender } from "./types";

type CardComponent = Component & { styles?: string[] };

// Minimum chrome so the Vue app fills the column and clips itself.
// The app root fills the host absolutely rather than via height:100%:
// the host is sized by flex (`.miller-col-card`'s flex: 1 1 auto), and
// WebKit (Safari + Tauri's WKWebView) resolves percentage heights
// against flex-sized boxes as `auto`, collapsing every card to its
// content height. Absolute positioning against the host sidesteps
// percentage resolution entirely; Chromium renders both forms the same.
const BASE_CSS = `
:host { display: block; height: 100%; position: relative; }
.card-app-root { position: absolute; inset: 0; overflow: hidden; }
`;

export function vueCard(
  component: CardComponent,
  props: Record<string, unknown> = {},
  opts?: { styleSources?: (CardComponent | string)[] },
): CardRender {
  return (root, ctx) => {
    const css = [BASE_CSS];
    for (const s of [component, ...(opts?.styleSources ?? [])]) {
      if (typeof s === "string") css.push(s);
      else css.push(...(s.styles ?? []));
    }
    const style = document.createElement("style");
    style.textContent = css.join("\n");
    root.appendChild(style);

    const el = document.createElement("div");
    el.className = "card-app-root";
    root.appendChild(el);

    const app = createApp({ render: () => h(component, { ...props, ctx }) });
    app.mount(el);
    return () => app.unmount();
  };
}
