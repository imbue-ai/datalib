// The column-type vocabulary, as AG Grid column definitions. A table's
// producer declares a `ColumnSpec` per column; `typedColumns` turns
// each into a `ColDef` whose cell is drawn by the column's *type* — a
// byte count as a size, an identity as its icon and label, a status as
// its glyph — rather than by comparing field names. Pure: it knows
// nothing about what the rows are, and owns no grid. `TableGrid.ce.vue`
// mounts one over these for the simple hosts; a card with a grid of
// its own (`GridCard`) calls this and keeps driving AG Grid itself.
import type { ColDef, ICellRendererComp, ICellRendererParams, ValueGetterParams } from "ag-grid-community";
import type { Action, Chip, ColumnSpec, ColumnType, Identity, StatusView, Timeseries } from "@/api";
import { iconUrl } from "@/config/icons";
import { STATUS_GLYPHS, STEP_GLYPHS, glyphSvg } from "@/config/glyphs";
import { calibrationMax, sparkline, type Sample } from "@/config/sparkline";
import { compareStamps, formatRelative, formatStamp } from "@/config/timeFormat";
import { formatBytes } from "@/config/bytes";

export type TypedColumnOptions<T> = {
  /// The rows the columns will draw, read when a cell needs the whole
  /// column — a `timeseries` sparkline is calibrated against the
  /// largest value any row reaches.
  rows: () => T[];
  /// The rows form a tree; the first column carries the chevron and
  /// shows each row's own id beside its label.
  tree?: boolean;
  /// How far back a `timeseries` cell's samples reach, in seconds.
  windowSecs?: number;
  /// What each action id does when its button is pressed. An id with
  /// no handler draws no button.
  actions?: Record<string, (row: T) => void>;
  /// An in-place edit of an `editable` cell was committed. The value
  /// never lands in the row; the card decides what it means.
  onEdit?: (row: T, field: string, value: string) => void;
  /// A `markdown_uuid` cell was clicked.
  onOpenDocument?: (uuid: string) => void;
  /// Per-field refinements a type cannot know — a width, a hover, a
  /// formatter — merged over the typed definition.
  overrides?: Record<string, ColDef<T>>;
};

/// The fields of `timestamp` type among the specs: the cells that read
/// "5 minutes ago" and go stale on their own, so the host repaints them
/// on a clock.
export function timestampFields(specs: ColumnSpec[]): string[] {
  return specs.filter((c) => c.type === "timestamp").map((c) => c.field);
}

// ── Icon tokens ──────────────────────────────────────────────────
// A producer names an icon as a token; this is where the token becomes
// a picture. Brand marks are bundled assets; `step:<phase>` and
// `applet` are the pipeline-role glyphs.

function iconFor(token: string | null | undefined, label: string): Element | null {
  if (!token) return null;
  const url = iconUrl(token);
  if (url) {
    const img = document.createElement("img");
    img.src = url;
    img.alt = label;
    img.className = "tg-brand";
    return img;
  }
  const glyph =
    token === "applet"
      ? STEP_GLYPHS.applet
      : token.startsWith("step:")
        ? STEP_GLYPHS[token.slice(5) as keyof typeof STEP_GLYPHS]
        : undefined;
  return glyph ? glyphSvg(glyph, label, 14) : null;
}

function formatUnit(n: number, unit: string): string {
  return unit === "bytes" ? formatBytes(n) : n.toLocaleString();
}

function none(): HTMLElement {
  const span = document.createElement("span");
  span.className = "tg-none";
  span.textContent = "—";
  return span;
}

function windowPhrase(secs: number): string {
  return secs % 60 === 0
    ? `the last ${secs / 60} minute${secs === 60 ? "" : "s"}`
    : `the last ${secs} seconds`;
}

const SPARK = { width: 120, height: 18 };

function sparkSvg(samples: Sample[], max: number, windowMs: number): SVGSVGElement | null {
  const spark = sparkline(samples, {
    nowMs: Date.now(),
    windowMs,
    max,
    width: SPARK.width,
    height: SPARK.height,
    inset: 0.5,
  });
  if (!spark) return null;
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", `0 0 ${SPARK.width} ${SPARK.height}`);
  svg.setAttribute("preserveAspectRatio", "none");
  svg.setAttribute("aria-hidden", "true");
  svg.classList.add("tg-spark");
  const area = document.createElementNS("http://www.w3.org/2000/svg", "polygon");
  area.setAttribute("points", spark.area);
  area.classList.add("tg-spark-area");
  svg.appendChild(area);
  const line = document.createElementNS("http://www.w3.org/2000/svg", "polyline");
  line.setAttribute("points", spark.line);
  line.classList.add("tg-spark-line");
  svg.appendChild(line);
  return svg;
}

// ── Cell renderers, one per type ─────────────────────────────────

function renderIdentity(
  v: Identity | null | undefined,
  isTreeColumn: boolean,
  isParent: boolean,
): HTMLElement {
  const wrap = document.createElement("span");
  wrap.className = "tg-identity";
  if (!v) return wrap;
  const icon = iconFor(v.icon, v.detail ?? v.label);
  // A brand mark leads; a role glyph follows the name, muted, because
  // the name is what the eye should land on and the glyph answers the
  // follow-up question.
  const brand = icon?.tagName === "IMG";
  if (icon && brand) wrap.appendChild(icon);
  const text = document.createElement("span");
  text.textContent = v.label;
  if (isParent) text.className = "tg-parent";
  text.title = v.detail ? `${v.id} — ${v.detail}` : v.id;
  wrap.appendChild(text);
  if (icon && !brand) {
    const mark = document.createElement("span");
    mark.className = "tg-mark";
    mark.title = v.detail ?? "";
    mark.appendChild(icon);
    wrap.appendChild(mark);
  }
  // The row's own id, where the column is the row's identity and the
  // label hides it.
  if (isTreeColumn && v.id !== v.label) {
    const id = document.createElement("span");
    id.className = "tg-id";
    id.textContent = v.id;
    id.title = `Id — stored in ${v.id}/ under the data root`;
    wrap.appendChild(id);
  }
  return wrap;
}

function renderStatus(s: StatusView | null | undefined): HTMLElement {
  const wrap = document.createElement("span");
  if (!s) return wrap;
  const { key, label } = s;
  wrap.className = `tg-status tg-status-${key.replace(/[\s_]+/g, "-")}`;
  // The word, and then why it is that word. The column is glyphs, so
  // this is the only place either appears.
  wrap.title = s.detail ? `${label} — ${s.detail}` : label;
  const spinnerOrGlyph = () => {
    if (key === "running") {
      // A still frame can't say "still going", so running is the one
      // state drawn rather than glyphed.
      const spin = document.createElement("span");
      spin.className = "tg-spinner";
      spin.setAttribute("role", "img");
      spin.setAttribute("aria-label", label);
      return spin;
    }
    const glyph = STATUS_GLYPHS[key];
    if (glyph) return glyphSvg(glyph, label);
    // A status this sheet hasn't met. Say the word rather than draw
    // nothing — an unknown state is exactly when a reader most needs
    // to know what it was.
    const word = document.createElement("span");
    word.textContent = label;
    word.setAttribute("role", "img");
    word.setAttribute("aria-label", label);
    return word;
  };
  wrap.appendChild(spinnerOrGlyph());
  if (s.segments) {
    // One segment per part, each in its own status colour, the running
    // one pulsing. No arithmetic across parts; the bar *is* the parts.
    const bar = document.createElement("span");
    bar.className = "tg-segs";
    for (const seg of s.segments) {
      const cell = document.createElement("span");
      cell.className = `tg-seg tg-seg-${seg.key.replace(/[\s_]+/g, "-")}`;
      cell.title = `${seg.id}: ${seg.label}`;
      bar.appendChild(cell);
    }
    wrap.appendChild(bar);
  } else if (key === "running" && s.fraction != null) {
    // A bar only when the thing said how much is ahead of it: a bar at
    // an invented fraction claims more than we know.
    const bar = document.createElement("span");
    bar.className = "tg-progress";
    const fill = document.createElement("span");
    fill.style.width = `${s.fraction * 100}%`;
    bar.appendChild(fill);
    wrap.appendChild(bar);
  }
  return wrap;
}

function renderChips(chips: Chip[] | null | undefined): HTMLElement {
  const wrap = document.createElement("span");
  wrap.className = "tg-chips";
  if (!chips?.length) return wrap;
  // The chips are cut at the column's edge; the whole row of them is
  // one hover away.
  wrap.title = chips.map((c) => c.text).join("  ");
  for (const chip of chips) {
    const el = document.createElement("span");
    el.className = `tg-chip tg-chip-${chip.kind}`;
    el.textContent = chip.text;
    el.title = chip.title;
    wrap.appendChild(el);
  }
  return wrap;
}

function renderTimestamp(iso: string | null | undefined): HTMLElement {
  if (!iso) return none();
  const span = document.createElement("span");
  span.textContent = formatRelative(iso, Date.now());
  // The exact stamp, for when "7 days ago" isn't the answer you needed.
  span.title = formatStamp(iso);
  return span;
}

function renderTimeseries(
  v: Timeseries | null | undefined,
  ceiling: number,
  windowSecs: number,
): HTMLElement {
  const wrap = document.createElement("span");
  wrap.className = "tg-series";
  if (!v || v.value === null) {
    // Null is "nothing measured yet", which is not a flat line at zero
    // — it's the absence of a plot.
    wrap.appendChild(none());
    if (v?.detail) wrap.title = v.detail;
    return wrap;
  }
  wrap.title =
    `${v.detail ?? formatUnit(v.value, v.unit)} · the line is ${windowPhrase(windowSecs)}, ` +
    `drawn against the largest row`;
  const track = document.createElement("span");
  track.className = "tg-plot";
  // No samples yet means the producer hasn't measured twice. The value
  // still shows; there is just nothing behind it to draw.
  const svg = sparkSvg(v.samples, ceiling, windowSecs * 1000);
  if (svg) track.appendChild(svg);
  const label = document.createElement("span");
  label.className = "tg-plot-label";
  label.textContent = formatUnit(v.value, v.unit);
  track.appendChild(label);
  wrap.appendChild(track);
  return wrap;
}

/// 24×24 Material-ish glyphs for the action ids the viewer knows a
/// picture for, drawn in `currentColor`. Any other id draws its label.
const ACTION_ICONS: Record<string, string> = {
  sync: "M8 5v14l11-7z",
  stop: "M6 6h12v12H6z",
};

/// A class rather than a function so that `refresh` can update the
/// buttons in place: a repaint runs on every job event — a few times a
/// second during a sync — and a function renderer is torn down and
/// rebuilt on each, so a click whose mousedown landed on the old button
/// and mouseup on its replacement fired nothing.
function actionsRenderer<T>(handlers: Record<string, (row: T) => void>) {
  return class implements ICellRendererComp<T> {
    private wrap!: HTMLSpanElement;
    private row!: T;
    private field!: string;
    private buttons = new Map<string, HTMLButtonElement>();

    init(p: ICellRendererParams<T>): void {
      this.row = p.data!;
      this.field = p.colDef?.field ?? "";
      this.wrap = document.createElement("span");
      this.wrap.className = "tg-actions";
      this.apply();
    }
    getGui(): HTMLElement {
      return this.wrap;
    }
    refresh(p: ICellRendererParams<T>): boolean {
      this.row = p.data!;
      this.apply();
      return true;
    }
    private apply(): void {
      const actions = ((this.row as Record<string, unknown>)[this.field] as Action[] | undefined) ?? [];
      const wanted = new Set<string>();
      for (const a of actions) {
        if (!handlers[a.id]) continue;
        wanted.add(a.id);
        let b = this.buttons.get(a.id);
        if (!b) {
          b = document.createElement("button");
          const glyph = ACTION_ICONS[a.id];
          if (glyph) {
            b.className = "tg-icon-btn";
            const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
            svg.setAttribute("viewBox", "0 0 24 24");
            svg.setAttribute("width", "15");
            svg.setAttribute("height", "15");
            svg.setAttribute("aria-hidden", "true");
            const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
            path.setAttribute("fill", "currentColor");
            path.setAttribute("d", glyph);
            svg.appendChild(path);
            b.appendChild(svg);
          } else {
            b.className = "tg-btn";
          }
          b.addEventListener("click", (e) => {
            e.stopPropagation();
            // The row current at click time, not the one the button was
            // built for.
            handlers[a.id]?.(this.row);
          });
          this.buttons.set(a.id, b);
        }
        if (!ACTION_ICONS[a.id]) b.textContent = a.label;
        b.title = a.enabled ? a.label : (a.disabled_reason ?? a.label);
        b.setAttribute("aria-label", a.label);
        b.disabled = !a.enabled;
        b.classList.toggle("danger", !!a.danger);
        this.wrap.appendChild(b);
      }
      for (const [id, b] of this.buttons) {
        if (!wanted.has(id)) {
          b.remove();
          this.buttons.delete(id);
        }
      }
    }
  };
}

// ── Column definitions, from the specs ───────────────────────────

const WIDTH: Record<ColumnType, number> = {
  text: 150,
  count: 90,
  number: 90,
  bytes: 110,
  timestamp: 150,
  datetime: 165,
  timeseries: 140,
  identity: 120,
  status: 96,
  chips: 260,
  actions: 64,
  markdown_uuid: 200,
};

export function typedColumns<T extends Record<string, unknown>>(
  specs: ColumnSpec[],
  opts: TypedColumnOptions<T>,
): ColDef<T>[] {
  const windowSecs = opts.windowSecs ?? 300;
  // Every sparkline in a column is drawn against the same ceiling —
  // the largest value any row in that column reaches — so heights
  // compare. Computed per paint, off the rows as they are then.
  const ceilingOf = (field: string) =>
    calibrationMax(
      opts.rows().map((r) => (r[field] as Timeseries | undefined) ?? { value: null, samples: [] }),
    );
  const Actions = actionsRenderer<T>(opts.actions ?? {});

  return specs.map((spec, index) => {
    const f = spec.field;
    const isTreeColumn = !!opts.tree && index === 0;
    const base: ColDef<T> = {
      field: f as ColDef<T>["field"],
      colId: f,
      headerName: spec.header,
      headerTooltip: spec.description,
      width: WIDTH[spec.type],
      minWidth: Math.min(WIDTH[spec.type], 70),
      hide: !spec.default_visible,
    };
    if (spec.editable) {
      base.editable = true;
      base.cellEditor = "agTextCellEditor";
      base.valueSetter = (p) => {
        const next = String(p.newValue ?? "").trim();
        if (p.data && next !== String(p.oldValue ?? "")) opts.onEdit?.(p.data, f, next);
        return false;
      };
    }
    const typed: ColDef<T> = (() => {
      switch (spec.type) {
        case "identity": {
          const inner = (p: ICellRendererParams<T>) =>
            renderIdentity(p.data?.[f] as Identity | null, isTreeColumn, !!p.node?.allChildrenCount);
          return {
            // Sort, filter and edit on the label, not the object.
            valueGetter: (p: ValueGetterParams<T>) => (p.data?.[f] as Identity | null)?.label ?? "",
            valueSetter: base.valueSetter,
            ...(isTreeColumn
              ? {
                  // The column the tree hangs off: AG Grid's group
                  // renderer draws the chevron and the indent, and hands
                  // the cell's content to the renderer.
                  flex: 1,
                  minWidth: 200,
                  showRowGroup: true,
                  cellRenderer: "agGroupCellRenderer",
                  cellRendererParams: { suppressCount: true, innerRenderer: inner },
                }
              : { cellRenderer: inner }),
          };
        }
        case "status":
          return {
            valueGetter: (p: ValueGetterParams<T>) => (p.data?.[f] as StatusView | null)?.label ?? "",
            cellRenderer: (p: ICellRendererParams<T>) => renderStatus(p.data?.[f] as StatusView | null),
          };
        case "chips":
          return {
            valueGetter: (p: ValueGetterParams<T>) =>
              ((p.data?.[f] as Chip[] | null) ?? []).map((c) => c.text).join("  "),
            cellRenderer: (p: ICellRendererParams<T>) => renderChips(p.data?.[f] as Chip[] | null),
          };
        case "timestamp":
          return {
            // Sort on the instant: an ISO string carrying its own offset
            // does not compare correctly as text.
            comparator: compareStamps,
            cellRenderer: (p: ICellRendererParams<T>) => renderTimestamp(p.data?.[f] as string | null),
          };
        case "datetime":
          return {
            comparator: compareStamps,
            valueFormatter: (p) => (typeof p.value === "string" && p.value ? formatStamp(p.value) : ""),
          };
        case "timeseries":
          return {
            valueGetter: (p: ValueGetterParams<T>) => (p.data?.[f] as Timeseries | null)?.value ?? null,
            cellRenderer: (p: ICellRendererParams<T>) =>
              renderTimeseries(p.data?.[f] as Timeseries | null, ceilingOf(f), windowSecs),
          };
        case "bytes":
          return {
            cellStyle: { "text-align": "right" },
            valueFormatter: (p) => (typeof p.value === "number" ? formatBytes(p.value) : ""),
            tooltipValueGetter: (p) => (typeof p.value === "number" ? `${p.value.toLocaleString()} bytes` : ""),
          };
        case "count":
          return {
            cellStyle: { "text-align": "right" },
            valueFormatter: (p) => (typeof p.value === "number" ? p.value.toLocaleString() : ""),
          };
        case "number":
          return {
            cellStyle: { "text-align": "right" },
            valueFormatter: (p) =>
              typeof p.value === "number"
                ? p.value.toLocaleString(undefined, { maximumFractionDigits: 3 })
                : "",
          };
        case "actions":
          return {
            sortable: false,
            filter: false,
            resizable: false,
            valueGetter: (p: ValueGetterParams<T>) => (p.data?.[f] as Action[] | null)?.map((a) => a.id).join(","),
            cellRenderer: Actions,
          };
        case "markdown_uuid":
          return {
            valueGetter: (p: ValueGetterParams<T>) => {
              const v = p.data?.[f] as Identity | string | null;
              return typeof v === "string" ? v : (v?.label ?? "");
            },
            cellRenderer: (p: ICellRendererParams<T>) => {
              const v = p.data?.[f] as Identity | string | null;
              const id = typeof v === "string" ? v : v?.id;
              const a = document.createElement("a");
              a.className = "tg-link";
              a.textContent = typeof v === "string" ? v : (v?.label ?? "");
              a.href = "#";
              a.addEventListener("click", (e) => {
                e.preventDefault();
                e.stopPropagation();
                if (id) opts.onOpenDocument?.(id);
              });
              return a;
            },
          };
        case "text":
          return {};
      }
    })();
    return { ...base, ...typed, ...opts.overrides?.[f] };
  });
}
