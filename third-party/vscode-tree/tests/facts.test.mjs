// The facts README.md lists, one test each, named as there. The first group
// is about upstream's AsyncDataTree as VS Code's source has it; the second is
// what <vs-tree> promises a page. A VS Code bump that moves one fails
// it by name: fix README.md, then the test, then src/vs-tree.js.

import { after, before, describe, test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import { chromium } from 'playwright';

const FILES = {
  '/': ['./harness.html', 'text/html'],
  '/harness.js': ['./harness.js', 'text/javascript'],
  '/vs-tree.js': ['../dist/vs-tree.js', 'text/javascript'],
};

let server;
let browser;
let base;

before(async () => {
  server = http.createServer((req, res) => {
    const file = FILES[new URL(req.url, 'http://x').pathname];
    if (!file) {
      res.writeHead(404);
      res.end();
      return;
    }
    res.writeHead(200, { 'content-type': file[1] });
    res.end(fs.readFileSync(new URL(file[0], import.meta.url)));
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  base = `http://127.0.0.1:${server.address().port}/`;
  try {
    browser = await chromium.launch({ executablePath: process.env.DATALIB_TEST_CHROMIUM || undefined });
  } catch (e) {
    console.error(
      'No Chromium for the playwright package.json pins. Install it with\n' +
        '  bazelisk run //third-party/vscode-tree:playwright -- install chromium\n' +
        `or point DATALIB_TEST_CHROMIUM at a Chromium binary.\n\n${e.message}`,
    );
    process.exit(1);
  }
});

after(async () => {
  await browser?.close();
  server?.close();
});

// A fresh page with the harness loaded. What it throws uncaught is in
// `window.harness.pageErrors`.
async function openHarness() {
  const page = await browser.newPage({ viewport: { width: 600, height: 600 } });
  await page.goto(base);
  await page.waitForFunction(() => window.harnessReady);
  return { page };
}

async function waitFor(page, what, fn, arg) {
  try {
    return await page.waitForFunction(fn, arg, { timeout: 5000 });
  } catch {
    throw new Error(`never happened: ${what}`);
  }
}

const row = (page, label) => page.locator('fake-card .monaco-list-row', { hasText: label }).first();

async function openRow(page, label, child) {
  await row(page, label).click();
  await row(page, child).waitFor({ timeout: 5000 });
}

// Opens upstream's tree by node id, a level at a time.
async function expandUpstream(page, ids) {
  for (const id of ids) await page.evaluate((i) => window.upstream.expand(window.harness.find(i)), id);
}

const focusedIds = (page) => page.evaluate(() => window.tree.viewState.focus);

describe('upstream', () => {
  test('view_state_keeps_what_a_closed_parent_hides', async () => {
    const { page } = await openHarness();
    await page.evaluate(() => window.harness.mountUpstream());
    await expandUpstream(page, ['folder/work', 'src/email']);
    const open = () => page.evaluate(() => [...window.upstream.getViewState().expanded].sort());
    assert.deepEqual(await open(), ['folder/work', 'src/email']);
    // Closing a parent hides its open child without closing it, and the
    // view state still lists the child.
    await page.evaluate(() => window.upstream.collapse(window.harness.find('folder/work')));
    assert.deepEqual(await open(), ['src/email']);
    await page.close();
  });

  test('a_restore_skips_what_a_closed_parent_hides', async () => {
    const { page } = await openHarness();
    // The view state the fact above reads off: Email open, Starfleet above it closed.
    await page.evaluate(() => window.harness.mountUpstream({}, { expanded: ['src/email'] }));
    assert.deepEqual(await page.evaluate(() => [...window.harness.fetched]), [null]);
    assert.deepEqual(await page.evaluate(() => window.upstream.getViewState().expanded), []);
    // Opening Starfleet afterwards leaves Email closed: the state is gone.
    await expandUpstream(page, ['folder/work']);
    assert.equal(await page.evaluate(() => window.upstream.getNode(window.harness.find('src/email')).collapsed), true);
    await page.close();
  });

  test('default_colours_are_dark', async () => {
    const { page } = await openHarness();
    const styles = await page.evaluate(() => window.harness.unthemedListStyles);
    assert.equal(styles.listHoverBackground, '#2A2D2E');
    assert.equal(styles.listActiveSelectionBackground, '#0E639C');
    await page.close();
  });

  test('a_click_opens_a_row_only_on_its_twistie', async () => {
    const { page } = await openHarness();
    await page.evaluate(() => window.harness.mountUpstream());
    await row(page, 'Starfleet').locator('.monaco-tl-contents').click();
    // Opening a node asks for its children within the click's own task, so by
    // the time this evaluate runs it would have.
    assert.deepEqual(await page.evaluate(() => [...window.harness.fetched]), [null]);
    // Without the codicon font the twistie has no size, so the click is sent to it.
    await row(page, 'Starfleet').locator('.monaco-tl-twistie').dispatchEvent('click');
    await row(page, 'Email').waitFor({ timeout: 5000 });
    await page.close();
  });

  test('fuzzy_type_navigation_needs_the_option', async () => {
    const focusAfterTyping = async (options) => {
      const { page } = await openHarness();
      await page.evaluate((o) => window.harness.mountUpstream({
        keyboardNavigationLabelProvider: { getKeyboardNavigationLabel: (n) => n.label },
        ...o,
      }), options);
      await expandUpstream(page, ['folder/work', 'src/slack', 'slack/ent']);
      await row(page, '#bridge').click();
      // Each key's handler runs while the key is dispatched, so the focus
      // has moved, or not, by the time `type` returns.
      await page.keyboard.type('ten');
      const focused = await page.evaluate(() => window.upstream.getFocus().map((n) => n.id));
      await page.close();
      return focused;
    };
    // "ten" is no prefix of "#ten-forward": without the option only a prefix moves the focus.
    assert.deepEqual(await focusAfterTyping({}), ['slack/ent/bridge']);
    assert.deepEqual(await focusAfterTyping({ typeNavigationEnabled: true }), ['slack/ent/tenforward']);
  });

  test('twistie_is_a_codicon_glyph', async () => {
    const { page } = await openHarness();
    await page.evaluate(() => window.harness.mountUpstream());
    const twistie = row(page, 'Starfleet').locator('.monaco-tl-twistie');
    const classes = await twistie.getAttribute('class');
    assert.match(classes, /\bcodicon\b/);
    assert.match(classes, /\bcodicon-tree-item-expanded\b/);
    // Upstream's own CSS gives it no shape: without the font it is not drawn.
    assert.equal(await twistie.isVisible(), false);
    await page.close();
  });

  test('styles_stay_in_the_trees_own_element', async () => {
    const { page } = await openHarness();
    const headBefore = await page.evaluate(() => document.head.querySelectorAll('style, link').length);
    await page.evaluate(() => window.harness.mountUpstream({ enableStickyScroll: true }));
    await expandUpstream(page, ['folder/work']);
    const where = await page.evaluate(() => ({
      head: document.head.querySelectorAll('style, link').length,
      inList: document.querySelector('fake-card').shadowRoot.querySelectorAll('.monaco-list style').length,
    }));
    assert.equal(where.head, headBefore, 'upstream added a stylesheet to the document');
    assert.ok(where.inList > 0, 'no stylesheet inside the tree');
    await page.close();
  });

  test('page_down_throws_upstream', async () => {
    const { page } = await openHarness();
    await page.evaluate(() => window.harness.mountUpstream());
    await expandUpstream(page, ['folder/work', 'src/drive', 'drive/root']);
    await row(page, 'Stardate log 41001').click();
    for (let i = 0; i < 4; i++) await page.keyboard.press('PageDown');
    try {
      await page.waitForFunction(() => window.harness.pageErrors.length > 0, null, { timeout: 5000 });
    } catch {
      assert.fail('PageDown no longer throws: drop #page from src/vs-tree.js');
    }
    assert.match((await page.evaluate(() => window.harness.pageErrors))[0], /reading 'size'/);
    await page.close();
  });
});

describe('vs-tree', () => {
  test('children_load_a_level_at_a_time', async () => {
    const { page } = await openHarness();
    await page.evaluate(() => window.harness.mountElement());
    assert.deepEqual(await page.evaluate(() => [...window.harness.fetched]), [null]);
    await openRow(page, 'Starfleet', 'Email');
    assert.deepEqual(await page.evaluate(() => [...window.harness.fetched]), [null, 'folder/work']);
    await page.close();
  });

  test('a_wide_level_draws_only_what_shows', async () => {
    const { page } = await openHarness();
    await page.evaluate(() => window.harness.mountElement());
    await openRow(page, 'Starfleet', 'Google Drive');
    await openRow(page, 'Google Drive', 'My Drive');
    await openRow(page, 'My Drive', 'Stardate log 41001');
    const drawn = await page.locator('vs-tree .monaco-list-row').count();
    assert.ok(drawn < 60, `${drawn} rows drawn for a level of 2,000`);
    await page.close();
  });

  test('type_to_navigate_moves_focus', async () => {
    const { page } = await openHarness();
    await page.evaluate(() => window.harness.mountElement());
    await openRow(page, 'Starfleet', 'Slack');
    await openRow(page, 'Slack', 'Enterprise-D');
    await openRow(page, 'Enterprise-D', '#bridge');
    await row(page, '#bridge').click();
    await page.keyboard.type('sick');
    assert.deepEqual(await focusedIds(page), ['slack/ent/sickbay']);
    await page.close();
  });

  test('page_down_pages_without_throwing', async () => {
    const { page } = await openHarness();
    await page.evaluate(() => window.harness.mountElement());
    await openRow(page, 'Starfleet', 'Google Drive');
    await openRow(page, 'Google Drive', 'My Drive');
    await openRow(page, 'My Drive', 'Stardate log 41001');
    await row(page, 'Stardate log 41001').click();
    for (let i = 0; i < 6; i++) await page.keyboard.press('PageDown');
    // The six pages are taken one after another, so wait for where they end:
    // focus well down the level, on a row that is drawn and marked focused.
    await waitFor(page, 'the focused row, pages down, on screen', () => {
      const [id] = window.tree.viewState.focus;
      if (!/^drive\/f\d+$/.test(id ?? '') || Number(id.slice('drive/f'.length)) <= 80) return false;
      const rows = document.querySelector('fake-card').shadowRoot.querySelector('vs-tree').shadowRoot
        .querySelectorAll('.monaco-list-row.focused');
      return rows.length === 1 && rows[0].textContent.includes(`Stardate log ${41000 + Number(id.slice('drive/f'.length))}`);
    });
    assert.deepEqual(await page.evaluate(() => window.harness.pageErrors), []);
    await page.close();
  });

  test('sticky_scroll_pins_the_ancestors', async () => {
    const { page } = await openHarness();
    await page.evaluate(() => window.harness.mountElement());
    await openRow(page, 'Starfleet', 'Google Drive');
    await openRow(page, 'Google Drive', 'My Drive');
    await openRow(page, 'My Drive', 'Stardate log 41001');
    await row(page, 'Stardate log 41001').click();
    for (let i = 0; i < 3; i++) await page.keyboard.press('PageDown');
    await waitFor(page, 'three ancestors pinned', () =>
      document.querySelector('fake-card').shadowRoot.querySelector('vs-tree').shadowRoot
        .querySelectorAll('.monaco-tree-sticky-row').length === 3);
    // Top to bottom as drawn; the rows are not in that order in the DOM.
    const pinned = await page.evaluate(() =>
      [...document.querySelector('fake-card').shadowRoot.querySelector('vs-tree').shadowRoot
        .querySelectorAll('.monaco-tree-sticky-row')]
        .sort((a, b) => a.getBoundingClientRect().top - b.getBoundingClientRect().top)
        .map((r) => r.querySelector('.label').textContent));
    assert.deepEqual(pinned, ['Starfleet', 'Google Drive', 'My Drive']);
    await page.close();
  });

  test('a_restore_reopens_under_a_closed_parent', async () => {
    const { page } = await openHarness();
    await page.evaluate(() => window.harness.mountElement());
    await openRow(page, 'Starfleet', 'Email');
    await openRow(page, 'Email', 'picard@enterprise.example');
    await openRow(page, 'picard@enterprise.example', 'Missions');
    await row(page, 'Starfleet').click();
    await row(page, 'Email').waitFor({ state: 'hidden', timeout: 5000 });
    const saved = await page.evaluate(() => window.tree.viewState);
    assert.deepEqual([...saved.expanded].sort(), ['email/picard', 'src/email']);

    await page.evaluate((s) => window.harness.mountElement(s), saved);
    assert.deepEqual(await page.evaluate(() => [...window.tree.viewState.expanded].sort()), ['email/picard', 'src/email']);
    await row(page, 'Starfleet').click();
    await row(page, 'Missions').waitFor({ timeout: 5000 });
    assert.deepEqual(
      await page.evaluate(() => [...window.tree.viewState.expanded].sort()),
      ['email/picard', 'folder/work', 'src/email'],
    );
    await page.close();
  });

  test('a_double_click_or_enter_opens_a_node', async () => {
    const { page } = await openHarness();
    await page.evaluate(() => window.harness.mountElement());
    await openRow(page, 'Starfleet', 'Slack');
    await row(page, 'Slack').dblclick();
    await waitFor(page, 'Slack opened', () => window.harness.opened.includes('src/slack'));
    await row(page, 'Email').click();
    await page.keyboard.press('Enter');
    await waitFor(page, 'Email opened', () => window.harness.opened.includes('src/email'));
    await page.close();
  });

  test('colours_follow_the_pages_variables', async () => {
    const { page } = await openHarness();
    await page.evaluate(() => window.harness.mountElement());
    await page.evaluate(() => window.tree.style.setProperty('--vs-tree-selected-bg', 'rgb(1, 2, 3)'));
    await row(page, 'Starfleet').click();
    const selected = page.locator('vs-tree .monaco-list-row.selected').first();
    await selected.waitFor({ timeout: 5000 });
    assert.equal(await selected.evaluate((el) => getComputedStyle(el).backgroundColor), 'rgb(1, 2, 3)');
    await page.close();
  });

  test('view_state_round_trips', async () => {
    const { page } = await openHarness();
    await page.evaluate(() => window.harness.mountElement());
    await openRow(page, 'Starfleet', 'Email');
    await openRow(page, 'Email', 'picard@enterprise.example');
    await openRow(page, 'picard@enterprise.example', 'Missions');
    await openRow(page, 'Missions', 'Borg');
    await openRow(page, 'Google Drive', 'My Drive');
    await openRow(page, 'My Drive', 'Stardate log 41001');
    await row(page, 'Stardate log 41003').click();
    for (let i = 0; i < 2; i++) await page.keyboard.press('PageDown');
    await waitFor(page, 'scrolled', () => window.tree.viewState.scrollTop > 0);
    const saved = await page.evaluate(() => window.tree.viewState);

    await page.evaluate((s) => window.harness.mountElement(s), saved);
    await waitFor(page, 'scroll restored', (top) => window.tree.viewState.scrollTop === top, saved.scrollTop);
    const restored = await page.evaluate(() => window.tree.viewState);
    const fetched = await page.evaluate(() => [...window.harness.fetched]);

    assert.deepEqual([...restored.expanded].sort(), [...saved.expanded].sort());
    assert.deepEqual(restored.selection, saved.selection);
    assert.equal(restored.scrollTop, saved.scrollTop);
    // Restoring asks once for the top and once for each open node, nothing more.
    assert.deepEqual([...fetched].sort(), [null, ...saved.expanded].sort());
    await page.close();
  });
});
