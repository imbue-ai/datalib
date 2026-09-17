// The DACTAL explorer page's own logic — the query bar, the render loop,
// and the wiring of the globals DACTAL's vendored renderer expects.
import { loadSearchIntoDactal, fetchSearch, awaitInit, hosted } from "./bridge.js";

// --- Wire up the globals DACTAL's renderer expects -------------------------
// dactal_utils.js references a bare global `dactal` (the engine instance) and
// `dactaldb` (used by column hide/filter handlers). buildView/arrayToTable
// also call a global `runQuery` for drill-down; we install OUR OWN below so
// clicks re-render into our container instead of DACTAL's host page.
const dactal = new DACTAL();
window.dactal = dactal;
// The renderer's store, in memory. IndexedDB is not available to a
// sandboxed page (an opaque origin has no storage), and nothing here
// needs to outlive the card: the working set is reloaded from the host.
window.dactaldb = {
  dbname: "dactal-datalib",
  persistent_query_history: false,
  store: new Map(),
  async keys() { return [...this.store.keys()]; },
  async get(key) { return this.store.get(key); },
  async set(key, val) { this.store.set(key, val); },
  async remove(key) { this.store.delete(key); },
};

// Inject DACTAL's table styles (normally added by its own host page).
const css = document.createElement("style");
// `dactal_css` is a top-level `const` in dactal_utils.js → a global *lexical*
// binding, reachable by bare name from this module but NOT as window.dactal_css.
css.textContent = dactal_css || "";
document.head.appendChild(css);

const out = document.getElementById("queryoutput");
const dqInput = document.getElementById("dq");
const statusEl = document.getElementById("status");
const crumbs = document.getElementById("crumbs");
let history = [];

// Our minimal stand-in for DACTAL's runQuery: execute with the engine, render
// with DACTAL's buildView. buildView's internal drill-down links call the
// global runQuery (this function), so navigation composes for free.
function runQuery(query) {
  if (query == null) query = dqInput.value;
  dqInput.value = query;
  out.textContent = "";
  // DACTAL's render() reads a global `audioshown` counter that its own
  // runQuery seeds before each render; reading it undeclared would throw.
  window.audioshown = 0;
  try {
    const results = dactal.executeq(query);     // synchronous engine path
    const view = buildView(results, query, out); // DACTAL's table UI (global fn)
    out.appendChild(view);
    if (history[history.length - 1] !== query) history.push(query);
    renderCrumbs();
    const n = Array.isArray(results) ? results.length : 1;
    setStatus(`${n} result${n === 1 ? "" : "s"} for `, query);
  } catch (e) {
    const pre = document.createElement("div");
    pre.className = "err";
    pre.textContent = `query error: ${e.message}\n${e.stack || ""}`;
    out.appendChild(pre);
  }
}
window.runQuery = runQuery; // override DACTAL's so drill-down routes here

function renderCrumbs() {
  crumbs.textContent = "";
  history.slice(-8).forEach((q, i, arr) => {
    const a = document.createElement("a");
    a.textContent = q;
    a.onclick = () => { history = history.slice(0, history.indexOf(q) + 1); runQuery(q); };
    crumbs.appendChild(a);
    if (i < arr.length - 1) crumbs.appendChild(document.createTextNode("  ›  "));
  });
}

function setStatus(prefix, query) {
  statusEl.textContent = prefix + (query ? `“${query}”` : "");
}

const DEFAULT_DQ = "rows/source";

// Provider-agnostic examples (work over any Datalib corpus). The last
// one shows bracketing a value that contains spaces/parens — DACTAL treats
// space/`(`/`)` as syntax, so `kind=Slack Message` must be `kind=[Slack Message]`.
const EXAMPLES = [
  "rows",
  "rows/source",
  "rows/kind",
  "rows/author#-count",
  "rows#-when",
  "rows:kind=[Slack Message]",
];
const chips = document.getElementById("chips");
for (const ex of EXAMPLES) {
  const c = document.createElement("span");
  c.className = "chip";
  c.textContent = ex;
  c.onclick = () => runQuery(ex);
  chips.appendChild(c);
}

async function load(datalibQuery, initialDq) {
  setStatus("loading working set…");
  history = [];
  // Reset datasets so re-loading doesn't append duplicates.
  for (const k of Object.keys(dactal.data)) {
    if (!dactal.internal_datasets.includes(k)) delete dactal.data[k];
  }
  try {
    const resp = await fetchSearch(datalibQuery, 500);
    const summary = loadSearchIntoDactal(dactal, resp);
    const ents = Object.entries(summary.entities)
      .filter(([, n]) => n)
      .map(([k, n]) => `${k}:${n}`)
      .join("  ");
    statusEl.textContent =
      `loaded ${summary.rows} rows → datasets: rows + ${ents}.  Try a DACTAL query.`;
    runQuery(initialDq || DEFAULT_DQ);
  } catch (e) {
    statusEl.textContent = "";
    const err = document.createElement("span");
    err.className = "err";
    err.textContent = `the search failed (${e.message})`;
    statusEl.appendChild(err);
  }
}

const datalibInput = document.getElementById("datalib");
document.getElementById("datalibgo").onclick = () => load(datalibInput.value);
document.getElementById("dqgo").onclick = () => runQuery();
dqInput.addEventListener("keydown", (e) => { if (e.key === "Enter") runQuery(); });
datalibInput.addEventListener("keydown", (e) => { if (e.key === "Enter") load(e.target.value); });

// The card's arguments arrive from the host, not from the URL: a page
// that ran whatever `?dq=` it was opened with would evaluate a query
// for anyone who could make the user follow a link.
if (hosted) {
  awaitInit().then(({ load: initialLoad, dq }) => {
    datalibInput.value = initialLoad;
    load(initialLoad, dq || DEFAULT_DQ);
  });
} else {
  setStatus("This page runs inside a Datalib card — open it with dactalView().");
}
