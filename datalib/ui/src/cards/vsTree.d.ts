// `<vs-tree>`, VS Code's tree as a custom element
// (third-party/vscode-tree/README.md). Vite resolves "vs-tree" to the
// bundle Bazel builds (vite.config.ts); these are the parts the app uses.
declare module "vs-tree" {
  export type VsTreeNode = {
    id: string;
    label: string;
    icon?: string;
    kind?: string;
    description?: string;
    hasChildren: boolean;
  };
  export type VsTreeViewState = {
    expanded?: string[];
    focus?: string[];
    selection?: string[];
    scrollTop?: number;
  };
  export class VsTree extends HTMLElement {
    dataSource: { getChildren(node: VsTreeNode | null): Promise<VsTreeNode[]> };
    viewState: VsTreeViewState | undefined;
    refresh(node?: VsTreeNode): Promise<void>;
    /** Fills a row's label element in place of its text. */
    renderLabel: ((node: VsTreeNode, el: HTMLElement) => void) | null;
    /** CSS for what `renderLabel` draws, inside the element's shadow root. */
    extraCss: string;
    /** Draws every loaded row again. */
    redraw(): void;
  }
}
