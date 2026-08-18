import { Graph } from "@cosmos.gl/graph";
import { ApiClient, ApiError } from "./api";
import { QueryConsole } from "./console";
import { expandNode, loadDemoNeighborhood } from "./expand";
import { GlowQueue, bornEdgeIds } from "./glow";
import { Inspector } from "./inspector";
import {
  flattenColors,
  flattenLinks,
  formatHoverCard,
  nextPositions,
  paintExplorer,
  visibleEdgeIds,
} from "./paint";
import { COLOR, GraphStore } from "./store";
import type { MutationEvent } from "./watch";

const CLICK_EXPAND_MS = 280;
const POINT_SIZE = 7;
const LINK_WIDTH = 1.15;
const GLOW_WIDTH = 2.4;

export type WatchDot = "idle" | "connected" | "reconnecting";

export type ExplorerOptions = {
  api: ApiClient;
  store: GraphStore;
  onEdgeSelect?: (id: string) => void;
};

/**
 * Thin cosmos.gl + chrome view over {@link GraphStore}.
 *
 * v1: the queried root's label stays blank until a later neighbor-expansion
 * (or a watch event) includes it — neighborhood rows omit the root and there
 * is no node-info endpoint. Blank-label nodes render muted paper and get no
 * label chip.
 */
export class Explorer {
  private readonly host: HTMLElement;
  private readonly api: ApiClient;
  private readonly store: GraphStore;
  private readonly onEdgeSelect: ((id: string) => void) | undefined;
  private readonly glow = new GlowQueue();
  private readonly queryConsole: QueryConsole;
  private readonly inspector: Inspector;

  private readonly rail: HTMLElement;
  private readonly exploreBtn: HTMLButtonElement;
  private readonly consoleBtn: HTMLButtonElement;
  private readonly rulesBtn: HTMLButtonElement;
  private readonly wordmark: HTMLElement;
  private readonly statusDot: HTMLElement;
  private readonly stage: HTMLElement;
  private readonly canvasHost: HTMLDivElement;
  private readonly emptyEl: HTMLElement;
  private readonly hoverEl: HTMLElement;
  private readonly labelsEl: HTMLElement;
  private readonly errorEl: HTMLElement;

  private graph: Graph | undefined;
  private lastKeys: string[] = [];
  private lastPos = new Map<string, [number, number]>();
  private lastFitCount = -1;
  private hoverKey: string | undefined;
  private clickTimer: number | undefined;
  private glowTimer: number | undefined;
  private tail: Promise<void> = Promise.resolve();

  constructor(host: HTMLElement, options: ExplorerOptions) {
    this.host = host;
    this.api = options.api;
    this.store = options.store;
    this.onEdgeSelect = options.onEdgeSelect;

    host.innerHTML = "";
    host.classList.add("shell");

    this.wordmark = el("div", "wordmark");
    const name = el("span", "wordmark-name");
    name.textContent = "graph-db";
    this.statusDot = el("span", "status-dot");
    this.statusDot.dataset.watch = "idle";
    this.statusDot.setAttribute("aria-label", "watch status");
    this.wordmark.append(name, this.statusDot);

    this.rail = el("nav", "rail");
    this.rail.setAttribute("aria-label", "views");
    this.exploreBtn = railButton("explore", "Explore", ICON_EXPLORE, true);
    this.consoleBtn = railButton("console", "Console", ICON_CONSOLE, false);
    this.rulesBtn = railButton("rules", "Rules", ICON_RULES, false);
    this.consoleBtn.setAttribute("aria-expanded", "false");
    this.rulesBtn.setAttribute("aria-expanded", "false");
    this.rail.append(this.exploreBtn, this.consoleBtn, this.rulesBtn);
    this.exploreBtn.addEventListener("click", () => {
      this.queryConsole.close();
      this.inspector.closeRules();
    });
    this.consoleBtn.addEventListener("click", () => {
      this.queryConsole.toggle();
    });
    this.rulesBtn.addEventListener("click", () => {
      this.inspector.toggleRules();
    });

    this.stage = el("div", "stage");
    this.canvasHost = el("div", "canvas-host") as HTMLDivElement;
    this.labelsEl = el("div", "label-layer");
    this.labelsEl.setAttribute("aria-hidden", "true");

    this.emptyEl = el("div", "empty");
    const emptyCopy = el("p");
    emptyCopy.textContent = "Open a node to start";
    const demoBtn = el("button", "empty-action") as HTMLButtonElement;
    demoBtn.type = "button";
    demoBtn.textContent = "Load demo neighborhood";
    demoBtn.addEventListener("click", () => {
      this.run(() => this.loadDemo());
    });
    this.emptyEl.append(emptyCopy, demoBtn);

    this.hoverEl = el("div", "hover-card");
    this.hoverEl.hidden = true;

    this.errorEl = el("div", "error-strip");
    this.errorEl.hidden = true;

    this.stage.append(
      this.canvasHost,
      this.labelsEl,
      this.emptyEl,
      this.hoverEl,
      this.errorEl,
    );
    host.append(this.rail, this.wordmark, this.stage);

    this.queryConsole = new QueryConsole(host, {
      api: this.api,
      store: this.store,
      onCanvasChange: () => {
        this.paint();
        this.scheduleFit();
      },
      onOpenChange: (open) => {
        this.syncRail(open ? "console" : this.inspector.isRulesOpen ? "rules" : "explore");
      },
    });
    this.inspector = new Inspector(host, {
      api: this.api,
      store: this.store,
      onNeedPaint: () => {
        this.paint();
      },
      onOpenChange: (which, open) => {
        if (which === "rules") {
          this.syncRail(
            open ? "rules" : this.queryConsole.isOpen ? "console" : "explore",
          );
        }
      },
    });

    this.canvasHost.addEventListener(
      "dblclick",
      (event) => {
        event.preventDefault();
        event.stopPropagation();
      },
      true,
    );

    this.paint();
  }

  setWatchStatus(state: WatchDot): void {
    this.statusDot.dataset.watch = state;
  }

  applyWatchEvent(event: MutationEvent): void {
    const before = [...this.store.edges.keys()];
    this.store.apply(event);
    this.inspector.closeIfEdgeMissing();
    if (!prefersReducedMotion()) {
      const born = bornEdgeIds(before, this.store.edges.keys());
      if (born.length > 0) {
        this.glow.schedule(born, performance.now());
        this.armGlow();
      }
    }
    this.paint();
  }

  destroy(): void {
    this.clearClick();
    if (this.glowTimer !== undefined) {
      window.clearTimeout(this.glowTimer);
      this.glowTimer = undefined;
    }
    this.graph?.destroy();
    this.graph = undefined;
    this.queryConsole.destroy();
    this.inspector.destroy();
    this.host.replaceChildren();
  }

  private syncRail(view: "explore" | "console" | "rules"): void {
    const consoleOpen = this.queryConsole.isOpen;
    const rulesOpen = this.inspector.isRulesOpen;
    this.consoleBtn.setAttribute("aria-expanded", consoleOpen ? "true" : "false");
    this.rulesBtn.setAttribute("aria-expanded", rulesOpen ? "true" : "false");
    setCurrent(this.exploreBtn, view === "explore");
    setCurrent(this.consoleBtn, view === "console");
    setCurrent(this.rulesBtn, view === "rules");
  }

  private async loadDemo(): Promise<void> {
    this.clearError();
    await loadDemoNeighborhood(this.store, this.api);
    this.paint();
    this.scheduleFit();
  }

  private async expand(key: string, depth: 1 | 2): Promise<void> {
    this.clearError();
    await expandNode(this.store, this.api, key, depth);
    this.paint();
    this.scheduleFit();
  }

  private scheduleFit(): void {
    const graph = this.graph;
    if (graph === undefined) {
      return;
    }
    const run = (): void => {
      graph.fitView(prefersReducedMotion() ? 0 : 280, 0.22);
    };
    void graph.ready.then(() => {
      run();
      window.requestAnimationFrame(() => {
        window.requestAnimationFrame(run);
      });
    });
  }

  private run(fn: () => Promise<void>): void {
    this.tail = this.tail
      .catch(() => undefined)
      .then(fn)
      .catch((err: unknown) => {
        this.showError(err);
      });
  }

  private paint(): void {
    this.inspector.closeIfEdgeMissing();
    const now = performance.now();
    const snap = paintExplorer(
      this.store,
      this.store.toCosmos(),
      new Set(this.glow.active(now)),
      this.inspector.highlightIds,
    );
    const empty = snap.pointKeys.length === 0;
    this.emptyEl.hidden = !empty;

    if (empty) {
      this.labelsEl.replaceChildren();
      this.hoverEl.hidden = true;
      return;
    }

    this.captureLivePositions();
    const { positions, map } = nextPositions(snap.pointKeys, this.lastPos);
    this.lastPos = map;
    this.lastKeys = snap.pointKeys;

    const graph = this.ensureGraph();
    const glowing = new Set(this.glow.active(now));
    const edgeIds = visibleEdgeIds(this.store);
    const highlighted = this.inspector.highlightIds;
    const widths = new Float32Array(
      edgeIds.map((id) =>
        glowing.has(id) || highlighted.has(id) ? GLOW_WIDTH : LINK_WIDTH,
      ),
    );
    const sizes = new Float32Array(snap.pointKeys.length);
    sizes.fill(POINT_SIZE);

    graph.setPointPositions(positions);
    graph.setPointColors(flattenColors(snap.pointColors));
    graph.setPointSizes(sizes);
    graph.setLinks(flattenLinks(snap.links));
    graph.setLinkColors(flattenColors(snap.linkColors));
    graph.setLinkWidths(widths);
    graph.setLinkArrows(snap.links.map(() => true));

    const selectedNode =
      this.store.selection?.kind === "node" ? this.store.selection.id : undefined;
    const selectedEdge =
      this.store.selection?.kind === "edge" ? this.store.selection.id : undefined;
    const focusedPoint =
      selectedNode === undefined ? undefined : snap.pointKeys.indexOf(selectedNode);
    const focusedLink =
      selectedEdge === undefined ? undefined : edgeIds.indexOf(selectedEdge);

    graph.setConfigPartial({
      focusedPointIndex:
        focusedPoint !== undefined && focusedPoint >= 0 ? focusedPoint : undefined,
      focusedLinkIndex:
        focusedLink !== undefined && focusedLink >= 0 ? focusedLink : undefined,
    });

    graph.render();
    if (this.lastKeys.length !== this.lastFitCount) {
      this.lastFitCount = this.lastKeys.length;
      this.scheduleFit();
    }

    const labeled: number[] = [];
    for (let i = 0; i < snap.pointKeys.length; i++) {
      const node = this.store.nodes.get(snap.pointKeys[i]!);
      if (node !== undefined && node.label !== "") {
        labeled.push(i);
      }
    }
    graph.trackPointPositionsByIndices(labeled);
    this.syncLabels();
  }

  private ensureGraph(): Graph {
    if (this.graph !== undefined) {
      return this.graph;
    }
    const rgba = (c: readonly [number, number, number, number]) =>
      [c[0], c[1], c[2], c[3]] as [number, number, number, number];
    const graph = new Graph(this.canvasHost, {
      backgroundColor: rgba(COLOR.ink),
      pointDefaultColor: rgba(COLOR.paper),
      linkDefaultColor: rgba(COLOR.structure),
      hoveredPointRingColor: rgba(COLOR.signal),
      focusedPointRingColor: rgba(COLOR.signal),
      hoveredLinkColor: rgba(COLOR.signal),
      outlinedPointRingColor: rgba(COLOR.signal),
      renderHoveredPointRing: true,
      enableDrag: true,
      enableZoom: true,
      enableSimulation: false,
      attribution: "",
      randomSeed: 1,
      fitViewOnInit: false,
      rescalePositions: true,
      transitionDuration: 0,
      scalePointsOnZoom: true,
      pointDefaultSize: POINT_SIZE,
      linkDefaultWidth: LINK_WIDTH,
      hoveredLinkWidthIncrease: 2,
      focusedLinkWidthIncrease: 2,
      linkVisibilityDistanceRange: [8, 800],
      linkArrowsSizeScale: 0.7,
      onPointClick: (index, _pos, event) => {
        this.onPointClick(index, event);
      },
      onLinkClick: (linkIndex) => {
        this.onLinkClick(linkIndex);
      },
      onBackgroundClick: () => {
        this.store.select(null);
        this.inspector.closeWhy();
        this.paint();
      },
      onPointMouseOver: (index) => {
        this.showHover(index);
      },
      onPointMouseOut: () => {
        this.hoverKey = undefined;
        this.hoverEl.hidden = true;
      },
      onSimulationTick: () => {
        this.syncLabels();
        this.syncHover();
      },
      onZoom: () => {
        this.syncLabels();
        this.syncHover();
      },
    });
    this.graph = graph;
    return graph;
  }

  private onPointClick(index: number | undefined, event?: MouseEvent): void {
    if (
      index === undefined ||
      !Number.isInteger(index) ||
      index < 0 ||
      index >= this.lastKeys.length
    ) {
      return;
    }
    const key = this.lastKeys[index];
    if (key === undefined) {
      return;
    }
    this.store.select({ kind: "node", id: key });
    this.inspector.closeWhy();
    this.paint();
    if (event !== undefined && event.detail >= 2) {
      this.clearClick();
      this.run(() => this.expand(key, 2));
      return;
    }
    this.clearClick();
    this.clickTimer = window.setTimeout(() => {
      this.clickTimer = undefined;
      this.run(() => this.expand(key, 1));
    }, CLICK_EXPAND_MS);
  }

  private onLinkClick(linkIndex: number): void {
    const id = visibleEdgeIds(this.store)[linkIndex];
    if (id === undefined) {
      return;
    }
    this.store.select({ kind: "edge", id });
    this.onEdgeSelect?.(id);
    this.run(() => this.inspector.openWhy(id));
    this.paint();
  }

  private showHover(index: number): void {
    const key = this.lastKeys[index];
    const node = key === undefined ? undefined : this.store.nodes.get(key);
    if (key === undefined || node === undefined) {
      this.hoverEl.hidden = true;
      return;
    }
    this.hoverKey = key;
    this.hoverEl.replaceChildren();
    const lines = formatHoverCard(node);
    for (let i = 0; i < lines.length; i++) {
      const line = el("div", i === 0 ? "hover-key" : i === 1 && node.label !== "" ? "hover-label" : "hover-props");
      line.textContent = lines[i]!;
      this.hoverEl.append(line);
    }
    this.hoverEl.hidden = false;
    this.syncHover();
  }

  private syncHover(): void {
    if (this.hoverKey === undefined || this.hoverEl.hidden || this.graph === undefined) {
      return;
    }
    const index = this.lastKeys.indexOf(this.hoverKey);
    if (index < 0) {
      this.hoverEl.hidden = true;
      return;
    }
    const live = this.graph.getPointPositions();
    const x = live[index * 2];
    const y = live[index * 2 + 1];
    if (x === undefined || y === undefined) {
      return;
    }
    const [sx, sy] = this.graph.spaceToScreenPosition([x, y]);
    const rect = this.canvasHost.getBoundingClientRect();
    const left = rect.left + sx + 14;
    const top = rect.top + sy + 14;
    this.hoverEl.style.left = `${Math.min(left, window.innerWidth - 180)}px`;
    this.hoverEl.style.top = `${Math.min(top, window.innerHeight - 80)}px`;
  }

  private syncLabels(): void {
    if (this.graph === undefined) {
      return;
    }
    const tracked = this.graph.getTrackedPointPositionsMap();
    const next: HTMLElement[] = [];
    for (const [index, pos] of tracked) {
      const key = this.lastKeys[index];
      const node = key === undefined ? undefined : this.store.nodes.get(key);
      if (node === undefined || node.label === "") {
        continue;
      }
      const [sx, sy] = this.graph.spaceToScreenPosition(pos);
      const chip = el("div", "label-chip");
      chip.textContent = node.label;
      chip.style.left = `${sx}px`;
      chip.style.top = `${sy}px`;
      next.push(chip);
    }
    this.labelsEl.replaceChildren(...next);
  }

  private captureLivePositions(): void {
    if (this.graph === undefined || this.lastKeys.length === 0) {
      return;
    }
    const live = this.graph.getPointPositions();
    for (let i = 0; i < this.lastKeys.length; i++) {
      const x = live[i * 2];
      const y = live[i * 2 + 1];
      if (x === undefined || y === undefined || !Number.isFinite(x) || !Number.isFinite(y)) {
        continue;
      }
      this.lastPos.set(this.lastKeys[i]!, [x, y]);
    }
  }

  private armGlow(): void {
    if (this.glowTimer !== undefined) {
      window.clearTimeout(this.glowTimer);
      this.glowTimer = undefined;
    }
    const expiry = this.glow.nextExpiry();
    if (expiry === undefined) {
      return;
    }
    const delay = Math.max(0, expiry - performance.now());
    this.glowTimer = window.setTimeout(() => {
      this.glowTimer = undefined;
      this.glow.prune(performance.now());
      this.paint();
      this.armGlow();
    }, delay);
  }

  private clearClick(): void {
    if (this.clickTimer !== undefined) {
      window.clearTimeout(this.clickTimer);
      this.clickTimer = undefined;
    }
  }

  private showError(err: unknown): void {
    const message =
      err instanceof ApiError
        ? err.message
        : err instanceof Error
          ? err.message
          : String(err);
    this.errorEl.textContent = message;
    this.errorEl.hidden = false;
  }

  private clearError(): void {
    this.errorEl.hidden = true;
    this.errorEl.textContent = "";
  }
}

function setCurrent(btn: HTMLElement, on: boolean): void {
  if (on) {
    btn.setAttribute("aria-current", "page");
  } else {
    btn.removeAttribute("aria-current");
  }
}

function prefersReducedMotion(): boolean {
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

function el(tag: string, className?: string): HTMLElement {
  const node = document.createElement(tag);
  if (className !== undefined) {
    node.className = className;
  }
  return node;
}

function railButton(
  view: string,
  label: string,
  svg: string,
  current: boolean,
): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "rail-btn";
  btn.dataset.view = view;
  btn.setAttribute("aria-label", label);
  if (current) {
    btn.setAttribute("aria-current", "page");
  }
  btn.innerHTML = svg;
  return btn;
}

const ICON_EXPLORE = `<svg viewBox="0 0 20 20" aria-hidden="true"><circle cx="6" cy="10" r="2" fill="none" stroke="currentColor" stroke-width="1.4"/><circle cx="14" cy="5.5" r="2" fill="none" stroke="currentColor" stroke-width="1.4"/><circle cx="14" cy="14.5" r="2" fill="none" stroke="currentColor" stroke-width="1.4"/><path d="M8 10h4M12.4 6.8 8 9.2M12.4 13.2 8 10.8" fill="none" stroke="currentColor" stroke-width="1.4"/></svg>`;

const ICON_CONSOLE = `<svg viewBox="0 0 20 20" aria-hidden="true"><path d="M5 6.5 9 10l-4 3.5" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="square"/><path d="M10.5 14.5H15" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="square"/></svg>`;

const ICON_RULES = `<svg viewBox="0 0 20 20" aria-hidden="true"><path d="M5 6h10M5 10h7M5 14h4" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="square"/></svg>`;
