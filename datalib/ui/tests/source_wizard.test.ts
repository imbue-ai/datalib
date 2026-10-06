import { describe, expect, it } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import SourceWizard from "../src/components/SourceWizard.vue";

function open() {
  return mount(SourceWizard, { props: { takenIds: new Set<string>() } });
}

/// Advance past the type picker into the form, on the first type that
/// has a guided setup at all.
async function configureFirstType(wiz: VueWrapper) {
  const tile = wiz.findAll("button.wiz-tile").find((t) => !t.attributes("disabled"));
  expect(tile, "no catalog entry offers a wizard").toBeTruthy();
  await tile!.trigger("click");
  expect(wiz.find("input.wiz-input").exists()).toBe(true);
}

describe("SourceWizard dismissal", () => {
  /// A click that lands on the backdrop rather than on the dialog is
  /// the "click outside" gesture. It used to close the dialog, which
  /// threw away everything typed into the form with no way back.
  it("survives a click outside", async () => {
    const wiz = open();
    await configureFirstType(wiz);
    await wiz.find("input.wiz-input").setValue("half-finished");

    await wiz.find(".wiz-backdrop").trigger("click");

    expect(wiz.emitted("close")).toBeUndefined();
    expect(wiz.find(".wiz").exists()).toBe(true);
    expect((wiz.find("input.wiz-input").element as HTMLInputElement).value).toBe("half-finished");
  });

  /// The two deliberate ways out still work, which is what keeps the
  /// assertion above from passing on a dialog nothing can close.
  it("closes on the header × and on Cancel", async () => {
    const x = open();
    await x.find("button.wiz-x").trigger("click");
    expect(x.emitted("close")).toHaveLength(1);

    const cancel = open();
    const btn = cancel.findAll("button").find((b) => b.text() === "Cancel");
    await btn!.trigger("click");
    expect(cancel.emitted("close")).toHaveLength(1);
  });
});

describe("SourceWizard requiresOneOf", () => {
  /// Lightroom's catalog and backups folder are each optional, but a
  /// source with neither reads nothing, so the form holds Add until one
  /// of them is filled in and says which it wants.
  it("holds Add until one of the fields is filled, and names them", async () => {
    const wiz = open();
    const tile = wiz.findAll("button.wiz-tile").find((t) => t.text().includes("Lightroom"));
    await tile!.trigger("click");
    const add = () => wiz.findAll("button").find((b) => b.text() === "Add source")!;

    expect(add().attributes("disabled")).toBeDefined();
    expect(wiz.find(".wiz-foot-note").text()).toContain("Catalog file or Backups folder");

    const paths = wiz.findAll("input.wiz-path");
    expect(paths).toHaveLength(2);
    await paths[1].setValue("/Users/x/Pictures/Lightroom/Backups");
    expect(add().attributes("disabled")).toBeUndefined();
  });
});
