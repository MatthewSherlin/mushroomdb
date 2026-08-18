import { ApiClient, type QueryResult } from "./api";
import { highlightHtml } from "./cypher";
import {
  addHarvestedToCanvas,
  formatTable,
  harvestDecision,
  queryErrorText,
  resultAfterRun,
} from "./query-result";
import type { GraphStore } from "./store";

export type QueryConsoleOptions = {
  api: ApiClient;
  store: GraphStore;
  onCanvasChange?: () => void;
  onOpenChange?: (open: boolean) => void;
};

const STARTER = "MATCH (n) RETURN n LIMIT 25";

/**
 * Bottom Cypher drawer. Graph work delegates to {@link addHarvestedToCanvas};
 * this file only owns DOM.
 */
export class QueryConsole {
  private readonly host: HTMLElement;
  private readonly api: ApiClient;
  private readonly store: GraphStore;
  private readonly onCanvasChange: (() => void) | undefined;
  private readonly onOpenChange: ((open: boolean) => void) | undefined;

  private readonly root: HTMLElement;
  private readonly highlight: HTMLElement;
  private readonly editor: HTMLTextAreaElement;
  private readonly runBtn: HTMLButtonElement;
  private readonly addBtn: HTMLButtonElement;
  private readonly hint: HTMLElement;
  private readonly errorEl: HTMLElement;
  private readonly tableWrap: HTMLElement;

  private result: QueryResult | undefined;
  private busy = false;
  private open = false;
  private readonly onResize = (): void => {
    if (this.open) {
      this.syncHeight();
    }
  };

  constructor(host: HTMLElement, options: QueryConsoleOptions) {
    this.host = host;
    this.api = options.api;
    this.store = options.store;
    this.onCanvasChange = options.onCanvasChange;
    this.onOpenChange = options.onOpenChange;

    this.root = el("section", "console");
    this.root.hidden = true;
    this.root.setAttribute("aria-label", "Query console");

    const bar = el("div", "console-bar");
    this.runBtn = el("button", "console-btn") as HTMLButtonElement;
    this.runBtn.type = "button";
    this.runBtn.textContent = "Run";
    this.runBtn.addEventListener("click", () => {
      void this.run();
    });

    this.addBtn = el("button", "console-btn") as HTMLButtonElement;
    this.addBtn.type = "button";
    this.addBtn.textContent = "Add to canvas";
    this.addBtn.addEventListener("click", () => {
      void this.addToCanvas();
    });

    this.hint = el("p", "console-hint");

    const hide = el("button", "console-btn console-hide") as HTMLButtonElement;
    hide.type = "button";
    hide.textContent = "Hide";
    hide.addEventListener("click", () => {
      this.close();
    });

    bar.append(this.runBtn, this.addBtn, this.hint, hide);

    const editorWrap = el("div", "console-editor");
    this.highlight = el("pre", "console-hl");
    this.highlight.setAttribute("aria-hidden", "true");
    this.editor = document.createElement("textarea");
    this.editor.className = "console-input";
    this.editor.spellcheck = false;
    this.editor.wrap = "off";
    this.editor.setAttribute("aria-label", "Cypher");
    this.editor.value = STARTER;
    this.editor.addEventListener("input", () => {
      this.syncHighlight();
    });
    this.editor.addEventListener("scroll", () => {
      this.highlight.scrollTop = this.editor.scrollTop;
      this.highlight.scrollLeft = this.editor.scrollLeft;
    });
    this.editor.addEventListener("keydown", (event) => {
      if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
        event.preventDefault();
        void this.run();
      }
    });
    editorWrap.append(this.highlight, this.editor);

    this.errorEl = el("div", "console-error");
    this.errorEl.hidden = true;

    this.tableWrap = el("div", "console-table-wrap");

    this.root.append(bar, editorWrap, this.errorEl, this.tableWrap);
    host.append(this.root);

    this.syncHighlight();
    this.syncAdd();
    window.addEventListener("resize", this.onResize);
  }

  get isOpen(): boolean {
    return this.open;
  }

  toggle(): void {
    if (this.open) {
      this.close();
      return;
    }
    this.show();
  }

  show(): void {
    this.open = true;
    this.root.hidden = false;
    this.syncHeight();
    this.editor.focus();
    this.onOpenChange?.(true);
  }

  close(): void {
    if (!this.open) {
      return;
    }
    this.open = false;
    this.root.hidden = true;
    this.host.style.removeProperty("--console-h");
    this.onOpenChange?.(false);
  }

  destroy(): void {
    window.removeEventListener("resize", this.onResize);
    this.close();
    this.root.remove();
  }

  private async run(): Promise<void> {
    if (this.busy) {
      return;
    }
    this.busy = true;
    this.runBtn.disabled = true;
    try {
      const result = await this.api.query(this.editor.value);
      this.result = resultAfterRun({ ok: true, value: result });
      this.errorEl.hidden = true;
      this.errorEl.textContent = "";
      this.renderTable(result);
      this.syncAdd();
    } catch (err: unknown) {
      this.result = resultAfterRun({ ok: false });
      this.errorEl.textContent = queryErrorText(err);
      this.errorEl.hidden = false;
      this.syncAdd();
    } finally {
      this.busy = false;
      this.runBtn.disabled = false;
    }
  }

  private async addToCanvas(): Promise<void> {
    if (this.busy || this.result === undefined) {
      return;
    }
    const decision = harvestDecision(this.result);
    if (decision.blocked !== undefined) {
      return;
    }
    this.busy = true;
    this.addBtn.disabled = true;
    try {
      await addHarvestedToCanvas(this.store, this.api, this.result);
      this.onCanvasChange?.();
    } catch (err: unknown) {
      this.errorEl.textContent = queryErrorText(err);
      this.errorEl.hidden = false;
    } finally {
      this.busy = false;
      this.syncAdd();
    }
  }

  private renderTable(result: QueryResult): void {
    const shaped = formatTable(result);
    const table = document.createElement("table");
    table.className = "console-table";
    const thead = document.createElement("thead");
    const headRow = document.createElement("tr");
    for (const col of shaped.columns) {
      const th = document.createElement("th");
      th.textContent = col;
      headRow.append(th);
    }
    thead.append(headRow);
    const tbody = document.createElement("tbody");
    for (const row of shaped.rows) {
      const tr = document.createElement("tr");
      for (const cell of row) {
        const td = document.createElement("td");
        td.textContent = cell;
        tr.append(td);
      }
      tbody.append(tr);
    }
    table.append(thead, tbody);
    this.tableWrap.replaceChildren(table);
  }

  private syncAdd(): void {
    const decision = harvestDecision(this.result);
    this.addBtn.disabled = decision.blocked !== undefined;
    if (decision.blocked !== undefined) {
      this.addBtn.title = decision.blocked;
      this.hint.textContent = decision.blocked;
    } else {
      this.addBtn.removeAttribute("title");
      this.hint.textContent = "";
    }
  }

  private syncHighlight(): void {
    this.highlight.innerHTML = `${highlightHtml(this.editor.value)}\n`;
  }

  private syncHeight(): void {
    this.host.style.setProperty("--console-h", `${this.root.offsetHeight}px`);
  }
}

function el(tag: string, className?: string): HTMLElement {
  const node = document.createElement(tag);
  if (className !== undefined) {
    node.className = className;
  }
  return node;
}
