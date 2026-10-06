// latchkey refuses a browser login to a service that names its own
// accounts when `--account` names one it does not hold yet, before any
// browser opens. These keep the wizard from ever asking for that.
import { describe, expect, it } from "vitest";
import { loginAccount, nameLeftToService } from "../src/config/accountNaming";

describe("loginAccount", () => {
  it("passes a new name only where the person names the account", () => {
    expect(loginAccount("chosen", [], " riker ")).toBe("riker");
    expect(loginAccount("service", [], "riker")).toBe("");
  });

  it("names a stored account either way, which refreshes it", () => {
    expect(loginAccount("service", ["picard@enterprise.test"], "picard@enterprise.test")).toBe(
      "picard@enterprise.test",
    );
    expect(loginAccount("chosen", ["work"], "work")).toBe("work");
  });

  it("leaves an empty box empty", () => {
    expect(loginAccount("service", ["picard"], "")).toBe("");
    expect(loginAccount("chosen", ["picard"], "")).toBe("");
  });
});

describe("nameLeftToService", () => {
  it("is a new name typed for a service that names its own", () => {
    expect(nameLeftToService("service", ["picard"], "riker")).toBe(true);
    expect(nameLeftToService("service", ["picard"], "picard")).toBe(false);
    expect(nameLeftToService("service", [], "")).toBe(false);
    expect(nameLeftToService("chosen", [], "riker")).toBe(false);
  });
});
