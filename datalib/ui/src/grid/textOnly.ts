// Options every grid spreads into its own, so that what a grid draws
// from a string is text, and the grid never stops the page with a
// dialog. Row values are written by strangers; nothing of theirs may
// become markup. `//:lint_repo` refuses a grid that does not spread this.
import type { GridOption } from "@slickgrid-universal/common";
import { escapeHtml } from "@/cards/help";

export const TEXT_ONLY_GRID = {
  // A formatter's string, a column name, a group title: set as text.
  enableHtmlRendering: false,
  // The sinks that ignore the flag above (a menu slot renderer that
  // returns a string) pass through the sanitizer instead. This one lets
  // no markup through at all: the string is shown as its characters.
  sanitizer: (dirty: string) => escapeHtml(dirty),
  // The grid's default is `alert()`: a pin too wide for the box, which a
  // card link can ask for, would stop the page once per column.
  errorCallback: (message: string) => console.warn(`grid: ${message}`),
} satisfies GridOption;
