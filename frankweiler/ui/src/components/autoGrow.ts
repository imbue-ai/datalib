// Auto-grow a textarea to fit its (soft-wrapped) content — both while
// typing and when the bound value changes from outside (e.g. the grid
// opening a card with a long documentView source). Shared by the
// layout hosts' source boxes (MillerView, TreeView).
export function growSourceBox(el: HTMLTextAreaElement) {
  el.style.height = "auto";
  el.style.height = `${el.scrollHeight}px`;
}

export const vAutoGrow = {
  mounted: growSourceBox,
  updated: growSourceBox,
};
