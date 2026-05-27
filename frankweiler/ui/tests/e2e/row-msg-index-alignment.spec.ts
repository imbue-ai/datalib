import { test, expect } from "@playwright/test";

// Regression test for the off-by-one bug: clicking a grid row in the
// message list highlighted a *different* message in the preview pane
// than the one the row pointed at. Repro fixture is "Sonnet on a Cat
// Named Spot", where the underlying ChatGPT payload has a leading
// system / model_editable_context message that the renderer filters
// out — but grid_rows used to count it, so `data-msg-index` and
// `message_index` were off by one.
//
// The fix removed `data-msg-index` entirely and keys selection off
// `data-section-uuid` (== the row's `uuid`). The invariant: whichever
// section ends up with `.selected`, its `data-section-uuid` must equal
// the clicked row's `uuid`.
//
// This test is independent of the specific fixture conversation — it
// scans every non-Chat row whose corresponding section actually exists
// in the rendered body. Any future drift between grid row uuids and
// renderer section uuids breaks this test.

type Row = {
  uuid: string;
  conversation_uuid: string;
  kind: string;
  message_index: number | null;
};

test("clicked grid row highlights the section with the matching uuid", async ({
  page,
  request,
}) => {
  // Per-row click + chat fetch is ~1s; with many candidates we need
  // headroom past the 30s default.
  test.setTimeout(120_000);
  const resp = await request.get("/api/search?q=&limit=2000");
  expect(resp.ok()).toBeTruthy();
  const data = (await resp.json()) as { rows: Row[] };

  // Find a (uuid, conv) pair where the rendered body actually contains
  // a `data-section-uuid="{uuid}"`. That filters out rows whose
  // underlying message the renderer dropped (system /
  // model_editable_context) — there's no section to highlight for
  // those, so they aren't a meaningful click target.
  await page.goto("/");
  await page
    .locator('.ag-center-cols-container [role="row"]')
    .first()
    .waitFor({ timeout: 10_000 });

  // Build the set of (row, body) we'll exercise: every non-Chat row
  // whose rendered section actually exists in the conversation body.
  // Cache chat bodies by conversation so we don't refetch per row.
  const bodyCache = new Map<string, string>();
  async function bodyFor(conv: string): Promise<string | null> {
    const cached = bodyCache.get(conv);
    if (cached !== undefined) return cached;
    const r = await request.get(`/api/chat/${encodeURIComponent(conv)}`);
    if (!r.ok()) {
      bodyCache.set(conv, "");
      return null;
    }
    const j = (await r.json()) as { body: string };
    bodyCache.set(conv, j.body);
    return j.body;
  }

  // Sample one row per conversation so a fixture with thousands of
  // messages still finishes in reasonable time. Picking the first
  // surviving message_index per conversation is enough — the bug
  // manifests on *any* conversation whose renderer drops a message
  // the grid_rows row counter still saw.
  const candidates: Row[] = [];
  const seenConvs = new Set<string>();
  for (const r of data.rows) {
    if (r.kind === "Chat") continue;
    if (r.message_index == null) continue;
    if (seenConvs.has(r.conversation_uuid)) continue;
    const body = await bodyFor(r.conversation_uuid);
    if (!body) continue;
    if (!body.includes(`id="m-${r.uuid}"`)) continue;
    candidates.push(r);
    seenConvs.add(r.conversation_uuid);
  }
  expect(
    candidates.length,
    "fixture must contain at least one message row whose rendered section exists",
  ).toBeGreaterThan(0);

  // Click each candidate row and record any mismatch between the
  // clicked row's uuid and the highlighted section's DOM id. Collecting
  // all of them (instead of bailing on the first) makes the failure
  // message useful for diagnosing how widespread the misalignment is.
  type Mismatch = {
    uuid: string;
    conv: string;
    messageIndex: number;
    selectedId: string | null;
  };
  const mismatches: Mismatch[] = [];

  for (const pick of candidates) {
    const rowIndex = await page.evaluate(
      ({ uuid }) => {
        type Node = {
          rowIndex: number | null;
          data?: { uuid: string };
        };
        const w = window as unknown as {
          __fwGridApi?: {
            forEachNode: (cb: (n: Node) => void) => void;
            ensureNodeVisible: (n: Node, pos: "middle") => void;
          };
        };
        const api = w.__fwGridApi!;
        let found: number | null = null;
        api.forEachNode((node) => {
          if (node.data && node.data.uuid === uuid) {
            api.ensureNodeVisible(node, "middle");
            found = node.rowIndex;
          }
        });
        return found;
      },
      { uuid: pick.uuid },
    );
    expect(rowIndex, `node for uuid=${pick.uuid} found in grid`).not.toBeNull();
    await page
      .locator(`.ag-center-cols-container [role="row"][row-index="${rowIndex}"]`)
      .click();

    // Wait for the preview to switch to the clicked row's conversation.
    // The preview pane is shared across clicks, so a stale selection
    // from the previous click can mask the new state if we don't gate
    // on the conversation attribute first.
    await page
      .locator(`.chat-preview[data-conversation-uuid="${pick.conversation_uuid}"]`)
      .waitFor({ timeout: 10_000 });
    // Allow applySelection's nextTick + scrollTop write to settle.
    // We can't gate on `.msg.selected` existing — a row whose section
    // uuid doesn't resolve to anything in the body leaves nothing
    // selected, and "nothing selected" is itself a misalignment we
    // want to report (not time out on).
    await page.waitForTimeout(150);

    const selectedSectionUuid = await page
      .locator(".chat-preview .msg.selected")
      .first()
      .getAttribute("data-section-uuid")
      .catch(() => null);
    if (selectedSectionUuid !== pick.uuid) {
      mismatches.push({
        uuid: pick.uuid,
        conv: pick.conversation_uuid,
        messageIndex: pick.message_index!,
        selectedId: selectedSectionUuid,
      });
    }
  }

  expect(
    mismatches,
    `clicked rows whose highlight landed on the wrong section:\n` +
      mismatches
        .map(
          (m) =>
            `  row uuid=${m.uuid} (conv=${m.conv} message_index=${m.messageIndex}) ` +
            `→ highlighted ${m.selectedId}`,
        )
        .join("\n"),
  ).toEqual([]);
});
