// An applet's card writes a date as `<time datetime="…">`, which it
// cannot draw as a chip itself (it has none of the app's code). The app
// draws each one as the date chip a document draws, kept in the time
// view, with the same double-click, drag and right-click menu
// (docs/dev/chips.md § "A date").
import { createApp, h, watch } from "vue";
import ChipMenu from "./ChipMenu.ce.vue";
import chipCss from "./chip.css?inline";
import { decorateDates } from "./dates";
import { timeView } from "@/config/timeView";
import {
  chipDragQuery,
  chipElAt,
  chipMenuOf,
  chipOf,
  runChipAction,
  type ChipEntryId,
  type ChipHost,
} from "./chipActions";
import type { CardRender } from "./types";

const UNDRAWN = "time[datetime]:not(.chip)";

export function withDateChips(render: CardRender): CardRender {
  return (root, ctx) => {
    const style = document.createElement("style");
    style.textContent = [chipCss, ...((ChipMenu as { styles?: string[] }).styles ?? [])].join("\n");
    root.appendChild(style);

    const draw = () => decorateDates(root);
    // The card draws when its fetches land, after its render returns.
    const added = new MutationObserver(() => {
      if (root.querySelector(UNDRAWN)) draw();
    });
    added.observe(root, { childList: true, subtree: true });
    const stopView = watch(timeView, draw);

    const host: ChipHost = { openCard: (source) => void ctx.host.openCards(source) };
    const menu = chipMenu(root);
    const on = <K extends keyof HTMLElementEventMap>(
      type: K,
      handle: (ev: HTMLElementEventMap[K], el: HTMLElement) => void,
    ) => {
      const listener = (ev: Event) => {
        const el = chipElAt(ev.target);
        if (el?.tagName === "TIME") handle(ev as HTMLElementEventMap[K], el);
      };
      root.addEventListener(type, listener);
      return () => root.removeEventListener(type, listener);
    };
    const offs = [
      on("dblclick", (ev, el) => {
        const chip = chipOf(el);
        if (!chip) return;
        ev.preventDefault();
        void runChipAction("open", chip, host);
      }),
      on("dragstart", (ev, el) => {
        const chip = chipOf(el);
        const q = chip ? chipDragQuery(chip.ref) : null;
        if (q) ev.dataTransfer?.setData("text/plain", ` ${q} `);
      }),
      on("contextmenu", (ev, el) => {
        const chip = chipOf(el);
        if (!chip) return;
        ev.preventDefault();
        menu.open(chipMenuOf(chip, false), ev.clientX, ev.clientY, (id) =>
          runChipAction(id, chip, host),
        );
      }),
    ];

    const teardown = render(root, ctx);
    draw();
    return () => {
      added.disconnect();
      stopView();
      for (const off of offs) off();
      menu.close();
      teardown();
    };
  };
}

/// The one right-click menu a card's dates share, drawn in its root.
function chipMenu(root: ShadowRoot) {
  let unmount: (() => void) | null = null;
  const close = () => {
    unmount?.();
    unmount = null;
  };
  const open = (
    entries: ReturnType<typeof chipMenuOf>,
    x: number,
    y: number,
    pick: (id: ChipEntryId) => Promise<void>,
  ) => {
    close();
    const el = document.createElement("div");
    root.appendChild(el);
    const app = createApp({
      render: () =>
        h(ChipMenu, {
          entries,
          x,
          y,
          onPick: (id: string) => {
            close();
            void pick(id as ChipEntryId);
          },
          onClose: close,
        }),
    });
    app.mount(el);
    unmount = () => {
      app.unmount();
      el.remove();
    };
  };
  return { open, close };
}
