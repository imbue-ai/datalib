// The demo page: a made-up TNG library served to <vs-tree> a level at a
// time, the tree's requests logged, and its view state kept in
// localStorage so a reload restores it.

import '/vs-tree.js';

const STATE_KEY = 'vs-tree-demo:view-state';
const DELAY_KEY = 'vs-tree-demo:delay';

const node = (id, label, kind, kids, description) => ({ id, label, kind, kids, description, hasChildren: !!kids });

const driveFolders = Array.from({ length: 10000 }, (_, i) => {
  const id = `drive/f${i}`;
  const kids = i % 100 === 0 ? [node(`${id}/a`, 'Away team reports', 'Folder'), node(`${id}/b`, 'Sensor logs', 'Folder')] : null;
  return node(id, `Stardate ${(41000 + i / 10).toFixed(1)}`, 'Folder', kids);
});

// A chain of pages deep enough that sticky scroll has ancestors to pin.
function notionPages(depth, prefix) {
  const titles = ['Archaeology', 'Iconia', 'Probe findings', 'Translations', 'Glyphs', 'Lexicon', 'Draft papers', 'Footnotes'];
  if (depth === titles.length) {
    return Array.from({ length: 60 }, (_, i) => node(`${prefix}/n${i}`, `Note ${i + 1}`, 'Page'));
  }
  const id = `${prefix}/${depth}`;
  const siblings = Array.from({ length: 6 }, (_, i) => node(`${id}/s${i}`, `${titles[depth]} appendix ${i + 1}`, 'Page'));
  return [node(id, titles[depth], 'Page', notionPages(depth + 1, id)), ...siblings];
}

const LIBRARY = [
  node('folder/work', 'Starfleet', 'Folder', [
    node('src/email', 'Email', 'Source', [
      node('email/picard', 'picard@enterprise.example', 'Account', [
        node('email/picard/inbox', 'Inbox', 'Label'),
        node('email/picard/missions', 'Missions', 'Label', [
          node('email/picard/missions/farpoint', 'Farpoint', 'Label'),
          node('email/picard/missions/borg', 'Borg', 'Label', [node('email/picard/missions/borg/wolf359', 'Wolf 359', 'Label')]),
          node('email/picard/missions/romulan', 'Romulan Neutral Zone', 'Label'),
        ]),
        node('email/picard/archive', 'Archive', 'Label'),
      ]),
      node('email/riker', 'riker@enterprise.example', 'Account', [node('email/riker/inbox', 'Inbox', 'Label'), node('email/riker/trombone', 'Trombone', 'Label')]),
    ], 'JMAP'),
    node('src/slack', 'Slack', 'Source', [
      node('slack/ent', 'Enterprise-D', 'Workspace', [
        node('slack/ent/bridge', '#bridge', 'Channel'),
        node('slack/ent/engineering', '#engineering', 'Channel'),
        node('slack/ent/holodeck', '#holodeck', 'Channel'),
        node('slack/ent/sickbay', '#sickbay', 'Channel'),
        node('slack/ent/tenforward', '#ten-forward', 'Channel'),
      ]),
    ]),
    node('src/drive', 'Google Drive', 'Source', [node('drive/root', 'My Drive', 'Folder', driveFolders)], '10,000 folders'),
  ]),
  node('folder/personal', 'Personal', 'Folder', [
    node('src/notion', 'Notion', 'Source', [node('notion/home', 'Picard’s notebook', 'Workspace', notionPages(0, 'notion/home'))]),
    node('src/contacts', 'Contacts', 'Source', [node('contacts/book', 'Address book', 'Book', [node('contacts/book/crew', 'Senior staff', 'Group'), node('contacts/book/family', 'Family', 'Group')])]),
    node('src/chatgpt', 'ChatGPT', 'Source', null, 'no places below'),
  ]),
];

function storageGet(key) {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}
function storageSet(key, value) {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, value);
  } catch {
    // Private windows and blocked storage: the demo still works, it just forgets.
  }
}

const logEl = document.getElementById('log');
function log(html) {
  const line = document.createElement('div');
  line.innerHTML = html;
  logEl.prepend(line);
  while (logEl.childElementCount > 200) logEl.lastElementChild.remove();
}
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]);
const stamp = () => new Date().toLocaleTimeString([], { hour12: false });

const delayInput = document.getElementById('delay');
const delayValue = document.getElementById('delay-value');
delayInput.value = storageGet(DELAY_KEY) ?? '300';
const showDelay = () => (delayValue.textContent = `${delayInput.value} ms`);
showDelay();
delayInput.addEventListener('input', () => {
  showDelay();
  storageSet(DELAY_KEY, delayInput.value);
});

const dataSource = {
  async getChildren(n) {
    const started = performance.now();
    await new Promise((r) => setTimeout(r, Number(delayInput.value)));
    const kids = n ? n.kids : LIBRARY;
    const ms = Math.round(performance.now() - started);
    log(`${stamp()}  children of <b>${esc(n ? n.label : 'the library')}</b> → ${kids.length.toLocaleString()} in ${ms} ms`);
    return kids;
  },
};

const tree = document.querySelector('vs-tree');
const saved = storageGet(STATE_KEY);
if (saved) {
  try {
    tree.viewState = JSON.parse(saved);
    log(`${stamp()}  restoring ${JSON.parse(saved).expanded.length} open nodes from the last visit`);
  } catch {
    storageSet(STATE_KEY, null);
  }
}
tree.dataSource = dataSource;

let saveQueued = false;
tree.addEventListener('viewstatechange', () => {
  if (saveQueued) return;
  saveQueued = true;
  requestAnimationFrame(() => {
    saveQueued = false;
    storageSet(STATE_KEY, JSON.stringify(tree.viewState));
  });
});

const selectedEl = document.getElementById('selected');
tree.addEventListener('select', (e) => {
  storageSet(STATE_KEY, JSON.stringify(tree.viewState));
  const [n] = e.detail.nodes;
  if (!n) return;
  const rows = [
    ['Name', n.label],
    ['Kind', n.kind],
    ['Id', n.id],
    ['Children', n.kids ? n.kids.length.toLocaleString() : 'none'],
  ];
  if (n.description) rows.push(['Note', n.description]);
  selectedEl.replaceChildren(
    ...rows.flatMap(([k, v]) => {
      const dt = document.createElement('dt');
      dt.textContent = k;
      const dd = document.createElement('dd');
      dd.textContent = v;
      return [dt, dd];
    }),
  );
});

document.getElementById('reset').addEventListener('click', () => {
  storageSet(STATE_KEY, null);
  location.reload();
});
