import { load } from "./loader.mjs";
const { parse } = require("./parser");

/** Public name of the demo. */
export const NAME = "demo";

/** Reads records by key. */
export class Reader {
    /** Read one record. */
    read(k) {
        return parse(load(k));
    }
}

/** Boot the demo. */
export function boot() {
    const r = new Reader();
    return r.read(NAME);
}
