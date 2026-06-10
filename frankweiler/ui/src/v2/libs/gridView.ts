// Prebuilt "grid" view factory — `gridView()` in card source returns
// a CardRender for a search bar + AG Grid combo. Uses AG Grid's
// vanilla `createGrid` (not the Vue wrapper) because the shadow root
// isn't a Vue subtree.
//
// Clicking a row opens a document column to the right via the host
// command API: the grid composes the new card's source
// (`documentView("md-uuid", "section-uuid")`) and calls
// `ctx.host.openColumn(source)`. Structure changes never go through
// the bus.
//
// Intentionally narrower than the v1 GridColumn.vue: no feedback
// modal, no context menus, no adaptive visibility, no account labels,
// no source icons, no qmd error banner. This is a port to validate
// that AG Grid runs inside a shadow root, not a feature replica.
import {
  ModuleRegistry,
  AllCommunityModule,
  themeQuartz,
  colorSchemeVariable,
  createGrid,
  type ColDef,
  type GridApi,
  type GridOptions,
  type RowSelectedEvent,
  type GetRowIdParams,
} from "ag-grid-community";
import { fetchSearch, type SearchRow } from "@/api";
import type { CardRender, Teardown } from "../types";

ModuleRegistry.registerModules([AllCommunityModule]);

const gridTheme = themeQuartz.withPart(colorSchemeVariable);

const COLUMN_DEFS: ColDef<SearchRow>[] = [
  { field: "when", headerName: "Time", width: 165 },
  { field: "source", headerName: "Source", width: 100 },
  { field: "kind", headerName: "Type", width: 110 },
  { field: "channel", headerName: "Channel", width: 130 },
  { field: "snippet", headerName: "Contents", flex: 1, minWidth: 200 },
  { field: "author", headerName: "Author", width: 130 },
];

const SEARCH_LIMIT = 5_000;

export function gridView(opts?: { q?: string }): CardRender {
  const initialQ = opts?.q ?? "";

  return (root, ctx) => {
    // Build the DOM scaffold inside the shadow root.
    const style = document.createElement("style");
    style.textContent = SHADOW_CSS;
    root.appendChild(style);

    const wrap = document.createElement("div");
    wrap.className = "grid-card";
    root.appendChild(wrap);

    const search = document.createElement("input");
    search.className = "search-input";
    search.type = "text";
    search.placeholder = "search messages…";
    search.value = initialQ;
    wrap.appendChild(search);

    const status = document.createElement("div");
    status.className = "status";
    status.textContent = "loading…";
    wrap.appendChild(status);

    const gridWrap = document.createElement("div");
    gridWrap.className = "grid-wrap";
    wrap.appendChild(gridWrap);

    let rows: SearchRow[] = [];
    let gridApi: GridApi<SearchRow> | null = null;
    let inflight: AbortController | null = null;
    let debounceTimer: ReturnType<typeof setTimeout> | null = null;
    let alive = true;

    const gridOptions: GridOptions<SearchRow> = {
      theme: gridTheme,
      animateRows: false,
      rowHeight: 38,
      rowSelection: { mode: "singleRow", enableClickSelection: true },
      columnDefs: COLUMN_DEFS,
      defaultColDef: { resizable: true, sortable: true, filter: true },
      getRowId: (p: GetRowIdParams<SearchRow>) => p.data.uuid,
      onRowSelected: (e: RowSelectedEvent<SearchRow>) => {
        if (!e.node.isSelected() || !e.data) return;
        const source = `documentView(${JSON.stringify(
          e.data.markdown_uuid,
        )}, ${JSON.stringify(e.data.uuid)})`;
        ctx.host.openColumn(source);
      },
    };

    gridApi = createGrid(gridWrap, gridOptions);

    async function runSearch(q: string) {
      inflight?.abort();
      inflight = new AbortController();
      status.textContent = "searching…";
      try {
        const r = await fetchSearch(q, SEARCH_LIMIT, inflight.signal);
        if (!alive) return;
        rows = r.rows;
        gridApi?.setGridOption("rowData", rows);
        status.textContent = `${rows.length} rows (of ${r.total_estimated})`;
      } catch (e) {
        if ((e as { name?: string }).name === "AbortError") return;
        if (!alive) return;
        status.textContent = `error: ${(e as Error).message}`;
      }
    }

    search.addEventListener("input", () => {
      if (debounceTimer) clearTimeout(debounceTimer);
      debounceTimer = setTimeout(() => runSearch(search.value), 150);
    });

    runSearch(initialQ);

    const teardown: Teardown = () => {
      alive = false;
      if (debounceTimer) clearTimeout(debounceTimer);
      inflight?.abort();
      gridApi?.destroy();
      gridApi = null;
    };
    return teardown;
  };
}

// Local copy of the bits of styling we need inside the shadow root.
// The host page's CSS doesn't pierce the boundary, so anything visual
// has to live here (or be loaded via the AG Grid theme, which already
// handles itself via constructable stylesheets).
const SHADOW_CSS = `
:host { display: block; height: 100%; }
.grid-card {
  display: flex;
  flex-direction: column;
  height: 100%;
  gap: 0.5rem;
  padding: 0.5rem;
  box-sizing: border-box;
  font: 14px system-ui, sans-serif;
  color: inherit;
}
.search-input {
  width: 100%;
  padding: 0.5rem 0.75rem;
  font-size: 1rem;
  box-sizing: border-box;
  border: 1px solid #888;
  border-radius: 4px;
  background: transparent;
  color: inherit;
}
.status {
  font-size: 0.85rem;
  opacity: 0.7;
}
.grid-wrap {
  flex: 1 1 auto;
  min-height: 200px;
}
`;
