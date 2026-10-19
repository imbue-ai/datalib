// What a Manage row's "Choose folder again" needs from the catalog and
// the config: which path a step's settings name, and the config text
// with that path changed. The server offers the button after the OS
// refused a read (`choose_again` in http/src/manage/buttons.rs); the
// clicks are `cards/rowActions.ts`'s.
import type { Action } from "@/api";
import { catalogForStep, type Field } from "./catalog";
import { fieldsFor, getParam, listSteps } from "./sourceSteps";
import { setIn } from "./tomlPatch";
import { quote } from "./tomlText";

export const CHOOSE_AGAIN = "choose_again";

export type PathField = Extract<Field, { kind: "path" }>;

export type ChooseAgain = { field: PathField; current: string };

/// The first path field the step's settings fill, by its source's
/// catalog entry; null for a type with none, or a step that names no
/// path.
export function chooseAgainFor(
  type: string | null,
  params: Record<string, unknown>,
): ChooseAgain | null {
  const entry = catalogForStep(type, params);
  if (!entry) return null;
  for (const field of fieldsFor(entry, "download")) {
    if (field.kind !== "path") continue;
    const current = getParam(params, field.target);
    if (typeof current === "string" && current) return { field, current };
  }
  return null;
}

/// The button as this host can offer it: gone without a picker or a
/// path field to point it at, and saying "file" for a file.
export function chooseAgainButton(
  action: Action,
  found: ChooseAgain | null,
  canPick: boolean,
): Action | null {
  if (!canPick || !found) return null;
  return found.field.picks === "file" ? { ...action, label: "Choose file again" } : action;
}

/// `text` with the step `stepId`'s setting at `target` set to `path`.
export function withStepPath(text: string, stepId: string, target: string, path: string): string {
  const index = listSteps(text)
    .filter((s) => s.kind === "step")
    .findIndex((s) => s.id === stepId);
  if (index < 0) throw new Error(`The config has no step ${stepId}.`);
  return setIn(text, { array: "steps", index }, ["params", ...target.split(".")], quote(path));
}
