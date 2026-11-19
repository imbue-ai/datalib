// Bundles src/vs-tree.js and the part of VS Code's source it imports into
// one ES module. `vscode/...` in an import names a file under the VS Code
// checkout; esbuild compiles its TypeScript with the two compiler options
// VS Code's own build depends on. Each .css file upstream imports becomes a
// string in the `css-registry` module, which the element puts in its own
// shadow root: a stylesheet a bundler adds to the document never reaches
// inside one.
//
//   node build.mjs <out.js> <the checkout's LICENSE.txt>
import * as esbuild from 'esbuild';
import fs from 'node:fs';
import path from 'node:path';

const [outfile, licenseFile] = process.argv.slice(2);
if (!outfile || !licenseFile) throw new Error('usage: build.mjs <out.js> <vscode LICENSE.txt>');

// Under `js_run_binary` an input is named by its path from the execroot,
// and the build runs elsewhere; run by hand, it is a plain path.
const execroot = process.env.JS_BINARY__EXECROOT;
const license = execroot ? path.join(execroot, licenseFile) : path.resolve(licenseFile);
const vscodeRoot = path.dirname(license);

const vscodeSource = {
  name: 'vscode-source',
  setup(build) {
    build.onResolve({ filter: /^vscode\// }, (args) => ({
      path: path.join(vscodeRoot, args.path.slice('vscode/'.length).replace(/\.js$/, '.ts')),
    }));
    build.onResolve({ filter: /^css-registry$/ }, () => ({ path: 'css-registry', namespace: 'css-registry' }));
    build.onLoad({ filter: /.*/, namespace: 'css-registry' }, () => ({
      contents: 'export const styles = []; export function add(s) { styles.push(s); }',
      loader: 'js',
    }));
    build.onLoad({ filter: /\.css$/ }, (args) => ({
      contents: `import { add } from 'css-registry'; add(${JSON.stringify(fs.readFileSync(args.path, 'utf8'))});`,
      loader: 'js',
    }));
  },
};

const result = await esbuild.build({
  entryPoints: ['./src/vs-tree.js'],
  bundle: true,
  format: 'esm',
  minify: true,
  legalComments: 'eof',
  banner: {
    js:
      '/*! <vs-tree>: the tree from Visual Studio Code (https://github.com/microsoft/vscode),\n' +
      ' * Copyright (c) Microsoft Corporation, MIT License; the full text, and the licenses of\n' +
      ' * the libraries VS Code includes, are in vs-tree.LICENSE.txt. */',
  },
  // VS Code's code is written for TypeScript's legacy decorators, and for
  // class fields that are assigned rather than defined.
  tsconfigRaw: { compilerOptions: { experimentalDecorators: true, useDefineForClassFields: false } },
  outfile,
  metafile: true,
  plugins: [vscodeSource],
  logLevel: 'warning',
});

// VS Code's license, then each library VS Code keeps a license beside.
const inputs = Object.keys(result.metafile.inputs);
const bundled = new Set(inputs.map((p) => path.dirname(path.resolve(p))));
const libraryLicenses = [...bundled]
  .flatMap((dir) => fs.readdirSync(dir).filter((f) => f.endsWith('.license.txt')).map((f) => path.join(dir, f)))
  .sort();
fs.writeFileSync(
  path.join(path.dirname(outfile), 'vs-tree.LICENSE.txt'),
  [license, ...libraryLicenses]
    .map((f) => `${f === license ? 'LICENSE.txt' : f.slice(f.lastIndexOf('/src/vs/') + 1)}\n\n${fs.readFileSync(f, 'utf8').trim()}\n`)
    .join('\n\n'),
);

const css = inputs.filter((p) => p.endsWith('.css')).length;
console.log(`vs-tree: ${inputs.length - css} modules and ${css} stylesheets, ${fs.statSync(outfile).size} bytes`);
