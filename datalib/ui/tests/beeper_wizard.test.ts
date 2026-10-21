// The Beeper form. What it must get right: the folder and the media switch
// under `texts`, the table that names the method, and the network filter
// on the render step, written only when networks are chosen.
import { describe, expect, it } from "vitest";
import { CATALOG } from "../src/config/catalog";
import {
  buildStep,
  listSteps,
  paramsAreRepresentable,
  seedFieldValues,
} from "../src/config/sourceSteps";

const BEEPER = CATALOG.find((e) => e.type === "beeper")!;
const FOLDER = "~/Library/Application Support/BeeperTexts";

const step = (phase: "download" | "render", values: Record<string, unknown> = {}) =>
  buildStep({
    entry: BEEPER,
    group: "beeper",
    phase,
    inputs: phase === "render" ? ["beeper/ingest"] : undefined,
    values: { ...seedFieldValues(BEEPER), "texts.path": FOLDER, ...values },
  });

describe("Beeper", () => {
  it("writes the folder and the media switch under the table that names the method", () => {
    const body = step("download");
    expect(body).toContain("[steps.params.texts]");
    expect(body).toContain(`path = "${FOLDER}"`);
    expect(body).toContain("media = true");
  });

  it("filters networks on the render step only when some are chosen", () => {
    expect(step("render")).not.toContain("networks");
    expect(step("download", { networks: ["signal"] })).not.toContain("networks");
    expect(step("render", { networks: ["signal", "slack"] })).toContain(
      'networks = ["signal", "slack"]',
    );
  });

  it("can edit the config the examples show", () => {
    const steps = listSteps(`data_root = "~/datalib"

[[groups]]
id = "beeper"
type = "beeper"

[[steps]]
group = "beeper"
function = "ingest"
[steps.params.texts]
media = true
path = "${FOLDER}"

[[steps]]
group = "beeper"
function = "render_markdown"
inputs = ["beeper/ingest"]
[steps.params]
period = "month"
networks = ["signal", "googlechat"]
`);
    for (const s of steps) expect(paramsAreRepresentable(s, BEEPER), s.id).toEqual({ ok: true });
  });
});
