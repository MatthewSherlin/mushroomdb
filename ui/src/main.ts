import "./fonts.css";
import "./tokens.css";
import "./style.css";
import { ApiClient } from "./api";
import { Explorer } from "./explorer";
import { GraphStore } from "./store";

const host = document.querySelector("#app");
if (!(host instanceof HTMLElement)) {
  throw new Error("missing #app");
}

new Explorer(host, {
  api: new ApiClient(),
  store: new GraphStore(),
});
