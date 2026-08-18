/** One-time gold pulse for edges born from a live `apply()`. */

export const GLOW_MS = 600;

export function bornEdgeIds(
  before: Iterable<string>,
  after: Iterable<string>,
): string[] {
  const prev = new Set(before);
  const born: string[] = [];
  for (const id of after) {
    if (!prev.has(id)) {
      born.push(id);
    }
  }
  born.sort();
  return born;
}

export class GlowQueue {
  private readonly until = new Map<string, number>();

  schedule(
    ids: readonly string[],
    now: number,
    durationMs: number = GLOW_MS,
  ): void {
    const expiry = now + durationMs;
    for (const id of ids) {
      this.until.set(id, expiry);
    }
  }

  active(now: number): string[] {
    const out: string[] = [];
    for (const [id, expiry] of this.until) {
      if (expiry > now) {
        out.push(id);
      }
    }
    out.sort();
    return out;
  }

  prune(now: number): boolean {
    for (const [id, expiry] of [...this.until.entries()]) {
      if (expiry <= now) {
        this.until.delete(id);
      }
    }
    return this.until.size > 0;
  }

  nextExpiry(): number | undefined {
    let min: number | undefined;
    for (const expiry of this.until.values()) {
      if (min === undefined || expiry < min) {
        min = expiry;
      }
    }
    return min;
  }
}
