// How a grid sits in its card: it fills the frame around its box,
// whatever size the card is, and takes the page's theme.
import type { GridOption } from "@slickgrid-universal/common";

export function isDarkTheme(): boolean {
  return document.documentElement.dataset.theme === "dark";
}

export function followFrame(box: HTMLElement, minHeight: number): GridOption {
  return {
    enableAutoResize: true,
    autoResize: {
      // The frame around the box, not the box: the resizer sizes the
      // box to what it measures, and a box it also measured would then
      // stop following the card. The frame is what the card sizes.
      container: box.parentElement!,
      calculateAvailableSizeBy: "container",
      resizeDetection: "container",
      autoHeight: false,
      bottomPadding: 0,
      minHeight,
    },
  };
}
