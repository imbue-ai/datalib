// Builtin view: visualize the sync pipeline's step DAG.
//
// Structure comes from `GET /api/dag` — the same load → to_specs →
// Graph::build chain the runner executes, so the picture can't drift
// from what actually runs. Steps are laid out in dependency layers
// (left → right) with edges drawn between boxes.
//
// Live state rides the existing sync-progress SSE stream: the worker's
// task board is keyed by step id, so while a job runs each node is
// tinted by its task state (flashing yellow running, green done /
// skipped, red failed, muted blocked) — the DAG itself becomes the
// progress display. States are cleared when the next job starts and
// kept after a terminal event so the last run's outcome stays visible.
//
// Plain-DOM + SVG (no Vue, no chart lib): a handful of rects and
// bezier edges.
import type { CardRender } from "../types";
import {
  fetchActiveJobs,
  fetchDag,
  openJobStream,
  type DagStep,
  type JobProgressEvent,
  type SyncTask,
} from "@/api";
import { parseTasks } from "@/sync/progress";

const NODE_H = 30;
const NODE_GAP_Y = 14;
const LAYER_GAP_X = 56;
const PAD = 16;
const CHAR_W = 7.2; // ui-monospace @ 12px, close enough for sizing

type NodePos = {
  step: DagStep;
  x: number;
  y: number;
  w: number;
};

export function sourceDagView(): CardRender {
  return (root, ctx) => {
    ctx.setTitle("Pipeline DAG");
    const style = document.createElement("style");
    style.textContent = `
      .dv { font: 12px/1.4 ui-monospace, Menlo, monospace; color: var(--fw-fg, inherit); }
      .dv-head { padding: 8px 12px; opacity: .6; border-bottom: 1px solid var(--fw-border, #8884); font-family: system-ui, sans-serif; }
      .dv-scroll { overflow: auto; }
      .dv-error { padding: 16px 12px; color: #c0392b; white-space: pre-wrap; }
      .dv-empty { padding: 16px 12px; opacity: .5; }
      .dv-node { fill: var(--fw-bg, #fff); stroke: var(--fw-border, #888); rx: 6px; }
      .dv-node.todo { stroke-dasharray: 3 3; }
      .dv-node.running { fill: #d4a01755; stroke: #d4a017; animation: dv-flash 1s ease-in-out infinite; }
      .dv-node.done { fill: #2e8b5733; stroke: #2e8b57; }
      .dv-node.skipped { fill: #2e8b5718; stroke: #2e8b5788; }
      .dv-node.failed { fill: #c0392b33; stroke: #c0392b; }
      .dv-node.blocked { fill: #88888822; stroke: #888888; }
      @keyframes dv-flash { 0%, 100% { fill-opacity: 1; } 50% { fill-opacity: .35; } }
      .dv-label { fill: currentColor; }
      .dv-sub { fill: currentColor; opacity: .5; font-size: 10px; }
      .dv-edge { fill: none; stroke: var(--fw-border, #888); stroke-width: 1.2; opacity: .7; }
      .dv-legend { display: flex; gap: 1rem; padding: 6px 12px; opacity: .7; font-family: system-ui, sans-serif; font-size: 11px; border-top: 1px solid var(--fw-border, #8882); }
      .dv-dot { display: inline-block; width: .6em; height: .6em; border-radius: 2px; margin-right: .3em; }
    `;
    root.appendChild(style);

    const wrap = document.createElement("div");
    wrap.className = "dv";
    root.appendChild(wrap);

    let steps: DagStep[] = [];
    // step id → task state ("running" | "done" | …) from the live job.
    let states = new Map<string, string>();
    let disposed = false;

    function layout(list: DagStep[]): NodePos[] {
      // Layer = longest dependency chain below the node. The response
      // is topo-ordered, so one pass suffices.
      const layer = new Map<string, number>();
      for (const s of list) {
        const l =
          s.deps.length === 0
            ? 0
            : Math.max(...s.deps.map((d) => layer.get(d) ?? 0)) + 1;
        layer.set(s.id, l);
      }
      const perLayer = new Map<number, DagStep[]>();
      for (const s of list) {
        const l = layer.get(s.id)!;
        if (!perLayer.has(l)) perLayer.set(l, []);
        perLayer.get(l)!.push(s);
      }
      const widths = new Map<number, number>();
      for (const [l, ss] of perLayer) {
        widths.set(
          l,
          Math.max(...ss.map((s) => s.id.length * CHAR_W + 20), 60),
        );
      }
      const pos: NodePos[] = [];
      for (const [l, ss] of [...perLayer.entries()].sort((a, b) => a[0] - b[0])) {
        let x = PAD;
        for (let i = 0; i < l; i++) {
          x += (widths.get(i) ?? 60) + LAYER_GAP_X;
        }
        ss.forEach((s, i) => {
          pos.push({
            step: s,
            x,
            y: PAD + i * (NODE_H + NODE_GAP_Y),
            w: widths.get(l)!,
          });
        });
      }
      return pos;
    }

    function svgEl<K extends keyof SVGElementTagNameMap>(
      tag: K,
      attrs: Record<string, string>,
    ): SVGElementTagNameMap[K] {
      const el = document.createElementNS("http://www.w3.org/2000/svg", tag);
      for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, v);
      return el;
    }

    function paint(error?: string) {
      wrap.replaceChildren();
      const head = document.createElement("div");
      head.className = "dv-head";
      head.textContent =
        "step DAG — edges derived from artifact paths; live states from the sync stream";
      wrap.appendChild(head);

      if (error) {
        const e = document.createElement("div");
        e.className = "dv-error";
        e.textContent = `config does not build a DAG:\n${error}`;
        wrap.appendChild(e);
        return;
      }
      if (steps.length === 0) {
        const e = document.createElement("div");
        e.className = "dv-empty";
        e.textContent = "no steps configured yet — add sources in the Setup tab";
        wrap.appendChild(e);
        return;
      }

      const pos = layout(steps);
      const byId = new Map(pos.map((p) => [p.step.id, p]));
      const width = Math.max(...pos.map((p) => p.x + p.w)) + PAD;
      const height = Math.max(...pos.map((p) => p.y + NODE_H)) + PAD;

      const scroll = document.createElement("div");
      scroll.className = "dv-scroll";
      const svg = svgEl("svg", {
        width: String(width),
        height: String(height),
        viewBox: `0 0 ${width} ${height}`,
      });
      scroll.appendChild(svg);
      wrap.appendChild(scroll);

      // Edges under nodes.
      for (const p of pos) {
        for (const d of p.step.deps) {
          const from = byId.get(d);
          if (!from) continue;
          const x1 = from.x + from.w;
          const y1 = from.y + NODE_H / 2;
          const x2 = p.x;
          const y2 = p.y + NODE_H / 2;
          const mx = (x1 + x2) / 2;
          const path = svgEl("path", {
            class: "dv-edge",
            d: `M ${x1} ${y1} C ${mx} ${y1}, ${mx} ${y2}, ${x2} ${y2}`,
          });
          svg.appendChild(path);
        }
      }
      for (const p of pos) {
        const state = states.get(p.step.id) ?? "todo";
        const rect = svgEl("rect", {
          class: `dv-node ${state}`,
          x: String(p.x),
          y: String(p.y),
          width: String(p.w),
          height: String(NODE_H),
          rx: "6",
        });
        const title = svgEl("title", {});
        title.textContent = [
          p.step.id,
          `runs: ${p.step.command}`,
          p.step.inputs.length ? `reads: ${p.step.inputs.join(", ")}` : "reads: (nothing — download step)",
          `writes: ${p.step.outputs.join(", ")}`,
          state !== "todo" ? `state: ${state}` : "",
        ]
          .filter(Boolean)
          .join("\n");
        rect.appendChild(title);
        svg.appendChild(rect);

        const label = svgEl("text", {
          class: "dv-label",
          x: String(p.x + 10),
          y: String(p.y + 19),
        });
        label.textContent = p.step.id;
        svg.appendChild(label);
      }

      const legend = document.createElement("div");
      legend.className = "dv-legend";
      for (const [cls, name, color] of [
        ["running", "running", "#d4a017"],
        ["done", "done", "#2e8b57"],
        ["skipped", "up to date", "#2e8b5788"],
        ["failed", "failed", "#c0392b"],
        ["blocked", "blocked", "#888888"],
      ] as const) {
        const item = document.createElement("span");
        const dot = document.createElement("span");
        dot.className = "dv-dot";
        dot.style.background = color;
        item.appendChild(dot);
        item.appendChild(document.createTextNode(name));
        item.dataset.cls = cls;
        legend.appendChild(item);
      }
      wrap.appendChild(legend);
    }

    function applyTasks(tasks: SyncTask[] | null | undefined) {
      if (!tasks) return;
      const next = new Map<string, string>();
      for (const t of tasks) {
        if (t.state !== "todo") next.set(t.id, t.state);
      }
      states = next;
      paint();
    }

    async function load() {
      try {
        const dag = await fetchDag();
        if (disposed) return;
        steps = dag.steps;
        paint(dag.ok ? undefined : (dag.error ?? "unknown error"));
      } catch (e) {
        if (!disposed) paint((e as Error).message);
      }
      // Seed live state from any already-running job.
      try {
        const jobs = await fetchActiveJobs();
        const running = jobs.find((j) => j.state === "running");
        if (!disposed && running) applyTasks(parseTasks(running.progress_msg));
      } catch {
        // no live state — structure alone is fine
      }
    }

    const stream = openJobStream((ev: JobProgressEvent) => {
      if (disposed) return;
      applyTasks(ev.tasks ?? parseTasks(ev.progress_msg));
    });
    // The DAG changes when the config is saved; a slow poll keeps the
    // picture current without config-save push machinery.
    const refresh = setInterval(load, 15_000);
    void load();

    return () => {
      disposed = true;
      stream.close();
      clearInterval(refresh);
    };
  };
}
