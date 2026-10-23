// What adding, updating and removing a package does to config.toml, and
// the catalog entry a package's source becomes.

import { describe, expect, it } from "vitest";

import type { PackageLookup, PackageManifest } from "@/api";
import {
  addPackage,
  entriesOfManifest,
  packageIdFor,
  removePackage,
  updatePackage,
} from "./packages";
import { buildSource, listPackages, listSteps } from "./sourceSteps";

const SHA = "a".repeat(64);
const SHA2 = "b".repeat(64);

const MANIFEST: PackageManifest = {
  schema: 1,
  name: "Weather",
  version: "1.2.0",
  description: "Hourly observations from a NOAA station.",
  artifacts: { any: { url: "weather.tar.gz", sha256: SHA } },
  sources: [
    {
      type: "weather",
      commands: { ingest: "bin/fetch-weather" },
      wizard: {
        label: "Weather station",
        fields: [{ kind: "text", target: "station", label: "Station", required: true }],
        sections: [{ heading: "Which station?", fields: ["station"] }],
      },
    },
  ],
  applets: [{ id: "weather_view", command: "bin/weather-view" }],
};

function lookup(sha = SHA, manifest = MANIFEST): PackageLookup {
  return {
    url: "https://github.com/alice/datalib-weather/releases/download/v1.2.0/datalib-package.toml",
    sha256: sha,
    manifest,
    target: "aarch64-apple-darwin",
    artifact: ["any", manifest.artifacts.any],
  };
}

const CONFIG = `log_level = "info"

# ── slack ──
[[groups]]
id = "slack"
type = "slack"

[[steps]]
group = "slack"
function = "ingest"
`;

describe("addPackage", () => {
  it("writes the pin above the first table, and the package's applets", () => {
    const text = addPackage(CONFIG, {
      id: "weather",
      source: "github:alice/datalib-weather",
      lookup: lookup(),
    });
    const [pkg] = listPackages(text);
    expect(pkg).toMatchObject({
      id: "weather",
      source: "github:alice/datalib-weather",
      sha256: SHA,
    });
    expect(text.indexOf("[[packages]]")).toBeLessThan(text.indexOf("# ── slack ──"));
    expect(text.startsWith('log_level = "info"')).toBe(true);
    const applet = listSteps(text).find((s) => s.kind === "applet");
    expect(applet).toMatchObject({ id: "weather_view", package: "weather" });
  });

  it("gives a second package of the same name its own id", () => {
    const once = addPackage(CONFIG, { id: "weather", source: "x", lookup: lookup() });
    expect(packageIdFor(once, MANIFEST)).toBe("weather-2");
  });
});

describe("updatePackage and removePackage", () => {
  const added = addPackage(CONFIG, { id: "weather", source: "x", lookup: lookup() });

  it("an update moves only the pin", () => {
    const next = updatePackage(added, "weather", lookup(SHA2));
    expect(listPackages(next)[0].sha256).toBe(SHA2);
    expect(next.replace(SHA2, SHA)).toBe(added);
  });

  it("an update adds the applets a new version brings and drops the ones it lost", () => {
    const v2 = {
      ...MANIFEST,
      applets: [{ id: "weather_chart", command: "bin/weather-chart" }],
    };
    const next = updatePackage(added, "weather", lookup(SHA2, v2));
    const applets = listSteps(next).filter((s) => s.kind === "applet");
    expect(applets.map((s) => [s.id, s.package, s.command])).toEqual([
      ["weather_chart", "weather", "bin/weather-chart"],
    ]);
  });

  it("an update keeps an applet the new version still has, as it is", () => {
    const edited = added.replace(
      'command = "bin/weather-view"',
      'command = "bin/weather-view"\n[applets.params]\nzoom = 2',
    );
    const next = updatePackage(edited, "weather", lookup(SHA2));
    expect(next.replace(SHA2, SHA)).toBe(edited);
  });

  it("removing takes the package's applets with it", () => {
    const next = removePackage(added, "weather");
    expect(listPackages(next)).toEqual([]);
    expect(listSteps(next).some((s) => s.kind === "applet")).toBe(false);
    expect(listSteps(next).map((s) => s.id)).toEqual(["slack/ingest"]);
  });

  it("is refused while a source runs the package", () => {
    const entry = entriesOfManifest("weather", MANIFEST)[0];
    const src = buildSource({
      entry,
      group: "sfo",
      name: "SFO",
      values: { station: "KSFO" },
      withGroup: true,
    });
    const withSource = `${added}\n${src.groupBody}\n\n${src.stepsBody}\n`;
    expect(() => removePackage(withSource, "weather")).toThrow(/Remove sfo first/);
  });
});

describe("a package source", () => {
  const entry = entriesOfManifest("weather", MANIFEST)[0];

  it("is a catalog entry drawn by the package's own form", () => {
    expect(entry).toMatchObject({
      type: "weather",
      label: "Weather station",
      kind: "package",
      wizard: true,
      rendersDocuments: false,
      package: { id: "weather", version: "1.2.0" },
    });
  });

  it("writes steps that name the package and run its command", () => {
    const src = buildSource({
      entry,
      group: "sfo",
      name: "SFO",
      values: { station: "KSFO" },
      withGroup: true,
    });
    expect(src.renderId).toBeNull();
    const [step] = listSteps(`${src.groupBody}\n\n${src.stepsBody}`);
    expect(step).toMatchObject({ id: "sfo/ingest", package: "weather", type: "weather" });
    expect(src.stepsBody).toContain('command = "bin/fetch-weather"');
    expect(step.params).toEqual({ station: "KSFO" });
  });
});
