import { flushPromises, mount } from "@vue/test-utils";
import { describe, expect, it, vi } from "vitest";

import type { ContactEdit } from "./contactEdit";
import type { NormalizedContact } from "./contacts";

const contactsApp = vi.hoisted(() => ({
  setHandleLabel: vi.fn(async () => {}),
}));

vi.mock("@/live", async (actual) => ({
  ...(await actual<typeof import("@/live")>()),
  subscribeLive: () => () => {},
}));

vi.mock("./contacts", async (actual) => {
  const real = await actual<typeof import("./contacts")>();
  const edit: ContactEdit = { name: "Geordi La Forge", note: null, fields: [] };
  const view = { base: edit, mine: edit, published: edit, published_commit: "c1" };
  return {
    ...real,
    publishedEdit: async () => edit,
    draftOf: async () => null,
    openDraft: async () => view,
    autosaveDraft: async () => ({}),
    people: { subscribe: () => () => {}, ask: async () => {}, get: () => undefined },
    setHandleLabel: contactsApp.setHandleLabel,
  };
});

import ContactEditor from "./ContactEditor.ce.vue";

const linked: NormalizedContact["handles"] = [
  {
    medium: "email",
    label: null,
    value: "glaforge@enterprise.starfleet",
    handle: "email:glaforge@enterprise.starfleet",
    stopped_working_by: null,
  },
];

describe("ContactEditor", () => {
  /** The handle's label input took its value from the prop alone, so any
   *  re-render before the blur (a contacts answer landing just after
   *  Edit) wrote the old label back over what was typed, and the label
   *  that went out was the old one, or none at all. */
  it("keeps a typed handle label through a re-render before the blur", async () => {
    const editor = mount(ContactEditor, {
      props: {
        contactId: "c-geordi",
        photoUrl: null,
        linked,
        sources: [],
        sourceName: (id: string) => id,
      },
    });
    await flushPromises();
    const edit = editor.findAll("button").find((b) => b.text() === "Edit");
    await edit!.trigger("click");
    await flushPromises();
    const label = editor.get<HTMLInputElement>(
      'input[aria-label="Label for glaforge@enterprise.starfleet"]',
    );

    label.element.value = "old";
    await label.trigger("input");
    await editor.setProps({ linked: linked.map((h) => ({ ...h })) });
    expect(label.element.value).toBe("old");

    await label.trigger("change");
    await flushPromises();
    expect(contactsApp.setHandleLabel).toHaveBeenCalledWith(
      "email:glaforge@enterprise.starfleet",
      "c-geordi",
      "old",
    );
  });
});
