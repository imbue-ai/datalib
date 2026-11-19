# `<vs-tree>` demo

A page that shows off `//third-party/vscode-tree` — VS Code's tree as a
web component — over a made-up TNG library: folders of sources, then
accounts, labels, channels, a 10,000-folder Drive and a chain of Notion
pages eight deep.

```sh
bazelisk run //hack/vscode_tree_demo            # http://127.0.0.1:8765/
bazelisk run //hack/vscode_tree_demo -- 9000    # another port
```

The page serves the tree one level at a time behind a delay you set,
and logs each level the tree asks for, so you can see that opening a
node fetches that node's children and nothing else. Open the Drive to
see 10,000 rows drawn as a few dozen; scroll into the Notion pages to
see the ancestors pinned at the top; type a name to jump to it. What is
open, selected and scrolled is saved in the browser's `localStorage`
and comes back when you reload; "Forget saved state" clears it.
