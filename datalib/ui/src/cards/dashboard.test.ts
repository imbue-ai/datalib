import { describe, expect, it } from "vitest";

import { statusTone } from "./dashboard";

describe("statusTone", () => {
  /// A queued or waiting step drew in running's blue, so a long prepare
  /// pass full of waiting steps looked like everything was running.
  it("draws waiting and queued apart from running and from warn", () => {
    expect(statusTone("running")).toBe("run");
    expect(statusTone("waiting")).toBe("wait");
    expect(statusTone("queued")).toBe("wait");
    expect(statusTone("interrupted")).toBe("warn");
    expect(statusTone("failed")).toBe("error");
  });
});
