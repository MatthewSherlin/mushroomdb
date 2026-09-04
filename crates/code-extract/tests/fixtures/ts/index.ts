import { helper } from "./util";
import { Shape } from "./types";
import { legacy } from "./legacy.js";
import { Widget } from "./widget";
import * as path from "node:path";
export { reexport } from "./types";

/** Largest accepted batch. */
export const LIMIT = 5;

/** Opaque handle. */
export type Handle = string;

/** Anything that accepts chunks. */
export interface Sink {
    write(chunk: string): void;
}

/** A queue of pending jobs. */
export class Queue {
    /** Push one job. */
    push(job: string): void {
        this.flush();
        helper(job);
    }

    flush(): void {}
}

/** Run everything. */
export function run(): number {
    const q = new Queue();
    q.push(path.join("a"));
    return LIMIT;
}
