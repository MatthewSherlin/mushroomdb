// Renders social-preview.html -> social-preview.png (2560x1280) + .jpg.
// Run from ui/: node scripts/render-social-preview.mjs
import { chromium } from "@playwright/test";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "docs", "assets");
const browser = await chromium.launch();
const page = await browser.newPage({
  viewport: { width: 1280, height: 640 },
  deviceScaleFactor: 2,
});
await page.goto(`file://${join(here, "social-preview.html")}`);
await page.waitForTimeout(400);
await page.screenshot({ path: join(here, "social-preview.png") });
await page.screenshot({ path: join(here, "social-preview.jpg"), type: "jpeg", quality: 92 });
await browser.close();
console.log("rendered social-preview.png / .jpg");
