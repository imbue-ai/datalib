// What the wizard's account box means for one latchkey service, which
// depends on who names the account a browser login adds
// (docs/dev/latchkey.md §"Accounts: who names them").

import type { AccountNaming } from "@/api";

/// The account a browser login is asked to store under, or "" for none.
/// A service that names its own accounts refuses a name it does not
/// hold yet, so a new one is left for the login to report.
export function loginAccount(naming: AccountNaming, stored: string[], typed: string): string {
  const name = typed.trim();
  if (naming !== "service") return name;
  return stored.includes(name) ? name : "";
}

/// Whether the box names an account a browser login will not use: the
/// service will name the new one itself.
export function nameLeftToService(naming: AccountNaming, stored: string[], typed: string): boolean {
  const name = typed.trim();
  return naming === "service" && name !== "" && !stored.includes(name);
}
