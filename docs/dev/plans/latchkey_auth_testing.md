# Testing the wizard's latchkey sign-in flows

**Status: the hermetic tier is built, with its fault matrix, and the
product fixes it pinned have landed (2026-10-05); the keychain tier and
the real-machine tier are proposals.** How
the built part works is in
[`datalib/ui/tests/e2e_auth/README.md`](../../../datalib/ui/tests/e2e_auth/README.md);
this page is what it rests on and what is left.

## 1. Facts the suite rests on

Read from latchkey 3.16's source and measured on a mac the same day.

- **Every latchkey run reads the keychain**, `--version` and
  `services info` included: `cli.js` resolves its encryption key before
  it parses arguments. Only `LATCHKEY_ENCRYPTION_KEY` or
  `LATCHKEY_GATEWAY` skips it. `LATCHKEY_KEYRING_SERVICE_NAME` only
  renames the item.
- **The wizard runs latchkey when a tile is picked**:
  `SourceWizard.vue` watches `service` with `immediate: true` and calls
  `GET /api/latchkey/<svc>`, which runs `services info` — without
  `--offline`, so a stored credential is also checked over the network.
  With one stored, that took 6.4s in the suite.
- **latchkey hard-codes a visible browser window** for `auth browser`;
  the only lever is the executable `ensure-browser` records.
- **latchkey's own credential check goes out as a plain curl**: it runs
  through `LATCHKEY_CURL` without datalib's impersonation marker, so for
  a host behind Cloudflare's bot wall it would be refused whatever the
  credential.
- **A `latchkey gateway` binds `localhost`**, which did not answer on
  127.0.0.1 here; the suite passes `--host 127.0.0.1`.

## 3. What is left

**Coverage still missing:**

- **The keychain tier (macOS).** Leave `LATCHKEY_ENCRYPTION_KEY` unset
  and give each run its own `LATCHKEY_KEYRING_SERVICE_NAME`, deleted at
  teardown; on a GitHub runner, also a throwaway default keychain
  (`security create-keychain`, `default-keychain -s`, `unlock-keychain`,
  `set-keychain-settings`). Never the second half on a laptop: it
  changes the person's keychain search list. Scenarios: first run
  creates the key; `.enc` present but the item gone ("encryption key was
  lost"); a locked keychain (latchkey's 30s timeout); an item written by
  one `node` and read by another — the command-line Node versus the
  bundled one, which is the likeliest cause of the prompts people see.
  Measure that last one before designing around it.
- **The real-machine tier (CI VM only).** Point `/etc/hosts` at the
  fake, trust a throwaway CA, and drop the curl shim and the browser
  wrapper: a headed browser, the real network stack, nothing between
  latchkey and the "site" but DNS. The specs stay the same; the harness
  switches on one variable.
- **Fastmail's and Google's OAuth logins**, which need a fake
  authorization server.
- **Uniformity**: one table-driven spec over every credentialed catalog
  entry, where a source that is meant to differ says so in the table.
- **Scenarios not yet written**: two stored accounts and the "No
  credentials stored for account" retry; a login page that never hands
  out a credential (needs a shorter connect timeout for tests); the real
  browser download, which the suite stands in for.
- **Linux.** The suite sets `DISPLAY` for latchkey's check there, but
  has only run on a mac.
