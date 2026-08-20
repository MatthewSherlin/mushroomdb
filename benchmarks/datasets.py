"""Dataset generator for comparative benchmarks.

Wraps dogfood/synthesize.py (read-only) and exports node records as JSONL
using the same deterministic synthetic-schema shapes.  No engine or binding
imports here — this module is database-agnostic.

Usage (standalone)::

    python datasets.py --scale 10000 --seed 42 --out nodes.jsonl

Programmatic::

    from datasets import iter_nodes, write_jsonl
    for node in iter_nodes(n=10_000, seed=42):
        ...          # {key, label, props}
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import sys
from pathlib import Path
from typing import Iterator

# ---------------------------------------------------------------------------
# Locate dogfood/synthesize.py without mutating sys.path permanently.
# ---------------------------------------------------------------------------
_REPO_ROOT = Path(__file__).resolve().parent.parent
_DOGFOOD_DIR = _REPO_ROOT / "dogfood"

_TRANSFORM_SPEC = importlib.util.spec_from_file_location(
    "transform", _DOGFOOD_DIR / "transform.py"
)
_transform_mod = importlib.util.module_from_spec(_TRANSFORM_SPEC)  # type: ignore[arg-type]
sys.modules.setdefault("transform", _transform_mod)
_TRANSFORM_SPEC.loader.exec_module(_transform_mod)  # type: ignore[union-attr]

_SYNTH_SPEC = importlib.util.spec_from_file_location(
    "synthesize", _DOGFOOD_DIR / "synthesize.py"
)
_synth_mod = importlib.util.module_from_spec(_SYNTH_SPEC)  # type: ignore[arg-type]
sys.modules.setdefault("synthesize", _synth_mod)
_SYNTH_SPEC.loader.exec_module(_synth_mod)  # type: ignore[union-attr]

# Re-export the generator.
generate = _synth_mod.generate  # type: ignore[attr-defined]

# ---------------------------------------------------------------------------
# Scale split: mirrors dogfood/scale_run.py split_scale logic (70/20/10).
# ---------------------------------------------------------------------------
DEFAULT_SCALE = 10_000
DEFAULT_SEED = 20260819


def split_scale(n: int) -> tuple[int, int, int]:
    """70 / 20 / 10 Talent / Company / Job split (remainder → Job)."""
    n_talent = (n * 7) // 10
    n_companies = (n * 2) // 10
    n_jobs = n - n_talent - n_companies
    return n_talent, n_companies, n_jobs


def iter_nodes(n: int = DEFAULT_SCALE, seed: int = DEFAULT_SEED) -> Iterator[dict]:
    """Yield node dicts ``{key, label, props}`` for n nodes at *seed*."""
    n_t, n_c, n_j = split_scale(n)
    yield from generate(n_t, n_c, n_j, seed)


def write_jsonl(path: Path, n: int = DEFAULT_SCALE, seed: int = DEFAULT_SEED) -> int:
    """Write JSONL of node dicts to *path*; return count written."""
    path.parent.mkdir(parents=True, exist_ok=True)
    count = 0
    with path.open("w") as f:
        for node in iter_nodes(n, seed):
            f.write(json.dumps(node) + "\n")
            count += 1
    return count


def read_jsonl(path: Path) -> list[dict]:
    """Read JSONL written by :func:`write_jsonl` back into a list."""
    with path.open() as f:
        return [json.loads(line) for line in f if line.strip()]


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def _cli() -> None:
    p = argparse.ArgumentParser(description="Dump benchmark dataset as JSONL")
    p.add_argument("--scale", type=int, default=DEFAULT_SCALE)
    p.add_argument("--seed", type=int, default=DEFAULT_SEED)
    p.add_argument("--out", default="nodes.jsonl")
    args = p.parse_args()
    out = Path(args.out)
    n = write_jsonl(out, n=args.scale, seed=args.seed)
    print(f"wrote {n} nodes → {out}")


if __name__ == "__main__":
    _cli()
