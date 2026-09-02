// The one node API the e2e specs need, declared rather than imported.
//
// `tsconfig.json`'s `types` is deliberately narrow (`vitest/globals`
// only) and `@types/node` is not a dependency, so a spec that imports
// `node:fs` would not typecheck under `//datalib/ui:typecheck_test`.
// Other specs sidestep this with a local `declare const process`; a
// module needs an ambient declaration instead.
//
// Deliberately minimal: add a signature here when a spec needs one, so
// what the suite reaches for outside the browser stays enumerable in
// one place.
declare module "node:fs" {
  export function copyFileSync(src: string, dest: string): void;
  export function readdirSync(dir: string): string[];
  export function readFileSync(file: string, encoding: "utf8"): string;
}
