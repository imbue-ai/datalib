// <vs-tree>: VS Code's AsyncDataTree, built from VS Code's own source, as a
// custom element with its own shadow root. The page hands it a data
// source of plain nodes, `{ id, label, kind?, description?, hasChildren }`;
// the element keeps VS Code's renderer and delegate plumbing to itself and
// says what is open by id, so a page can save and restore it. It fires
// `select` as the selection moves and `open` on a double-click or Enter.
// What it relies on upstream is in README.md §"Facts", each a test.

import { AsyncDataTree } from 'vscode/src/vs/base/browser/ui/tree/asyncDataTree.js';
import { unthemedListStyles } from 'vscode/src/vs/base/browser/ui/list/listWidget.js';
import { styles as upstreamCss } from 'css-registry';

// The facts test reaches upstream through these, as the element does.
export { AsyncDataTree, unthemedListStyles };

const ROW_HEIGHT = 22;

// VS Code's light theme for a tree, each colour a CSS variable a page can
// set; the defaults upstream are its dark one. Upstream writes each value
// into its CSS as it is, so a `var()` works wherever a colour does.
const THEMED = {
  ...unthemedListStyles,
  listFocusBackground: 'var(--vs-tree-focus-bg, #e8f0fe)',
  listFocusForeground: 'var(--vs-tree-fg, #1f2328)',
  listActiveSelectionBackground: 'var(--vs-tree-selected-bg, #0060c0)',
  listActiveSelectionForeground: 'var(--vs-tree-selected-fg, #ffffff)',
  listActiveSelectionIconForeground: 'var(--vs-tree-selected-fg, #ffffff)',
  listFocusAndSelectionBackground: 'var(--vs-tree-selected-bg, #0060c0)',
  listFocusAndSelectionForeground: 'var(--vs-tree-selected-fg, #ffffff)',
  listFocusAndSelectionOutline: 'var(--vs-tree-focus, #0090f1)',
  listFocusOutline: 'var(--vs-tree-focus, #0090f1)',
  listInactiveSelectionBackground: 'var(--vs-tree-inactive-selected-bg, #e4e6f1)',
  listInactiveSelectionIconForeground: undefined,
  listHoverBackground: 'var(--vs-tree-hover, #f0f0f0)',
  listDropOverBackground: 'var(--vs-tree-focus-bg, #d6ebff)',
  treeIndentGuidesStroke: 'var(--vs-tree-guide, #a9a9a9)',
  treeInactiveIndentGuidesStroke: 'var(--vs-tree-guide-faint, #a9a9a966)',
};

const OWN_CSS = `
:host { display: block; position: relative; contain: strict;
  font: var(--vs-tree-font, 13px/1.4 system-ui, -apple-system, "Segoe UI", sans-serif);
  color: var(--vs-tree-fg, #1f2328); background: var(--vs-tree-bg, #fff);
  --vscode-focusBorder: var(--vs-tree-focus, #0090f1);
  --vscode-sideBar-background: var(--vs-tree-bg, #fff);
  --vscode-list-hoverBackground: var(--vs-tree-hover, #f0f0f0);
  --vscode-scrollbar-shadow: #0000001f;
  --vscode-widget-border: #d0d7de; }
.frame { position: absolute; inset: 0; }
/* Upstream draws the twistie with the codicon font, and a font declared
   inside a shadow root is never loaded. */
.monaco-tl-twistie.collapsible::before { content: ""; display: inline-block;
  width: 6px; height: 6px; margin: 0 5px; border: solid currentColor;
  border-width: 0 1.5px 1.5px 0; border-radius: 0; opacity: .7;
  transform: rotate(45deg) translate(-2px, -2px); }
.monaco-tl-twistie.collapsible.collapsed::before { transform: rotate(-45deg) translate(-1px, -1px); }
.monaco-tl-twistie.codicon-tree-item-loading::before { content: ""; width: 8px; height: 8px;
  margin: 0 4px; border: 1.5px solid currentColor; border-right-color: transparent;
  border-radius: 50%; transform: none; animation: vs-tree-spin .8s linear infinite; }
@keyframes vs-tree-spin { to { transform: rotate(360deg); } }
.row { display: flex; gap: 6px; align-items: baseline; white-space: nowrap; overflow: hidden; }
.label { overflow: hidden; text-overflow: ellipsis; }
.kind { opacity: .55; font-size: 11px; text-transform: uppercase; letter-spacing: .03em; }
.desc { opacity: .6; font-size: 12px; }
`;

const ROOT = Object.freeze({ id: '\u0000root', label: '' });

class Renderer {
  templateId = 'node';
  renderTemplate(container) {
    const row = container.appendChild(document.createElement('div'));
    row.className = 'row';
    const part = (cls) => {
      const el = row.appendChild(document.createElement('span'));
      el.className = cls;
      return el;
    };
    return { label: part('label'), kind: part('kind'), desc: part('desc') };
  }
  renderElement(treeNode, _index, template) {
    const node = treeNode.element;
    template.label.textContent = node.label;
    template.kind.textContent = node.kind ?? '';
    template.desc.textContent = node.description ?? '';
  }
  disposeTemplate() {}
}

export class VsTree extends HTMLElement {
  #tree;
  #frame;
  #source;
  #restoring;
  #resize;
  // Ids a restored view state had open that the tree has not reached yet:
  // nodes under a parent that was closed when the state was saved. Upstream
  // reopens only what it loads (`a_restore_skips_what_a_closed_parent_hides`),
  // so these are reopened when their parent opens, and kept in `viewState`
  // until then.
  #pending = new Set();

  constructor() {
    super();
    const root = this.attachShadow({ mode: 'open' });
    const style = root.appendChild(document.createElement('style'));
    style.textContent = upstreamCss.join('\n') + OWN_CSS;
    this.#frame = root.appendChild(document.createElement('div'));
    this.#frame.className = 'frame';
    this.#frame.addEventListener('keydown', (e) => this.#onKeyDown(e), { capture: true });
  }

  // `{ getChildren(node | null): Promise<node[]> }`; null asks for the top.
  set dataSource(source) {
    this.#source = source;
    if (this.isConnected) this.#build();
  }

  // `{ expanded: id[], focus: id[], selection: id[], scrollTop }`, by node id.
  get viewState() {
    if (!this.#tree) return this.#restoring;
    const state = this.#tree.getViewState();
    return { ...state, expanded: [...new Set([...state.expanded, ...this.#pending])] };
  }
  set viewState(state) {
    this.#restoring = state;
    if (this.#tree) this.#build();
  }

  async refresh(node) {
    await this.#tree?.updateChildren(node ?? ROOT, true);
  }

  connectedCallback() {
    this.#resize = new ResizeObserver(() => this.#tree?.layout(this.clientHeight, this.clientWidth));
    this.#resize.observe(this);
    if (this.#source && !this.#tree) this.#build();
  }

  disconnectedCallback() {
    this.#resize?.disconnect();
  }

  #build() {
    this.#tree?.dispose();
    this.#frame.replaceChildren();
    const source = this.#source;
    const tree = new AsyncDataTree(
      'vs-tree',
      this.#frame,
      { getHeight: () => ROW_HEIGHT, getTemplateId: () => 'node' },
      [new Renderer()],
      {
        hasChildren: (n) => n === ROOT || !!n.hasChildren,
        getChildren: (n) => source.getChildren(n === ROOT ? null : n),
      },
      {
        identityProvider: { getId: (n) => n.id },
        keyboardNavigationLabelProvider: { getKeyboardNavigationLabel: (n) => n.label },
        typeNavigationEnabled: true,
        expandOnlyOnTwistieClick: false,
        accessibilityProvider: {
          getAriaLabel: (n) => n.label,
          getWidgetAriaLabel: () => this.getAttribute('aria-label') ?? 'Tree',
        },
        renderIndentGuides: 'always',
        enableStickyScroll: true,
        stickyScrollMaxItemCount: 4,
        indent: 10,
      },
    );
    tree.style(THEMED);
    this.#tree = tree;

    const state = this.#restoring;
    this.#pending = new Set(state?.expanded ?? []);
    tree.onDidChangeCollapseState((e) => {
      const node = e.node.element?.element;
      if (node && node !== ROOT && !e.node.collapsed) this.#reopenUnder(tree, node);
      this.#emit('viewstatechange');
    });
    tree.onDidChangeSelection((e) => this.#emit('select', { nodes: e.elements }));
    tree.onMouseDblClick((e) => e.element && this.#emit('open', { node: e.element }));
    tree.onDidScroll(() => this.#emit('viewstatechange'));

    tree.layout(this.clientHeight, this.clientWidth);
    tree.setInput(ROOT, state).then(() => {
      this.#reopenUnder(tree, ROOT);
      this.#emit('ready');
    });
  }

  // Once `parent`'s children are loaded and drawn, reopens those the restored
  // state had open; each one that opens comes back here for its own.
  async #reopenUnder(tree, parent) {
    if (this.#pending.size === 0) return;
    if (parent !== ROOT) await tree.expand(parent);
    if (tree !== this.#tree) return;
    for (const child of tree.getNode(parent).children) {
      const node = child.element;
      if (!this.#pending.delete(node.id)) continue;
      if (child.collapsed) tree.expand(node);
    }
  }

  // Upstream's PageUp/PageDown handler does not wait for the async page
  // step it starts, then reveals the focus that step has just cleared
  // (README.md §"Facts", `page_down_throws_upstream`). This waits for it.
  #onKeyDown(e) {
    if (!this.#tree) return;
    if (e.key === 'Enter') {
      const [node] = this.#tree.getFocus();
      if (node) this.#emit('open', { node });
      return;
    }
    if (e.key !== 'PageDown' && e.key !== 'PageUp') return;
    e.preventDefault();
    e.stopPropagation();
    this.#page(e);
  }

  async #page(e) {
    // AsyncDataTree keeps the ObjectTree it drives here; the paging lives on it.
    const inner = this.#tree.tree;
    if (e.key === 'PageDown') await inner.focusNextPage(e);
    else await inner.focusPreviousPage(e);
    const [focused] = inner.getFocus();
    if (focused) inner.reveal(focused);
  }

  #emit(type, detail) {
    this.dispatchEvent(new CustomEvent(type, { detail, bubbles: true, composed: true }));
  }
}

customElements.define('vs-tree', VsTree);
