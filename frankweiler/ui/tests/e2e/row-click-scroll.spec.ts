import { test, expect } from "@playwright/test";

// Bug #2: clicking a grid row should scroll the markdown panel on the
// right to the corresponding message — even when the chat is already
// loaded.
//
// The chat preview pane marks the message at `messageIndex` with a
// `.selected` class and scrolls it into view. The contract this test
// pins: clicking two grid rows that map to *different* messages of the
// same conversation must end up highlighting different messages in the
// preview pane.

test("row click selects the right message in the preview pane", async ({
  page,
  request,
}) => {
  // Use the backend search API to find a conversation with at least two
  // messages of distinct message_index. This avoids guessing about which
  // grid rows happen to be visible / contiguous.
  const resp = await request.get("/api/search?q=&limit=2000");
  expect(resp.ok()).toBeTruthy();
  const data = (await resp.json()) as {
    rows: {
      conversation_uuid: string;
      kind: string;
      message_index: number | null;
      snippet: string;
    }[];
  };
  const byConv = new Map<string, typeof data.rows>();
  for (const r of data.rows) {
    if (r.kind === "Chat") continue;
    const list = byConv.get(r.conversation_uuid) ?? [];
    list.push(r);
    byConv.set(r.conversation_uuid, list);
  }
  // Want two rows with distinct, non-null message_index values.
  let chosen:
    | {
        uuid: string;
        idxA: number;
        idxB: number;
      }
    | null = null;
  for (const [uuid, list] of byConv) {
    const distinct = list.filter((r) => r.message_index != null);
    distinct.sort((a, b) => a.message_index! - b.message_index!);
    const first = distinct[0];
    const last = distinct[distinct.length - 1];
    if (first && last && first.message_index !== last.message_index) {
      chosen = {
        uuid,
        idxA: first.message_index!,
        idxB: last.message_index!,
      };
      break;
    }
  }
  expect(
    chosen,
    "fixture must contain a conversation with at least two messages of distinct message_index",
  ).not.toBeNull();

  await page.goto("/");
  // Wait for the grid (and its api) to be ready.
  await page
    .locator('.ag-center-cols-container [role="row"]')
    .first()
    .waitFor({ timeout: 10_000 });

  // AG Grid virtualizes rows: only those near the viewport exist in
  // the DOM. Use the grid api (exposed on window) to scroll the
  // target node into view before clicking.
  async function scrollToAndClick(idx: number) {
    const rowIndex = await page.evaluate(
      ({ uuid, msgIdx }) => {
        type Node = {
          rowIndex: number | null;
          data?: {
            conversation_uuid: string;
            kind: string;
            message_index: number | null;
          };
        };
        const w = window as unknown as {
          __fwGridApi?: {
            forEachNode: (cb: (n: Node) => void) => void;
            ensureNodeVisible: (n: Node, pos: "middle") => void;
          };
        };
        const api = w.__fwGridApi!;
        let foundIdx: number | null = null;
        api.forEachNode((node) => {
          if (
            node.data &&
            node.data.conversation_uuid === uuid &&
            node.data.kind !== "Chat" &&
            node.data.message_index === msgIdx
          ) {
            api.ensureNodeVisible(node, "middle");
            foundIdx = node.rowIndex;
          }
        });
        return foundIdx;
      },
      { uuid: chosen!.uuid, msgIdx: idx },
    );
    expect(rowIndex, `node for msg_idx=${idx} found in grid`).not.toBeNull();
    const row = page.locator(
      `.ag-center-cols-container [role="row"][row-index="${rowIndex}"]`,
    );
    await expect(row).toBeVisible({ timeout: 5_000 });
    await row.click();
  }

  await scrollToAndClick(chosen!.idxA);
  const preview = page.locator(".chat-preview");
  await expect(
    preview.locator(`[data-msg-index="${chosen!.idxA}"].selected`),
  ).toBeVisible({ timeout: 10_000 });

  await scrollToAndClick(chosen!.idxB);
  await expect(
    preview.locator(`[data-msg-index="${chosen!.idxB}"].selected`),
  ).toBeVisible({ timeout: 10_000 });
  await expect(
    preview.locator(`[data-msg-index="${chosen!.idxA}"].selected`),
  ).toHaveCount(0);
});
