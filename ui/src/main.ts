import "./fonts.css";
import "./tokens.css";
import "./style.css";

const host = document.querySelector("#app");
if (!(host instanceof HTMLElement)) {
  throw new Error("missing #app");
}

const { Explorer } = await import("./explorer");
const { ApiClient } = await import("./api");
const { GraphStore } = await import("./store");

new Explorer(host, {
  api: new ApiClient(),
  store: new GraphStore(),
});
