// Bundles src/vs-tree.js and the part of monaco-editor it imports into one
// ES module. Each .css file upstream imports becomes a string in the
// `css-registry` module, which the element puts in its own shadow root:
// a stylesheet a bundler adds to the document never reaches inside one.
//
//   node build.mjs <out.js>
import * as esbuild from 'esbuild';
import fs from 'node:fs';

const [outfile] = process.argv.slice(2);
if (!outfile) throw new Error('usage: build.mjs <out.js>');

const cssAsStrings = {
  name: 'css-registry',
  setup(build) {
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
  outfile,
  metafile: true,
  plugins: [cssAsStrings],
  logLevel: 'warning',
});

const inputs = Object.keys(result.metafile.inputs);
const css = inputs.filter((p) => p.endsWith('.css')).length;
console.log(`vs-tree: ${inputs.length - css} modules and ${css} stylesheets, ${fs.statSync(outfile).size} bytes`);
