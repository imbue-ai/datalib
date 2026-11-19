# vscode-tree: VS Code's tree as a `<vs-tree>` element

VS Code's `AsyncDataTree` — the tree behind its Explorer — wrapped as a
custom element a page can drop in. It loads a node's children only when
the node is opened, draws only the rows on screen (a level of 2,000
nodes is a few dozen elements), keeps the ancestors of what you are
looking at pinned at the top (sticky scroll), and moves the focus as
you type a label. Which nodes are open, what is selected and how far
it is scrolled come back out as plain data keyed by node id, and go
back in on the next mount.

Nothing in datalib uses it yet. It is built from this directory alone,
with its own lockfile and nothing from the rest of the tree, so that it
can move to a repository of its own.

## Where the code comes from

The tree's code is **not checked in**. It is VS Code's own TypeScript
(MIT, Microsoft), fetched by Bazel at the commit of the 1.145.0 release
(`vscode_source` in `MODULE.bazel`; `vscode_source.BUILD.bazel` names
the files read: the `vs/base` layer and `vs/nls.ts`). Bazel fetches it
with git rather than as GitHub's tarball, because a cloud session's
proxy serves git reads of a public repository but not its archive
downloads; the commit hash is the pin.

`build.mjs` bundles the part the tree imports, about 155 modules and 13
stylesheets, with `src/vs-tree.js` into one ES module,
`dist/vs-tree.js`: 727 KB, 204 KB gzipped. esbuild compiles the
TypeScript with the two options VS Code's own build depends on, legacy
decorators and class fields that are assigned rather than defined.
Beside the bundle, `dist/vs-tree.LICENSE.txt` holds VS Code's license
and those of the two libraries VS Code keeps in `vs/base` and the bundle
includes, DOMPurify (Apache-2.0 or MPL-2.0) and marked (MIT).

`package.json` and `pnpm-lock.yaml` pin only the tools: esbuild, and the
Playwright the facts test drives.

To move to another VS Code release, change the commit in
`MODULE.bazel`, run the facts test, and follow §"Facts".

## Using it

```js
import 'vs-tree.js';

const tree = document.createElement('vs-tree');
tree.viewState = saved;                 // optional, from an earlier `tree.viewState`
tree.dataSource = {
  // null asks for the top level
  async getChildren(node) {
    return [{ id, label, kind, description, hasChildren }];
  },
};
tree.addEventListener('select', (e) => e.detail.nodes);
tree.addEventListener('viewstatechange', () => save(tree.viewState));
```

`viewState` is `{ expanded, focus, selection, scrollTop }`, the first
three lists of node ids. The element sizes itself to its box, so give
it one. Its colours are VS Code's light theme; `--vs-tree-fg`,
`--vs-tree-bg`, `--vs-tree-hover` and `--vs-tree-focus` change them.

## Build and test

```sh
bazelisk build //third-party/vscode-tree:vs_tree      # dist/vs-tree.js
bazelisk test //third-party/vscode-tree:facts_test
```

The test runs the facts below in headless Chromium, in about five
seconds. It uses the browser Playwright already installed for the e2e
suite: the `playwright` pin here is the one `datalib/ui`'s lockfile
resolves (`//tools:version_pins_test` holds them together), CI passes
the devcontainer's browser cache to every test, and locally Playwright
looks in its usual cache. With no browser for that version the test
says so and stops. Install one with
`bazelisk run //third-party/vscode-tree:playwright -- install chromium`,
or set `DATALIB_TEST_CHROMIUM` to a Chromium binary.

## Facts

**The facts are tests**: one each in `tests/facts.test.mjs`, under the
name given here. Checked against VS Code 1.145.0. After a bump,
run the test: a fact that moved fails by name. Fix it here, then the
test, then whatever in `src/vs-tree.js` leaned on it.

### Upstream, as VS Code's source has it

- `view_state_keeps_what_a_closed_parent_hides` — `getViewState()`
  lists every open node by id, including one whose parent has since
  been closed.
- `a_restore_skips_what_a_closed_parent_hides` — but
  `setInput(input, viewState)` reopens only the nodes it loads, so one
  under a closed parent is dropped, and stays closed when the parent is
  opened later. The element keeps such ids, reports them in its
  `viewState`, and reopens each when its parent opens.
- `default_colours_are_dark` — `unthemedListStyles` is VS Code's dark
  theme (hover `#2A2D2E`, selection `#0E639C`), so the element passes
  its own.
- `a_click_opens_a_row_only_on_its_twistie` — `expandOnlyOnTwistieClick`
  defaults to true: a click on the label selects and does not open. The
  element turns it off.
- `fuzzy_type_navigation_needs_the_option` — typing moves the focus to
  a label the keys are a prefix of; to match inside a label ("ten" to
  `#ten-forward`) it needs `typeNavigationEnabled: true`, which the
  element sets.
- `twistie_is_a_codicon_glyph` — the arrow is a glyph of VS Code's icon
  font, and without that font it has no size. A font declared inside a
  shadow root is never loaded, so the element draws the arrow in CSS.
- `styles_stay_in_the_trees_own_element` — the stylesheet the tree
  writes for itself goes inside its own element, not the document's
  `<head>`, so it works inside a shadow root. Its static CSS still has
  to be put there (`build.mjs`).
- `page_down_throws_upstream` — PageDown's handler starts an async step
  that clears the focus, does not wait for it, then reveals the cleared
  focus: an uncaught `TypeError` once the focus is on a page's last
  row. The element handles PageUp and PageDown itself and waits.

### What `<vs-tree>` does

- `children_load_a_level_at_a_time` — mounting asks for the top level
  only; opening a node asks for that node's children and nothing else.
- `a_wide_level_draws_only_what_shows` — a level of 2,000 nodes draws
  fewer than 60 rows.
- `type_to_navigate_moves_focus` — "sick" moves the focus to
  `#sickbay`.
- `page_down_pages_without_throwing` — PageDown moves the focus a page
  at a time, keeps it on screen, and throws nothing.
- `sticky_scroll_pins_the_ancestors` — scrolled into the 2,000-node
  level, its three ancestors stay pinned at the top, in order.
- `a_restore_reopens_under_a_closed_parent` — a node that was open
  under a closed parent is still in `viewState` after a remount, and
  opening the parent brings it back open.
- `view_state_round_trips` — the open nodes, the selection and the
  scroll position read from one mount come back on the next, which asks
  once for the top level and once for each open node.
