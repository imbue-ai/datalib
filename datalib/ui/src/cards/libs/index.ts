import type { ViewLibs } from "../types";
import { gridView } from "./gridView";
import { documentView } from "./documentView";
import { documentPickerView } from "./documentPickerView";
import { galleryView } from "./galleryView";
import { agentSeedView } from "./agentSeedView";
import { aliasView } from "./aliasView";
import { dactalView } from "./dactalView";
import { perseusView } from "./perseusView";
import { sourceDagView } from "./sourceDagView";
import { tableView } from "./tableView";
import { sourcesView } from "./sourcesView";
import { configView } from "./configView";
import { logView } from "./logView";
import { logLineView } from "./logLineView";
import { umapView } from "./umapView";

// The names in scope when card source is evaluated (cardSource.ts).
export const viewLibs: ViewLibs = {
  gridView,
  documentView,
  documentPickerView,
  galleryView,
  agentSeedView,
  aliasView,
  dactalView,
  perseusView,
  sourceDagView,
  tableView,
  sourcesView,
  configView,
  logView,
  logLineView,
  umapView,
};

export {
  gridView,
  documentView,
  documentPickerView,
  galleryView,
  agentSeedView,
  aliasView,
  dactalView,
  perseusView,
  sourceDagView,
  tableView,
  sourcesView,
  configView,
  logView,
  logLineView,
  umapView,
};
