// "Choose folder again" on a source the OS refused: the path field it
// points the picker at, and the one key a new choice rewrites.

import { describe, expect, it } from "vitest";

import type { Action } from "@/api";
import { CHOOSE_AGAIN, chooseAgainButton, chooseAgainFor, withStepPath } from "./chooseAgain";
import { listSteps } from "./sourceSteps";

const BACKUP = "/Users/me/Dropbox/backups/WhatsApp";

const CONFIG = `# my sources
[[groups]]
id = "whatsapp"
name = "WhatsApp"
type = "whatsapp"

[[steps]]
group = "whatsapp"
function = "ingest"
[steps.params.backup]
path = "${BACKUP}"   # the old phone
key_env_var = "WHATSAPP_BACKUP_DECRYPTION_KEY"

[[steps]]
group = "whatsapp"
function = "render_markdown"
inputs = ["whatsapp/ingest"]
`;

const action: Action = {
  id: CHOOSE_AGAIN,
  label: "Choose folder again",
  enabled: true,
  hint: null,
  disabled_reason: null,
};

describe("chooseAgainFor", () => {
  it("points at the path field the step's settings fill", () => {
    const found = chooseAgainFor("whatsapp", { backup: { path: BACKUP } });
    expect(found?.field.target).toBe("backup.path");
    expect(found?.current).toBe(BACKUP);
  });

  it("is nothing for a step that names no path, or a type with no path field", () => {
    expect(chooseAgainFor("whatsapp", { backup: {} })).toBeNull();
    expect(chooseAgainFor("slack", { api: {} })).toBeNull();
    expect(chooseAgainFor(null, {})).toBeNull();
  });
});

describe("chooseAgainButton", () => {
  const found = chooseAgainFor("whatsapp", { backup: { path: BACKUP } });

  it("is offered only where there is a picker and a field to point it at", () => {
    expect(chooseAgainButton(action, found, true)).toEqual(action);
    expect(chooseAgainButton(action, found, false)).toBeNull();
    expect(chooseAgainButton(action, null, true)).toBeNull();
  });
});

describe("withStepPath", () => {
  it("rewrites the one key and leaves the rest of the file as written", () => {
    const moved = "/Users/me/Backups/WhatsApp";
    const next = withStepPath(CONFIG, "whatsapp/ingest", "backup.path", moved);
    expect(next).toBe(CONFIG.replace(`path = "${BACKUP}"`, `path = "${moved}"`));
    const ingest = listSteps(next).find((s) => s.id === "whatsapp/ingest")!;
    expect(ingest.params).toEqual({
      backup: { path: moved, key_env_var: "WHATSAPP_BACKUP_DECRYPTION_KEY" },
    });
  });

  it("refuses a step the config does not have", () => {
    expect(() => withStepPath(CONFIG, "gone/ingest", "backup.path", "/x")).toThrow(/no step/);
  });
});
