import { ApiClient, type RuleStats } from "./api";
import { queryErrorText } from "./query-result";
import type { GraphStore } from "./store";
import {
  buildWhyModel,
  ensureProvenance,
  formatScore,
  highlightedIdsForRule,
  loadNodeProps,
  whyEdgeMissing,
  type TokenMark,
  type WhyModel,
} from "./why";

export type InspectorOptions = {
  api: ApiClient;
  store: GraphStore;
  onNeedPaint?: () => void;
  onOpenChange?: (which: "rules" | "why", open: boolean) => void;
};

/**
 * Right why slide-over + left rules list. Arithmetic lives in {@link buildWhyModel}.
 */
export class Inspector {
  private readonly api: ApiClient;
  private readonly store: GraphStore;
  private readonly onNeedPaint: (() => void) | undefined;
  private readonly onOpenChange:
    | ((which: "rules" | "why", open: boolean) => void)
    | undefined;

  private readonly whyEl: HTMLElement;
  private readonly whyBody: HTMLElement;
  private readonly rulesEl: HTMLElement;
  private readonly rulesList: HTMLElement;
  private readonly rulesError: HTMLElement;

  private whyOpen = false;
  private whyEdgeId: string | undefined;
  private rulesOpen = false;
  private highlightedRule: string | undefined;
  private highlighted = new Set<string>();

  constructor(host: HTMLElement, options: InspectorOptions) {
    this.api = options.api;
    this.store = options.store;
    this.onNeedPaint = options.onNeedPaint;
    this.onOpenChange = options.onOpenChange;

    this.whyEl = el("aside", "why");
    this.whyEl.setAttribute("aria-label", "Why");
    this.whyEl.setAttribute("aria-hidden", "true");
    const whyHead = el("div", "panel-head");
    const whyTitle = el("h2", "panel-title");
    whyTitle.textContent = "Why";
    const whyHide = button("Hide", "console-btn");
    whyHide.addEventListener("click", () => {
      this.closeWhy();
    });
    whyHead.append(whyTitle, whyHide);
    this.whyBody = el("div", "why-body");
    this.whyEl.append(whyHead, this.whyBody);

    this.rulesEl = el("aside", "rules");
    this.rulesEl.setAttribute("aria-label", "Rules");
    this.rulesEl.setAttribute("aria-hidden", "true");
    const rulesHead = el("div", "panel-head");
    const rulesTitle = el("h2", "panel-title");
    rulesTitle.textContent = "Rules";
    const rulesHide = button("Hide", "console-btn");
    rulesHide.addEventListener("click", () => {
      this.closeRules();
    });
    rulesHead.append(rulesTitle, rulesHide);
    this.rulesError = el("div", "console-error");
    this.rulesError.hidden = true;
    this.rulesList = el("div", "rules-list");
    this.rulesEl.append(rulesHead, this.rulesError, this.rulesList);

    host.append(this.rulesEl, this.whyEl);
  }

  get isRulesOpen(): boolean {
    return this.rulesOpen;
  }

  get highlightIds(): ReadonlySet<string> {
    return this.highlighted;
  }

  toggleRules(): void {
    if (this.rulesOpen) {
      this.closeRules();
      return;
    }
    void this.openRules();
  }

  closeRules(): void {
    if (!this.rulesOpen) {
      return;
    }
    this.rulesOpen = false;
    this.rulesEl.classList.remove("is-open");
    this.rulesEl.setAttribute("aria-hidden", "true");
    this.onOpenChange?.("rules", false);
  }

  closeWhy(): void {
    if (!this.whyOpen) {
      return;
    }
    this.whyOpen = false;
    this.whyEdgeId = undefined;
    this.whyEl.classList.remove("is-open");
    this.whyEl.setAttribute("aria-hidden", "true");
    this.whyEl.removeAttribute("data-kind");
    this.onOpenChange?.("why", false);
  }

  /** Close the why panel if its edge is gone. T6 wires watch events to this. */
  closeIfEdgeMissing(): void {
    if (whyEdgeMissing(this.store, this.whyEdgeId)) {
      this.closeWhy();
    }
  }

  async openWhy(id: string): Promise<void> {
    const edge = this.store.edges.get(id);
    if (edge === undefined) {
      this.closeWhy();
      return;
    }
    this.whyOpen = true;
    this.whyEdgeId = id;
    this.whyEl.classList.add("is-open");
    this.whyEl.setAttribute("aria-hidden", "false");
    this.onOpenChange?.("why", true);

    await ensureProvenance(this.store, this.api);
    const src = this.store.nodes.get(edge.src);
    const dst = this.store.nodes.get(edge.dst);
    if (src === undefined || dst === undefined) {
      this.closeWhy();
      return;
    }
    await loadNodeProps(this.store, this.api, src.key);
    await loadNodeProps(this.store, this.api, dst.key);
    const fresh = this.store.edges.get(id);
    const srcNow = this.store.nodes.get(src.key);
    const dstNow = this.store.nodes.get(dst.key);
    if (fresh === undefined || srcNow === undefined || dstNow === undefined) {
      this.closeWhy();
      return;
    }
    const model = buildWhyModel({ edge: fresh, src: srcNow, dst: dstNow });
    this.renderWhy(model);
  }

  destroy(): void {
    this.closeWhy();
    this.closeRules();
    this.whyEl.remove();
    this.rulesEl.remove();
  }

  private async openRules(): Promise<void> {
    this.rulesOpen = true;
    this.rulesEl.classList.add("is-open");
    this.rulesEl.setAttribute("aria-hidden", "false");
    this.onOpenChange?.("rules", true);
    try {
      const stats = await this.api.stats();
      this.rulesError.hidden = true;
      this.rulesError.textContent = "";
      this.renderRules(stats.rules);
    } catch (err: unknown) {
      this.rulesError.textContent = queryErrorText(err);
      this.rulesError.hidden = false;
    }
  }

  private renderRules(rules: RuleStats[]): void {
    const next: HTMLElement[] = [];
    for (const rule of rules) {
      const btn = button("", "rules-item");
      btn.type = "button";
      if (this.highlightedRule === rule.name) {
        btn.setAttribute("aria-current", "true");
      }
      const name = el("div", "rules-name");
      name.textContent = rule.name;
      const meta = el("div", "rules-meta");
      meta.textContent = `${rule.edges} edges · ${rule.fires} fires`;
      btn.append(name, meta);
      if (rule.tripped) {
        const badge = el("span", "rules-tripped");
        badge.textContent = "tripped";
        btn.append(badge);
      }
      btn.addEventListener("click", () => {
        void this.onRuleClick(rule.name);
      });
      next.push(btn);
    }
    this.rulesList.replaceChildren(...next);
  }

  private async onRuleClick(name: string): Promise<void> {
    if (this.highlightedRule === name) {
      this.highlightedRule = undefined;
      this.highlighted = new Set();
      this.markRuleRows();
      this.onNeedPaint?.();
      return;
    }
    this.rulesError.hidden = true;
    this.rulesError.textContent = "";
    const error = await runRuleClick(async () => {
      await ensureProvenance(this.store, this.api);
      this.highlightedRule = name;
      const ids = highlightedIdsForRule(this.store, name);
      this.highlighted = new Set(ids);
      this.markRuleRows();
      this.onNeedPaint?.();
      const first = ids[0];
      if (first !== undefined) {
        await this.openWhy(first);
      }
    });
    if (error !== undefined) {
      this.rulesError.textContent = error;
      this.rulesError.hidden = false;
    }
  }

  private markRuleRows(): void {
    for (const child of this.rulesList.children) {
      if (!(child instanceof HTMLElement)) {
        continue;
      }
      const name = child.querySelector(".rules-name")?.textContent;
      if (name === this.highlightedRule) {
        child.setAttribute("aria-current", "true");
      } else {
        child.removeAttribute("aria-current");
      }
    }
  }

  private renderWhy(model: WhyModel): void {
    this.whyEl.dataset.kind = model.kind;
    const bits: HTMLElement[] = [];
    if (model.kind === "hand") {
      bits.push(mono("why-etype", model.etype));
      bits.push(mono("why-ends", `${model.src} → ${model.dst}`));
      bits.push(mono("why-line why-hand", model.line));
      this.whyBody.replaceChildren(...bits);
      return;
    }
    bits.push(mono("why-rule", model.rule));
    const typeLine =
      model.weight === null
        ? model.etype
        : `${model.etype} · ${formatScore(model.weight)}`;
    bits.push(mono("why-etype", typeLine));
    bits.push(mono("why-ends", `${model.srcKey} → ${model.dstKey}`));
    if (model.kind === "overlap") {
      bits.push(mono("why-line", model.line));
      bits.push(tokenRow(model.srcKey, model.srcTokens));
      bits.push(tokenRow(model.dstKey, model.dstTokens));
    } else if (model.kind === "key_match") {
      bits.push(mono("why-line", model.line));
    } else if (model.kind === "field_equal") {
      bits.push(fieldEqualRow(model.field, model.value));
    } else {
      bits.push(mono("why-line", model.line));
    }
    this.whyBody.replaceChildren(...bits);
  }
}

/** Rule-click API failures become strip text; `undefined` means success. */
export async function runRuleClick(
  action: () => Promise<void>,
): Promise<string | undefined> {
  try {
    await action();
    return undefined;
  } catch (err: unknown) {
    return queryErrorText(err);
  }
}

function fieldEqualRow(field: string, value: string): HTMLElement {
  const row = el("div", "why-line");
  row.append(document.createTextNode(`field_equal(${field}): `));
  const left = el("span", "why-tok why-tok-shared");
  left.textContent = value;
  const right = el("span", "why-tok why-tok-shared");
  right.textContent = value;
  row.append(left, document.createTextNode(" = "), right);
  return row;
}

function tokenRow(key: string, tokens: readonly TokenMark[]): HTMLElement {
  const wrap = el("div", "why-set");
  wrap.append(mono("why-set-key", key));
  const row = el("div", "why-tokens");
  for (const mark of tokens) {
    const chip = el("span", mark.shared ? "why-tok why-tok-shared" : "why-tok");
    chip.textContent = mark.token;
    row.append(chip);
  }
  wrap.append(row);
  return wrap;
}

function mono(className: string, text: string): HTMLElement {
  const node = el("div", className);
  node.textContent = text;
  return node;
}

function button(label: string, className: string): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = className;
  if (label !== "") {
    btn.textContent = label;
  }
  return btn;
}

function el(tag: string, className?: string): HTMLElement {
  const node = document.createElement(tag);
  if (className !== undefined) {
    node.className = className;
  }
  return node;
}

