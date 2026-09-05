// The one node API the e2e specs need, declared rather than imported.
declare module "node:fs" {
  export function copyFileSync(src: string, dest: string): void;
  export function readdirSync(dir: string): string[];
  export function readFileSync(file: string, encoding: "utf8"): string;
  export function writeFileSync(file: string, data: string): void;
}
