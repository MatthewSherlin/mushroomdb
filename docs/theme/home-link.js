// The book's title links back to the landing page, one level above /docs/.
// mdBook has no setting for this. Every page of the book is directly under
// /docs/, and 404.html carries a <base> pointing there, so "../" is the
// landing page from all of them.
document.addEventListener("DOMContentLoaded", () => {
  const title = document.querySelector(".menu-title");
  if (!title || title.querySelector("a")) return;
  const link = document.createElement("a");
  link.href = "../";
  link.textContent = title.textContent;
  title.textContent = "";
  title.appendChild(link);
});
