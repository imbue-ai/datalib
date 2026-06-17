import type { ViewLibs } from "../types";
import { gridView } from "./gridView";
import { documentView } from "./documentView";
import { aliasView } from "./aliasView";

// The names in scope when card source is evaluated (cardSource.ts).
export const viewLibs: ViewLibs = {
  gridView,
  documentView,
  aliasView,
};

export { gridView, documentView, aliasView };
