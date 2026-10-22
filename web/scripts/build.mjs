// Bundles the page into dist/ (index.html + app.js); no CDN, no network.
import { copyFileSync, mkdirSync, readFileSync } from "node:fs";
import { build } from "esbuild";

const dist = new URL("../dist/", import.meta.url);
mkdirSync(dist, { recursive: true });
await build({
  entryPoints: [new URL("../src/main.js", import.meta.url).pathname],
  bundle: true,
  format: "esm",
  minify: true,
  target: "es2022",
  outfile: new URL("app.js", dist).pathname,
  logLevel: "info",
});
copyFileSync(new URL("../index.html", import.meta.url), new URL("index.html", dist));

// The page must load nothing from another host.
const html = readFileSync(new URL("index.html", dist), "utf8");
const js = readFileSync(new URL("app.js", dist), "utf8");
const external = /(?:src|href)\s*=\s*["']?(?:https?:)?\/\//i;
const remoteImport = /(?:\bimport\s*\(|\bfrom)\s*["'](?:https?:)?\/\//;
if (external.test(html) || remoteImport.test(js)) {
  throw new Error("dist/ references an external host");
}
console.log("dist/ has no external script or style addresses");
