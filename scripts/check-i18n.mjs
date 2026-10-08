#!/usr/bin/env node
/* check-i18n.mjs - Task 2-b verification gate.
 *
 * Parses the STR dictionary out of the dashboard's app.js and every
 * consumer reference (data-i18n / data-i18n-attr in index.html, t("…")
 * literals and dynamic maps in app.js), then asserts:
 *
 *   1. every referenced key exists in STR,
 *   2. every STR key (used or not) carries all four languages
 *      (en, de-DE, ja-JP, zh-CN) with non-empty copy,
 *   3. no visible copy contains an em-dash (house hard ban, see the
 *      app.js i18n header; "·" and "-" carry),
 *   4. every data-i18n key in index.html is referenced somewhere too
 *      (guards against copy that silently lost its translation hook).
 *
 * The review portal (crates/cairn-review/assets/review.js) uses the same
 * STR + t() shape - when its dictionary grows, add it via --review.
 *
 * Usage: node scripts/check-i18n.mjs [--review]
 * Exits 1 with a report on the first failing category.
 */
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const DASH = join(root, "crates/cairn-cli/assets/dashboard");
const REV = join(root, "crates/cairn-review/assets");

const LANGS = ["en", "de-DE", "ja-JP", "zh-CN"];
const problems = { missing: [], incomplete: [], emdash: [], dangling: [] };

/* ---- extract `const STR = { ... };` from a JS file and evaluate it ---- */
function parseSTR(path) {
  const src = readFileSync(path, "utf8");
  const start = src.indexOf("const STR = {");
  if (start < 0) throw new Error(`no STR dictionary in ${path}`);
  const open = src.indexOf("{", start);
  let depth = 0, end = -1, inStr = null, esc = false;
  for (let i = open; i < src.length; i++) {
    const ch = src[i];
    if (inStr) {
      if (esc) esc = false;
      else if (ch === "\\") esc = true;
      else if (ch === inStr) inStr = null;
      continue;
    }
    if (ch === '"' || ch === "'") { inStr = ch; continue; }
    if (ch === "/" && src[i + 1] === "*") { i = src.indexOf("*/", i + 2); if (i < 0) break; i++; continue; }
    if (ch === "{") depth++;
    else if (ch === "}") { depth--; if (depth === 0) { end = i; break; } }
  }
  if (end < 0) throw new Error(`unbalanced STR object in ${path}`);
  const literal = src.slice(open, end + 1);
  return { STR: new Function(`return (${literal});`)(), src };
}

/* ---- reference collection -------------------------------------------- */
function refsFromHTML(html) {
  const keys = new Set();
  for (const m of html.matchAll(/data-i18n="([^"]+)"/g)) keys.add(m[1]);
  for (const m of html.matchAll(/data-i18n-attr="([^"]+)"/g)) {
    for (const pair of m[1].split(";")) {
      const k = pair.split(":")[1];
      if (k) keys.add(k.trim());
    }
  }
  return keys;
}

function refsFromJS(js) {
  const keys = new Set();
  // t("key") / t('key') literals
  for (const m of js.matchAll(/\bt\(\s*["']([^"']+)["']/g)) keys.add(m[1]);
  // STR["key"] index form
  for (const m of js.matchAll(/\bSTR\[\s*["']([^"']+)["']\s*\]/g)) keys.add(m[1]);
  // dynamic maps and object fields whose value is a key:
  //   { ok: "chip.ok" } / { key: "home.issue.files", cta: "btn.retry" }
  for (const m of js.matchAll(/(?:^|[{,]\s*)(?:[a-zA-Z_$][\w$]*|["'][\w.$-]+["'])\s*:\s*["']([a-z][a-z0-9]*(?:\.[a-zA-Z0-9]+)+)["']/g)) {
    keys.add(m[1]);
  }
  return keys;
}

/* ---- run -------------------------------------------------------------- */
const dash = parseSTR(join(DASH, "app.js"));
const sources = [{ name: "dashboard", STR: dash.STR, html: join(DASH, "index.html"), js: dash.src }];
if (process.argv.includes("--review")) {
  const rev = parseSTR(join(REV, "review.js"));
  sources.push({ name: "review", STR: rev.STR, html: join(REV, "review.html"), js: rev.src });
}

for (const { name, STR, html: htmlPath, js } of sources) {
  const used = refsFromJS(js);
  used.difference ??= null;
  const htmlRefs = refsFromHTML(readFileSync(htmlPath, "utf8"));
  for (const k of htmlRefs) used.add(k);

  // 1 + 2 + 3: dictionary completeness
  for (const [key, row] of Object.entries(STR)) {
    for (const lang of LANGS) {
      const v = row ? row[lang] : undefined;
      if (typeof v !== "string" || !v.trim()) {
        problems.incomplete.push(`[${name}] "${key}" missing ${lang}`);
      } else if (v.includes("\u2014")) {
        problems.emdash.push(`[${name}] "${key}" ${lang} copy contains an em-dash`);
      }
    }
  }

  // referenced but undefined
  for (const k of used) {
    if (!Object.prototype.hasOwnProperty.call(STR, k)) problems.missing.push(`[${name}] "${k}" referenced but not in STR`);
  }

  // 4: html hooks that no JS copy path uses would still resolve (data-i18n
  //    reads STR directly), so dangling = html key absent from STR
  for (const k of htmlRefs) {
    if (!Object.prototype.hasOwnProperty.call(STR, k)) problems.dangling.push(`[${name}] html data-i18n "${k}" not in STR`);
  }
}

const total = Object.values(problems).reduce((n, a) => n + a.length, 0);
if (total === 0) {
  console.log("check-i18n: OK - all keys present in en/de-DE/ja-JP/zh-CN, no em-dashes, no dangling references");
  process.exit(0);
}
for (const [kind, list] of Object.entries(problems)) {
  if (!list.length) continue;
  console.error(`check-i18n: ${list.length} ${kind}:`);
  for (const line of [...new Set(list)].sort()) console.error(`  - ${line}`);
}
process.exit(1);
