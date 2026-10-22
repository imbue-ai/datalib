// One "Sign in with browser" for a source whose download reaches several
// latchkey services as one account (Google Drive: drive, docs, sheets,
// slides). Every one of them names its own accounts, so only the first
// login can be left to say who; the rest must land on the same one.

/// The account the sign-in is for, before any login has run: the one
/// the box names when some service already holds it, else none yet, and
/// the first login says.
export function targetAccount(held: Record<string, string[]>, typed: string): string {
  const name = typed.trim();
  return name !== "" && Object.values(held).some((names) => names.includes(name)) ? name : "";
}

/// Which services a sign-in as `account` runs, in order. Those already
/// holding it are skipped, so finishing a half-done sign-in costs only
/// the logins left; when every one holds it, the person is signing in
/// again, and every one is run.
export function servicesToSignIn(
  services: string[],
  held: Record<string, string[]>,
  account: string,
): string[] {
  const holds = (s: string) => account !== "" && (held[s] ?? []).includes(account);
  return services.every(holds) ? services : services.filter((s) => !holds(s));
}

/// What the button says for the box's account.
export function signInLabel(
  services: string[],
  held: Record<string, string[]>,
  account: string,
): string {
  if (account === "") return "Sign in with browser";
  const left = servicesToSignIn(services, held, account);
  return left.length === services.length
    ? `Sign in again as ${account}`
    : `Finish signing in as ${account}`;
}

/// "Drive, Docs and Sheets": a list of labels as a sentence says it.
export function listed(labels: string[]): string {
  if (labels.length <= 1) return labels.join("");
  return `${labels.slice(0, -1).join(", ")} and ${labels[labels.length - 1]}`;
}
