import "./fonts.css";
import "./tokens.css";
import "./style.css";

const host = document.querySelector("#app");
if (!(host instanceof HTMLElement)) {
  throw new Error("missing #app");
}

const [{ Explorer }, { ApiClient }, { GraphStore }] = await Promise.all([
  import("./explorer"),
  import("./api"),
  import("./store"),
]);

new Explorer(host, {
  api: new ApiClient(),
  store: new GraphStore(),
});
