import { describe, expect, it } from "vitest";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";

// Architecture scan, not a behavior test. Imports point one way:
// pages → features → shared layers (see readme.md).

const ROOT = fileURLToPath(new URL(".", import.meta.url));
const SHARED_LAYERS = ["components", "hooks", "lib", "navigation", "icons"];
const importOf = (dir: string) =>
  new RegExp(`(?:from\\s+|import\\s*\\(?\\s*)["'](?:@/${dir}/|(?:\\.\\./)+${dir}/)`);
const FEATURE_IMPORT = importOf("features");
const PAGE_IMPORT = importOf("pages");

function walk(dir: string): string[] {
  const out: string[] = [];
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) {
      out.push(...walk(path));
    } else if (/\.(ts|tsx)$/.test(name) && !/\.test\.(ts|tsx)$/.test(name)) {
      out.push(path);
    }
  }
  return out;
}

function rel(file: string): string {
  return relative(ROOT, file).split(sep).join("/");
}

function importers(dirs: readonly string[], pattern: RegExp): string[] {
  return dirs
    .flatMap((dir) => walk(join(ROOT, dir)))
    .filter((file) => pattern.test(readFileSync(file, "utf8")))
    .map(rel)
    .sort();
}

function crossPageImports(): string[] {
  const pages = readdirSync(join(ROOT, "pages"));
  return pages.flatMap((page) =>
    walk(join(ROOT, "pages", page))
      .filter((file) => {
        const source = readFileSync(file, "utf8");
        return pages.some(
          (other) =>
            other !== page &&
            new RegExp(`["'](?:@/pages/${other}/|(?:\\.\\./)+${other}/)`).test(source),
        );
      })
      .map(rel),
  );
}

describe("import direction", () => {
  it("shared layers never import features or pages", () => {
    expect(importers(SHARED_LAYERS, FEATURE_IMPORT)).toEqual([]);
    expect(importers(SHARED_LAYERS, PAGE_IMPORT)).toEqual([]);
  });

  it("features never import pages", () => {
    expect(importers(["features"], PAGE_IMPORT)).toEqual([]);
  });

  it("a page never imports another page", () => {
    expect(crossPageImports()).toEqual([]);
  });

  it("toasts go through @/lib/toast, which picks the platform's surface", () => {
    const direct = importers(
      ["components", "features", "hooks", "lib", "navigation", "pages"],
      /from\s+["']sonner["']/,
    );
    expect(direct).toEqual(["lib/toast.ts"]);
  });
});
