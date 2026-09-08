// The node:fs surface the e2e suite uses, declared rather than imported:
// tsconfig's `types` is deliberately narrow.
declare module "node:fs" {
  export function copyFileSync(src: string, dest: string): void;
  export function readdirSync(dir: string): string[];
  export function rmSync(
    target: string,
    options: { recursive: boolean; force: boolean },
  ): void;
  export function readFileSync(file: string, encoding: "utf8"): string;
  export function writeFileSync(file: string, data: string): void;
}
