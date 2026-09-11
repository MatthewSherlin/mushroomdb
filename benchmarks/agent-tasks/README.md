# The agent benchmark

Two suites share this harness. Each cell is one `claude -p` session captured as stream-json, so
tool calls are counted and the final `result` event supplies usage, cost, turns and duration.

## `--suite association` — the engine's benchmark

One generated world written three ways, twenty relationship questions, graded against executable
truth. This is the suite that measures what mushroomdb is for. Full description, the gate, and how
to rebuild the world: [`docs/site/association-bench.md`](../../docs/site/association-bench.md).

```sh
python3 benchmarks/agent-tasks/run.py --suite association --setup-only
python3 benchmarks/agent-tasks/run.py --suite association --reps 3
```

## `--suite code` — retired

The code suite asked twenty repository questions of a stock session and of sessions carrying the
code-graph door. **It is retired as of v0.6.4.** It still runs — nothing was deleted — but no
further runs will be committed, because the question it asks has been answered:

| run | what it measured | result |
|---|---|---|
| [`20260910T000418Z`](results/20260910T000418Z/summary.md) | arms stock / installed / invoked / cli × 3 reps × 20 tasks, 240 cells | invoked arm 0.924 vs stock 0.927; cost $0.2713 vs $0.2267; **0 graph calls in 120 unprompted sessions**. Gate FAILED on every arm |
| [`20260910T050239Z`](results/20260910T050239Z/summary.md) | the grep-redirect variants | adoption 25% and 10%; gate FAILED |

The committed summaries are the record. The code-graph door they measured is deprecated in v0.6.4
and removed in 0.7.
