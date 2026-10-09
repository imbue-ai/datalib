// Auto-grow a textarea to fit its (soft-wrapped) content — both while
// typing and when the bound value changes from outside (e.g. the grid
// opening a card with a long documentView source). Used by a card's
// source box in edit mode (ContainerNode).
export function growSourceBox(el: HTMLTextAreaElement) {
  el.style.height = "auto";
  el.style.height = `${el.scrollHeight}px`;
}

export const vAutoGrow = {
  mounted: growSourceBox,
  updated: growSourceBox,
};
