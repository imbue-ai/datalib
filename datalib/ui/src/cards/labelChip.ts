// A label as a chip: where a message is filed upstream (an email's labels
// and mailboxes), drawn beside its header. Like a date, its name is the
// whole of what it names, so there is no URI and no resolver
// (docs/dev/chips.md § "A label").
import { filterToken } from "@/grid/query";

/** The search for everything filed under `name`. */
export function labelQuery(name: string): string {
  return filterToken("label", name, false);
}

export type LabelMenuEntry = {
  id: "copy-name" | "search";
  label: string;
  separator?: boolean;
};

export function labelMenu(name: string): LabelMenuEntry[] {
  return [
    { id: "copy-name", label: `Copy “${name}”` },
    { id: "search", label: `Everything labelled ${name}`, separator: true },
  ];
}

/** Draw every `.msg-label` a render wrote under `root` as a label chip. */
export function decorateLabels(root: ParentNode): void {
  for (const el of Array.from(root.querySelectorAll<HTMLElement>("span.msg-label"))) {
    const name = el.textContent ?? "";
    if (!name) continue;
    el.classList.add("chip");
    el.dataset.label = name;
    el.draggable = true;
    el.title = `Label: ${name}`;
  }
}
