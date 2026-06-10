import type { ViewLibs } from "../types";
import { gridView } from "./gridView";
import { documentView } from "./documentView";

// The names in scope when card source is evaluated (cardSource.ts).
export const viewLibs: ViewLibs = {
  gridView,
  documentView,
};

export { gridView, documentView };
