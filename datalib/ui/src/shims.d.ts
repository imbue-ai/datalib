declare module "*.css";
// Vite's `import.meta.glob`, as `config/icons.ts` calls it: eager, one
// export per file. The full signature is in `vite/client`, which would
// also declare every asset module this file already does.
interface ImportMeta {
  glob<T>(pattern: string, options: { eager: true; import: "default" }): Record<string, T>;
}
declare module "*.css?inline" {
  const text: string;
  export default text;
}
declare module "*.svg" {
  const url: string;
  export default url;
}
// Brand marks are SVG wherever the vendor publishes one. YoLink does
// not — shop.yosmart.com serves its circle logo as PNG, which is also
// what it uses for its own favicon, i.e. the vendor's own choice of
// 16px representation.
declare module "*.png" {
  const url: string;
  export default url;
}
