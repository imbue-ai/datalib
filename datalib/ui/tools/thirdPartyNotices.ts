// Vite plugin: write `THIRD_PARTY_NOTICES.md` into `dist/`, naming every
// npm package whose code ended up in the bundle, with its license text.
// The Rust side embeds `dist/` into datalib-http, and
// `scripts/third_party_notices.sh` copies this file into the release
// tarball and the .app; it is what lets an MIT/BSD/Apache notice travel
// with the bytes it covers. Bundle-exact by construction: the package
// list is read off the chunks rolldown emitted, not off package.json.

import fs from "node:fs";
import path from "node:path";
import type { Plugin } from "vite";

interface PackageInfo {
  name: string;
  version: string;
  license: string;
  repository: string;
  licenseText: string;
}

const LICENSE_FILE = /^(licen[cs]e|copying|notice)(\.|$)/i;

function packageRoot(file: string): string | null {
  let dir = path.dirname(file);
  while (dir.includes("node_modules")) {
    const manifest = path.join(dir, "package.json");
    if (fs.existsSync(manifest)) {
      try {
        if (JSON.parse(fs.readFileSync(manifest, "utf8")).name) return dir;
      } catch {
        // A stray package.json inside a package's own tree; keep climbing.
      }
    }
    const parent = path.dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return null;
}

function readPackage(root: string): PackageInfo {
  const manifest = JSON.parse(fs.readFileSync(path.join(root, "package.json"), "utf8"));
  const repository =
    typeof manifest.repository === "string" ? manifest.repository : (manifest.repository?.url ?? "");
  const licenseText = fs
    .readdirSync(root)
    .filter((f) => LICENSE_FILE.test(f))
    .sort()
    .map((f) => fs.readFileSync(path.join(root, f), "utf8").trim())
    .join("\n\n");
  return {
    name: manifest.name,
    version: manifest.version ?? "",
    license: typeof manifest.license === "string" ? manifest.license : "UNKNOWN",
    repository: repository.replace(/^git\+/, "").replace(/\.git$/, ""),
    licenseText,
  };
}

export function thirdPartyNotices(): Plugin {
  return {
    name: "datalib:third-party-notices",
    apply: "build",
    generateBundle(_options, bundle) {
      const roots = new Set<string>();
      for (const output of Object.values(bundle)) {
        if (output.type !== "chunk") continue;
        for (const id of output.moduleIds) {
          if (id.startsWith("\0") || !id.includes("/node_modules/")) continue;
          const root = packageRoot(id.split("?")[0]);
          if (root) roots.add(fs.realpathSync(root));
        }
      }
      const packages = [...roots]
        .map(readPackage)
        .sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));
      const missing = packages.filter((p) => !p.licenseText && p.license === "UNKNOWN");
      if (missing.length) {
        this.error(`packages with no license: ${missing.map((p) => p.name).join(", ")}`);
      }
      const lines = [
        "# Third-party notices: UI bundle",
        "",
        "The npm packages bundled into this UI, with their license texts.",
        "",
        ...packages.map((p) => `- ${p.name} ${p.version} — ${p.license}`),
        "",
      ];
      for (const p of packages) {
        lines.push(`## ${p.name} ${p.version}`, "", `License: ${p.license}`);
        if (p.repository) lines.push(`Repository: ${p.repository}`);
        lines.push("", "```", p.licenseText || `(no license file in the package; see its package.json)`, "```", "");
      }
      this.emitFile({ type: "asset", fileName: "THIRD_PARTY_NOTICES.md", source: lines.join("\n") });
    },
  };
}
