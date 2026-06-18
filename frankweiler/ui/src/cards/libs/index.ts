import type { ViewLibs } from "../types";
import { gridView } from "./gridView";
import { documentView } from "./documentView";
import { aliasView } from "./aliasView";
import { dactalView } from "./dactalView";

// The names in scope when card source is evaluated (cardSource.ts).
export const viewLibs: ViewLibs = {
  gridView,
  documentView,
  aliasView,
  dactalView,
};

export { gridView, documentView, aliasView, dactalView };
