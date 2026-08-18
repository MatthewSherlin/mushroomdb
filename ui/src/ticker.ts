import { TickerBuffer } from "./live";

/**
 * Collapsible mono activity list. Lines come from {@link TickerBuffer}.
 */
export class ActivityTicker {
  private readonly root: HTMLElement;
  private readonly lastEl: HTMLElement;
  private readonly listEl: HTMLElement;
  private readonly toggle: HTMLButtonElement;
  private readonly buf = new TickerBuffer();
  private open = false;

  constructor(host: HTMLElement) {
    this.root = document.createElement("div");
    this.root.className = "ticker";

    this.toggle = document.createElement("button");
    this.toggle.type = "button";
    this.toggle.className = "ticker-toggle";
    this.toggle.textContent = "Activity";
    this.toggle.setAttribute("aria-expanded", "false");
    this.toggle.addEventListener("click", () => {
      this.open = !this.open;
      this.toggle.setAttribute("aria-expanded", this.open ? "true" : "false");
      this.listEl.hidden = !this.open;
      this.lastEl.hidden = this.open;
    });

    this.lastEl = document.createElement("div");
    this.lastEl.className = "ticker-last";
    this.lastEl.textContent = "no events";

    this.listEl = document.createElement("ol");
    this.listEl.className = "ticker-list";
    this.listEl.hidden = true;

    this.root.append(this.toggle, this.lastEl, this.listEl);
    host.append(this.root);
  }

  push(line: string): void {
    this.buf.push(line);
    this.lastEl.textContent = line;
    this.syncList();
  }

  destroy(): void {
    this.root.remove();
  }

  private syncList(): void {
    const items = this.buf.lines().map((line) => {
      const li = document.createElement("li");
      li.textContent = line;
      return li;
    });
    this.listEl.replaceChildren(...items);
    const last = items[items.length - 1];
    last?.scrollIntoView({ block: "nearest" });
  }
}
