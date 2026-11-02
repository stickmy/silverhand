#!/usr/bin/env node
// Lists user-facing English left in renderer source: JSX text, copy-bearing
// attributes, and prose-shaped string literals. Exit code 1 when anything is found.
//
//   node scripts/i18n-scan.mjs [path ...]
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import ts from "typescript";

const ROOT = fileURLToPath(new URL("../src", import.meta.url));

const SKIP_FILE =
  /(\.test\.tsx?|\.d\.ts|-contract\.ts|viewer-channel-removal\.ts|conversation-ingestor\.ts|html-preview-csp\.ts)$|\/lib\/i18n\/|\/icons\//;
const COPY_ATTRIBUTES = new Set([
  "title",
  "label",
  "placeholder",
  "aria-label",
  "aria-description",
  "alt",
  "description",
  "confirmLabel",
  "cancelLabel",
  "emptyLabel",
  "tooltip",
  "hint",
  "heading",
]);
const NON_COPY_ATTRIBUTES =
  /^(className|class|id|key|href|src|type|role|name|data-.*|style|htmlFor|rel|target|method|autoComplete|inputMode|enterKeyHint|form|lang|dir|accept|pattern|persistId|testId|variant|size|side|align|kind|mode|tone|icon|value|defaultValue|sizes|to|path|as)$/;
const NON_COPY_CALLEES =
  /^(console\.\w+|Error|TypeError|RangeError|t|cn|clsx|cva|twMerge|require|import|querySelector(All)?|closest|matches|getItem|setItem|removeItem|addEventListener|removeEventListener|dispatchEvent|CustomEvent|Event|postMessage|invoke|send|on|off|emit|fetch|URL|URLSearchParams|RegExp|new Intl\.\w+|startsWith|endsWith|includes|split|replace|replaceAll|join|padStart|padEnd|localeCompare|toLocaleString|toLocaleDateString|toLocaleTimeString|format|setAttribute|getAttribute|hasAttribute|removeAttribute|setProperty|getPropertyValue|createElement|matchMedia|assert\w*|invariant|warn|debug|log|info|error)$/;

const NOT_COPY = [
  /^(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)$/,
  /^(Digit|Key|Arrow)\w+$/,
  /^[A-Z][a-z]+(-[A-Z][a-z]+)+$/, // header names
  /^<svg[\s>]/,
];

/** Every token looks like a utility class and at least one is unmistakably one. */
function isClassList(text) {
  const tokens = text.trim().split(/\s+/);
  return (
    tokens.every((token) => /^[!a-z-][\w:/[\]().%,&>*=~#'"!+-]*$/.test(token)) &&
    tokens.some((token) => /[[:]|^-?[a-z]+-[\w[(]/.test(token))
  );
}

const ALLOW = new Set([
  "Silverhand",
  "GitHub",
  "macOS",
  "iOS",
  "iPadOS",
  "Markdown",
  "Claude Code",
  "Codex",
  "Grok",
  "pi",
  "OpenCode",
  "Qoder",
  "Cursor",
  "Gemini",
  "Electron",
  "SQLite",
  "JSON",
  "URL",
  "API",
  "ID",
  "OK",
  "⌘",
  "Esc",
  "Enter",
  "Tab",
  "Shift",
  "Option",
  "Ctrl",
  "Cmd",
]);

function isProse(text) {
  const trimmed = text.trim();
  if (!/[A-Za-z]{2,}/.test(trimmed) || ALLOW.has(trimmed)) {
    return false;
  }
  if (NOT_COPY.some((re) => re.test(trimmed)) && !/^[A-Z][a-z]+( [a-z]+)*[.?!…]?$/.test(trimmed)) {
    return false;
  }
  if (isClassList(trimmed.replace(/…/g, " "))) {
    return false;
  }
  if (/^[a-z0-9_.:/#@%-]+$/.test(trimmed)) {
    return false; // identifiers, keys, classes, routes
  }
  if (
    /^[\w-]+(\s+[\w:/[\]().%-]+)+$/.test(trimmed) &&
    /(^|\s)(-?[a-z]+-[\w[\]().%/-]+|[a-z]+:[\w-]+)/.test(trimmed)
  ) {
    return false; // class lists
  }
  if (/^(https?:|mailto:|data:|\/|\.\/|@\/|#|\$\{)/.test(trimmed)) {
    return false;
  }
  return (
    /^[A-Z][a-z]/.test(trimmed) ||
    /[A-Za-z’']{2,}\s+[A-Za-z]{2,}/.test(trimmed) ||
    (/^[A-Z]{2,}$/.test(trimmed) && trimmed.length > 3)
  );
}

function calleeName(node) {
  const expr = node.expression;
  if (ts.isIdentifier(expr)) return expr.text;
  if (ts.isPropertyAccessExpression(expr)) {
    const base = ts.isIdentifier(expr.expression) ? `${expr.expression.text}.` : "";
    // `toast.error(...)` carries copy; only `console.error` style loggers don't.
    if (base === "toast.") return "toast";
    return base === "console." ? `console.${expr.name.text}` : expr.name.text;
  }
  return "";
}

function skipContext(node) {
  for (let cur = node.parent; cur; cur = cur.parent) {
    if (
      ts.isImportDeclaration(cur) ||
      ts.isExportDeclaration(cur) ||
      ts.isTypeNode(cur) ||
      ts.isLiteralTypeNode(cur)
    )
      return true;
    if (ts.isCaseClause(cur) && cur.expression === node) return true;
    if (
      ts.isBinaryExpression(cur) &&
      [
        ts.SyntaxKind.EqualsEqualsEqualsToken,
        ts.SyntaxKind.ExclamationEqualsEqualsToken,
        ts.SyntaxKind.EqualsEqualsToken,
        ts.SyntaxKind.ExclamationEqualsToken,
      ].includes(cur.operatorToken.kind)
    )
      return true;
    if (ts.isPropertyAssignment(cur) && cur.name === node) return true;
    if (
      ts.isBinaryExpression(cur) &&
      cur.operatorToken.kind === ts.SyntaxKind.EqualsToken &&
      ts.isPropertyAccessExpression(cur.left) &&
      /^(displayName|name)$/.test(cur.left.name.text)
    )
      return true;
    if (ts.isElementAccessExpression(cur) && cur.argumentExpression === node) return true;
    if (ts.isJsxAttribute(cur)) {
      const name = cur.name.getText();
      if (NON_COPY_ATTRIBUTES.test(name)) return true;
      return false;
    }
    if (ts.isCallExpression(cur) || ts.isNewExpression(cur)) {
      const name = ts.isNewExpression(cur)
        ? ts.isIdentifier(cur.expression)
          ? cur.expression.text
          : ""
        : calleeName(cur);
      if (NON_COPY_CALLEES.test(name)) return true;
      return false;
    }
    if (ts.isTaggedTemplateExpression(cur)) return true;
    if (ts.isBlock(cur) || ts.isSourceFile(cur)) return false;
  }
  return false;
}

const ALLOW_FILE = fileURLToPath(new URL("./i18n-scan-allow.txt", import.meta.url));
let allowEntries = new Set();
try {
  allowEntries = new Set(
    readFileSync(ALLOW_FILE, "utf8")
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line && !line.startsWith("#")),
  );
} catch {
  // No allowlist yet.
}

export function scanFile(path) {
  const source = readFileSync(path, "utf8");
  const file = ts.createSourceFile(
    path,
    source,
    ts.ScriptTarget.Latest,
    true,
    path.endsWith("x") ? ts.ScriptKind.TSX : ts.ScriptKind.TS,
  );
  const hits = [];
  const report = (node, text) => {
    const { line } = file.getLineAndCharacterOfPosition(node.getStart());
    const clean = text.trim().replace(/\s+/g, " ");
    const rel = relative(ROOT, path);
    if (allowEntries.has(`${rel}: ${clean}`) || allowEntries.has(`*: ${clean}`)) return;
    hits.push({ node, line: line + 1, text: clean.slice(0, 90) });
  };

  const visit = (node) => {
    if (ts.isJsxText(node)) {
      const text = node.getText();
      if (/[A-Za-z]{2,}/.test(text) && !ALLOW.has(text.trim())) report(node, text);
    } else if (
      ts.isJsxAttribute(node) &&
      node.initializer &&
      ts.isStringLiteral(node.initializer)
    ) {
      const name = node.name.getText();
      const text = node.initializer.text;
      if (
        (COPY_ATTRIBUTES.has(name) && /[A-Za-z]{2,}/.test(text) && !ALLOW.has(text)) ||
        (!NON_COPY_ATTRIBUTES.test(name) && isProse(text))
      ) {
        report(node, `${name}="${text}"`);
      }
      return;
    } else if (
      (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) &&
      !ts.isJsxAttribute(node.parent)
    ) {
      if (isProse(node.text) && !skipContext(node)) report(node, node.text);
    } else if (ts.isTemplateExpression(node)) {
      const text = [node.head.text, ...node.templateSpans.map((s) => s.literal.text)].join("…");
      if (isProse(text) && !skipContext(node)) report(node, `\`${text}\``);
    }
    ts.forEachChild(node, visit);
  };
  visit(file);
  return Object.assign(hits, { file });
}

export function walk(path) {
  if (statSync(path).isDirectory()) {
    return readdirSync(path).flatMap((name) => walk(join(path, name)));
  }
  return /\.tsx?$/.test(path) && !SKIP_FILE.test(path) ? [path] : [];
}

export { ROOT };

if (import.meta.url !== pathToFileURL(process.argv[1] ?? "").href) {
  // Imported as a module.
} else {
  main();
}

function main() {
  const targets = process.argv.slice(2);
  const files = (targets.length ? targets : [ROOT]).flatMap(walk).sort();
  let total = 0;
  const perDir = new Map();
  for (const path of files) {
    const hits = scanFile(path);
    if (!hits.length) continue;
    total += hits.length;
    const rel = relative(ROOT, path);
    const dir = rel.split("/").slice(0, 2).join("/");
    perDir.set(dir, (perDir.get(dir) ?? 0) + hits.length);
    for (const hit of hits) console.log(`${rel}:${hit.line}  ${hit.text}`);
  }
  if (total) {
    console.log(`\n${total} strings`);
    for (const [dir, n] of [...perDir].sort((a, b) => b[1] - a[1]))
      console.log(`  ${String(n).padStart(4)}  ${dir}`);
  }
  process.exit(total ? 1 : 0);
}
