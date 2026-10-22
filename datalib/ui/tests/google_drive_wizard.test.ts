// The Google Drive form: one account signed in to four latchkey
// services, a question about which files, and the export lists, which a
// config may also write as a table per kind the form has no field for.
import { describe, expect, it } from "vitest";
import { CATALOG } from "../src/config/catalog";
import { listed, servicesToSignIn, signInLabel, targetAccount } from "../src/config/multiSignIn";
import {
  buildStep,
  chosenAnswer,
  layoutOf,
  listSteps,
  paramsAreRepresentable,
  patchSource,
  seedFieldValues,
  sourceStepsOf,
} from "../src/config/sourceSteps";

const DRIVE = CATALOG.find((e) => e.type === "google_drive")!;
const SERVICES = ["google-drive", "google-docs", "google-sheets", "google-slides"];
const PICARD = "picard@enterprise.test";

describe("signing in to every service as one account", () => {
  it("names the four services the download reaches", () => {
    expect([DRIVE.credentialService, ...DRIVE.credentialAlso!.map((s) => s.service)]).toEqual(
      SERVICES,
    );
  });

  it("runs every login when nothing is held, and lets the first one say who", () => {
    const held = { "google-drive": [], "google-docs": [] };
    expect(targetAccount(held, "")).toBe("");
    expect(targetAccount(held, PICARD)).toBe("");
    expect(servicesToSignIn(SERVICES, held, "")).toEqual(SERVICES);
    expect(signInLabel(SERVICES, held, "")).toBe("Sign in with browser");
  });

  /// A sign-in that stopped halfway (a consent turned down on Sheets)
  /// is finished by the logins left, not by every one again.
  it("finishes a half-done sign-in with only the logins left", () => {
    const held = { "google-drive": [PICARD], "google-docs": [PICARD], "google-sheets": ["riker"] };
    expect(targetAccount(held, PICARD)).toBe(PICARD);
    expect(servicesToSignIn(SERVICES, held, PICARD)).toEqual(["google-sheets", "google-slides"]);
    expect(signInLabel(SERVICES, held, PICARD)).toBe(`Finish signing in as ${PICARD}`);
  });

  it("signs in to every one again when all of them hold the account", () => {
    const held = Object.fromEntries(SERVICES.map((s) => [s, [PICARD]]));
    expect(servicesToSignIn(SERVICES, held, PICARD)).toEqual(SERVICES);
    expect(signInLabel(SERVICES, held, PICARD)).toBe(`Sign in again as ${PICARD}`);
  });

  it("says a list of names the way a sentence does", () => {
    expect(listed(["Drive"])).toBe("Drive");
    expect(listed(["Drive", "Docs", "Sheets"])).toBe("Drive, Docs and Sheets");
  });
});

const config = (params: string) => `data_root = "~/datalib"

[[groups]]
id = "google_drive"
type = "google_drive"

[[steps]]
group = "google_drive"
function = "ingest"
${params}

[[steps]]
group = "google_drive"
function = "render_markdown"
inputs = ["google_drive/ingest"]
`;
const ingest = (text: string) => sourceStepsOf("google_drive", listSteps(text)).ingest!;
const seeded = (text: string) =>
  seedFieldValues(DRIVE, sourceStepsOf("google_drive", listSteps(text)));

describe("the form", () => {
  const toml = (values: Record<string, unknown> = {}) =>
    buildStep({
      entry: DRIVE,
      group: "google_drive",
      phase: "download",
      values: { ...seedFieldValues(DRIVE), ...values },
    });

  it("adds with every default: every file, no converted copies", () => {
    const body = toml();
    expect(body).toContain("[steps.params.api]");
    expect(body).toContain("owned_only = false");
    expect(body).not.toContain("exports");
    expect(body).not.toContain("kinds");
  });

  it("asks which files, and leaves kinds and exports to Advanced options", () => {
    const layout = layoutOf(DRIVE, true);
    expect(layout.basic.map((r) => r.heading)).toEqual(["Which files?"]);
    expect(layout.advanced.map((r) => r.heading)).toEqual([
      "Kinds of file",
      "Converted copies",
      "Converted copies of old versions",
    ]);
  });

  it("reads each answer back from what it wrote", () => {
    const answers = layoutOf(DRIVE, true).basic[0].answers!;
    const answerOf = (params: string) => chosenAnswer(answers, seeded(config(params)));
    expect(answerOf("[steps.params.api]")).toBe(0);
    expect(answerOf("[steps.params.api]\nowned_only = true")).toBe(1);
    expect(answerOf('[steps.params.api]\nfile_ids = ["1EnterpriseNCC1701D"]')).toBe(2);
  });

  it("can edit the config the minimal example shows", () => {
    expect(paramsAreRepresentable(ingest(config("[steps.params.api]")), DRIVE)).toEqual({
      ok: true,
    });
  });

  /// `exports` may be a table with a list per kind, which the list field
  /// cannot show. The box reads empty, and saving the form deleted an
  /// inline table as though the person had emptied it.
  it("keeps exports written per kind through an edit, and names them", () => {
    for (const exports of [
      'exports = { spreadsheet = ["odf"] }',
      '[steps.params.api.exports]\nspreadsheet = ["odf"]',
    ]) {
      const text = config(`[steps.params.api]\nowned_only = false\n${exports}`);
      expect(seeded(text)["api.exports"]).toEqual([]);
      expect(patchSource(text, { entry: DRIVE, group: "google_drive", values: seeded(text) })).toBe(
        text,
      );
      expect(paramsAreRepresentable(ingest(text), DRIVE)).toEqual({
        ok: false,
        unknown: ["api.exports.spreadsheet"],
      });
    }
  });
});
