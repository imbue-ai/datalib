import type { ViewLibs } from "../types";
import { titled } from "../title";
import { gridView } from "./gridView";
import { documentView } from "./documentView";
import { documentPickerView } from "./documentPickerView";
import { galleryView } from "./galleryView";
import { aliasView } from "./aliasView";
import { dactalView } from "./dactalView";
import { perseusView } from "./perseusView";

// The names in scope when card source is evaluated (cardSource.ts).
export const viewLibs: ViewLibs = {
  gridView,
  documentView,
  documentPickerView,
  galleryView,
  aliasView,
  dactalView,
  perseusView,
};

// Helpers (not view factories) that are also in scope for card and
// alias source, so a user-defined factory can declare its own title
// with `titled("…", render)` just like the builtins do.
export const scopeHelpers = { titled } as const;

export {
  gridView,
  documentView,
  documentPickerView,
  galleryView,
  aliasView,
  dactalView,
  perseusView,
};
