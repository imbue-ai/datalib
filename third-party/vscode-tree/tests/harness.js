// The page facts.test.mjs drives. A tree lives inside a card's shadow root,
// as it would in datalib, over made-up TNG data; `window.harness` mounts
// one and records what it asked for.

import { AsyncDataTree, unthemedListStyles } from '/vs-tree.js';

// What the page threw and nobody caught, recorded in the task that threw it,
// so a test reading it after an action sees everything that action raised.
const pageErrors = [];
window.addEventListener('error', (e) => pageErrors.push(String(e.message)));

customElements.define(
  'fake-card',
  class extends HTMLElement {
    constructor() {
      super();
      this.attachShadow({ mode: 'open' }).innerHTML = `
        <style>
          :host { display: flex; flex-direction: column; margin: 16px; width: 420px; height: 480px;
            border: 1px solid #d0d7de; border-radius: 8px; background: #fff; overflow: hidden; }
          header { padding: 8px 12px; font-weight: 600; border-bottom: 1px solid #d0d7de; }
          .body { flex: 1; position: relative; }
          .body > * { position: absolute; inset: 0; }
        </style>
        <header>Library</header>
        <div class="body"></div>`;
    }
    get body() {
      return this.shadowRoot.querySelector('.body');
    }
  },
);

const node = (id, label, kind, kids, description) => ({ id, label, kind, kids, description, hasChildren: !!kids });

const driveFolders = Array.from({ length: 2000 }, (_, i) =>
  node(`drive/f${i}`, `Stardate log ${41000 + i}`, 'Folder', i % 50 === 0 ? [node(`drive/f${i}/a`, 'Attachments', 'Folder')] : null),
);

// A mark for a source, as a page hands one: a URL the row draws.
const MARK = 'data:image/svg+xml,' + encodeURIComponent('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><circle cx="8" cy="8" r="7" fill="teal"/></svg>');

const LIBRARY = [
  node('folder/work', 'Starfleet', 'Folder', [
    node('src/email', 'Email', 'Source', [
      node('email/picard', 'picard@enterprise.example', 'Account', [
        node('email/picard/inbox', 'Inbox', 'Label'),
        node('email/picard/missions', 'Missions', 'Label', [
          node('email/picard/missions/farpoint', 'Farpoint', 'Label'),
          node('email/picard/missions/borg', 'Borg', 'Label', [node('email/picard/missions/borg/wolf359', 'Wolf 359', 'Label')]),
        ]),
      ]),
    ]),
    node('src/slack', 'Slack', 'Source', [
      node('slack/ent', 'Enterprise-D', 'Workspace', [
        node('slack/ent/bridge', '#bridge', 'Channel'),
        node('slack/ent/engineering', '#engineering', 'Channel'),
        node('slack/ent/tenforward', '#ten-forward', 'Channel'),
        node('slack/ent/sickbay', '#sickbay', 'Channel'),
      ]),
    ]),
    node('src/drive', 'Google Drive', 'Source', [node('drive/root', 'My Drive', 'Folder', driveFolders)], '2,000 folders'),
  ]),
  node('folder/personal', 'Personal', 'Folder', [node('src/chatgpt', 'ChatGPT', 'Source', null, 'no places below')]),
];

// Slack's row draws a mark; the others draw none.
LIBRARY[0].kids.find((n) => n.id === 'src/slack').icon = MARK;

const fetched = [];
const dataSource = {
  async getChildren(n) {
    fetched.push(n ? n.id : null);
    await new Promise((r) => setTimeout(r, 5));
    return n ? n.kids : LIBRARY;
  },
};

function freshCard() {
  document.querySelector('fake-card')?.remove();
  return document.body.appendChild(document.createElement('fake-card'));
}

// The element, as a page would use it. Resolves once the top level (and
// whatever `viewState` opens) has loaded.
const opened = [];

function mountElement(viewState) {
  fetched.length = 0;
  opened.length = 0;
  const tree = document.createElement('vs-tree');
  tree.addEventListener('open', (e) => opened.push(e.detail.node.id));
  if (viewState) tree.viewState = viewState;
  const ready = new Promise((r) => tree.addEventListener('ready', r, { once: true }));
  tree.dataSource = dataSource;
  freshCard().body.appendChild(tree);
  window.tree = tree;
  return ready;
}

// Upstream's AsyncDataTree with nothing of ours around it but what it
// needs to draw, for the facts about upstream itself.
async function mountUpstream(options = {}, viewState) {
  fetched.length = 0;
  const frame = freshCard().body.appendChild(document.createElement('div'));
  const renderer = {
    templateId: 'node',
    renderTemplate: (container) => container.appendChild(document.createElement('span')),
    renderElement: (n, _i, span) => (span.textContent = n.element.label),
    disposeTemplate() {},
  };
  const ROOT = { id: 'root' };
  const tree = new AsyncDataTree(
    'facts',
    frame,
    { getHeight: () => 22, getTemplateId: () => 'node' },
    [renderer],
    {
      hasChildren: (n) => n === ROOT || !!n.hasChildren,
      getChildren: (n) => dataSource.getChildren(n === ROOT ? null : n),
    },
    { identityProvider: { getId: (n) => n.id }, ...options },
  );
  tree.layout(frame.clientHeight, frame.clientWidth);
  await tree.setInput(ROOT, viewState);
  window.upstream = tree;
}

window.harness = {
  mountElement,
  mountUpstream,
  fetched,
  opened,
  pageErrors,
  find: (id) => {
    const walk = (list) => {
      for (const n of list ?? []) {
        if (n.id === id) return n;
        const hit = walk(n.kids);
        if (hit) return hit;
      }
    };
    return walk(LIBRARY);
  },
  AsyncDataTree,
  unthemedListStyles,
};
window.harnessReady = true;
