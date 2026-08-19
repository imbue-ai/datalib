// Asserts that the Node ABI the rules_js toolchain provides matches the
// `node-v<abi>` prebuilt pinned for better-sqlite3 in MODULE.bazel.
//
// Why this test exists: better-sqlite3's native binding is the one file
// in the qmd tree that does not come from the pnpm lockfile. We fetch it
// as a sha256-pinned http_archive whose URL embeds Node's
// NODE_MODULE_VERSION. That number is a property of the Node binary, not
// of better-sqlite3, so a rules_nodejs bump silently invalidates the
// pins — `require()` then fails at qmd startup with a mismatched-ABI
// error, deep inside a genrule, which is a miserable place to learn it.
//
// Comparing the two here turns that into a named test failure that says
// exactly which assets to re-pin.
const fs = require("node:fs");

const moduleBazel = fs.readFileSync(process.env.MODULE_BAZEL, "utf8");

const pinned = [...moduleBazel.matchAll(/better-sqlite3-v[\d.]+-node-v(\d+)-/g)].map(
  (m) => m[1],
);
if (pinned.length === 0) {
  console.error("no better-sqlite3 prebuilt URLs found in MODULE.bazel");
  process.exit(1);
}

const distinct = [...new Set(pinned)];
if (distinct.length !== 1) {
  console.error(`better-sqlite3 prebuilts pin mixed ABIs: ${distinct.join(", ")}`);
  process.exit(1);
}

const actual = process.versions.modules;
if (distinct[0] !== actual) {
  console.error(
    `Node ABI mismatch.\n` +
      `  rules_js Node ${process.version} has NODE_MODULE_VERSION ${actual}\n` +
      `  MODULE.bazel pins better-sqlite3 prebuilts for node-v${distinct[0]}\n\n` +
      `Re-pin the four better_sqlite3_prebuilt_* http_archives in MODULE.bazel\n` +
      `to the node-v${actual} assets of the same better-sqlite3 release, and\n` +
      `update their sha256s.`,
  );
  process.exit(1);
}

console.log(
  `Node ${process.version} (ABI ${actual}) matches the pinned better-sqlite3 prebuilts.`,
);
