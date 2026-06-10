import { test, expect } from "@playwright/test";
import { clickRowByUuid } from "./grid-helpers";

// Regression test for the off-by-one bug: clicking a grid row in the
// message list highlighted a *different* message in the document pane
// than the one the row pointed at. Repro fixture is "Sonnet on a Cat
// Named Spot", where the underlying ChatGPT payload has a leading
// system / model_editable_context message that the renderer filters
// out — but grid_rows used to count it, so the rendered index and
// `message_index` were off by one.
//
// Selection is keyed off `data-section-uuid` (== the row's `uuid`):
// the grid opens `documentView(markdown_uuid, row.uuid)` and the doc
// card highlights the section whose `data-section-uuid` matches. The
// invariant: whichever section ends up with `.selected`, its
// `data-section-uuid` must equal the clicked row's `uuid`.
//
// This test is independent of the specific fixture conversation — it
// scans every non-Chat row whose corresponding section actually exists
// in the rendered body, sampling one row per conversation.

type Row = {
  uuid: string;
  conversation_uuid: string;
  markdown_uuid: string | null;
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

  await page.goto("/");
  await page
    .locator('.ag-center-cols-container [role="row"]')
    .first()
    .waitFor({ timeout: 10_000 });

  // Build the set of rows we'll exercise: every non-Chat row whose
  // rendered section actually exists in the conversation body (the
  // renderer drops some messages — system / model_editable_context —
  // and those have no section to highlight). Cache chat bodies by
  // conversation so we don't refetch per row; sample one row per
  // conversation so a fixture with thousands of messages still
  // finishes in reasonable time.
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
  // clicked row's uuid and the highlighted section's uuid. Collecting
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
    await clickRowByUuid(page, pick.uuid);

    // Each click opens a fresh documentView card; gate on the card
    // for the clicked row's markdown being in place before reading
    // the selection.
    await page
      .locator(
        `.chat-preview[data-markdown-uuid="${pick.markdown_uuid ?? pick.uuid}"]`,
      )
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
    `every clicked row must highlight the section with its own uuid; mismatches: ${JSON.stringify(
      mismatches,
      null,
      2,
    )}`,
  ).toEqual([]);
});
