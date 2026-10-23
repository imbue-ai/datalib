// What adding, updating and removing a package does to config.toml, and
// the catalog entry a package's source becomes.

import { describe, expect, it } from "vitest";

import type { PackageLookup, PackageManifest } from "@/api";
import {
  addPackage,
  addSourceApplets,
  entriesOfManifest,
  packageIdFor,
  removePackage,
  updatePackage,
} from "./packages";
import {
  buildSource,
  listPackages,
  listSteps,
  setTimeSeriesStep,
  type ConfiguredStep,
} from "./sourceSteps";

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
      applets: [{ id: "view", command: "bin/weather-view" }],
    },
  ],
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
  it("writes the pin above the first table, and no applet", () => {
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
    expect(listSteps(text).some((s) => s.kind === "applet")).toBe(false);
  });

  it("gives a second package of the same name its own id", () => {
    const once = addPackage(CONFIG, { id: "weather", source: "x", lookup: lookup() });
    expect(packageIdFor(once, MANIFEST)).toBe("weather-2");
  });
});

/// `text` with a Weather source `sfo` added the way the wizard adds one:
/// the group, its steps, and the applets the source brings.
function withSource(text: string, manifest = MANIFEST): string {
  const entry = entriesOfManifest("weather", manifest)[0];
  const src = buildSource({
    entry,
    group: "sfo",
    name: "SFO",
    values: { station: "KSFO" },
    withGroup: true,
  });
  const next = `${text}\n${src.groupBody}\n\n${src.stepsBody}\n`;
  return addSourceApplets(next, "sfo", "weather", entry.package!.applets);
}

const appletsOf = (text: string) =>
  listSteps(text)
    .filter((s): s is ConfiguredStep => s.kind === "applet")
    .map((s) => [s.id, s.group, s.package, s.command]);

describe("a source's applets", () => {
  it("come with the source, filed under it, told which source they serve", () => {
    const text = withSource(addPackage(CONFIG, { id: "weather", source: "x", lookup: lookup() }));
    expect(appletsOf(text)).toEqual([["sfo_view", "sfo", "weather", "bin/weather-view"]]);
    expect(text).toContain('[applets.params]\nsource = "sfo"');
  });

  it("get an identifier for an id, and a second source its own", () => {
    const once = addSourceApplets(CONFIG, "my-sfo", "weather", MANIFEST.sources[0].applets);
    const twice = addSourceApplets(once, "my-sfo", "weather", MANIFEST.sources[0].applets);
    expect(appletsOf(twice).map(([id]) => id)).toEqual(["my_sfo_view", "my_sfo_view_2"]);
  });
});

describe("updatePackage and removePackage", () => {
  const added = addPackage(CONFIG, { id: "weather", source: "x", lookup: lookup() });

  it("an update moves only the pin", () => {
    const next = updatePackage(added, "weather", lookup(SHA2));
    expect(listPackages(next)[0].sha256).toBe(SHA2);
    expect(next.replace(SHA2, SHA)).toBe(added);
  });

  it("an update gives each source the applets a new version brings, and drops the lost", () => {
    const v2 = {
      ...MANIFEST,
      sources: [
        { ...MANIFEST.sources[0], applets: [{ id: "chart", command: "bin/weather-chart" }] },
      ],
    };
    const next = updatePackage(withSource(added), "weather", lookup(SHA2, v2));
    expect(appletsOf(next)).toEqual([["sfo_chart", "sfo", "weather", "bin/weather-chart"]]);
  });

  it("an update keeps an applet the new version still has, as it is", () => {
    const edited = withSource(added).replace('source = "sfo"', 'source = "sfo"\nzoom = 2');
    const next = updatePackage(edited, "weather", lookup(SHA2));
    expect(next.replace(SHA2, SHA)).toBe(edited);
  });

  it("removing takes only the pin", () => {
    const next = removePackage(added, "weather");
    expect(listPackages(next)).toEqual([]);
    expect(listSteps(next).map((s) => s.id)).toEqual(["slack/ingest"]);
  });

  it("is refused while a source runs the package", () => {
    expect(() => removePackage(withSource(added), "weather")).toThrow(/Remove sfo first/);
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

  it("gets a time-series step running the package's program, wired into the index", () => {
    const ts = entriesOfManifest("weather", {
      ...MANIFEST,
      sources: [
        {
          ...MANIFEST.sources[0],
          commands: { ...MANIFEST.sources[0].commands, render_time_series: "bin/weather-ts" },
        },
      ],
    })[0];
    const text = setTimeSeriesStep(CONFIG, "sfo", "weather", ts.package);
    const step = listSteps(text).find((s) => s.id === "sfo/render_time_series");
    expect(step).toMatchObject({ package: "weather", command: "bin/weather-ts" });
    const index = listSteps(text).find(
      (s) => s.id === "unified_time_series_index/time_series_index",
    );
    expect(index?.inputs).toEqual(["sfo/render_time_series"]);
    expect(setTimeSeriesStep(CONFIG, "sfo", "weather", entry.package)).toBe(CONFIG);
  });
});
