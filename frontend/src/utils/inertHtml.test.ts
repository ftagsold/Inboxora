import assert from "node:assert/strict";
import test from "node:test";
import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const srcDir = fileURLToPath(new URL("../", import.meta.url));

async function sourceFiles(dir: string): Promise<string[]> {
  const entries = await readdir(dir, { withFileTypes: true });
  const nested = await Promise.all(entries.map(entry => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) return sourceFiles(path);
    return /\.tsx?$/.test(entry.name) && !/\.test\.tsx?$/.test(entry.name) ? [path] : [];
  }));
  return nested.flat();
}

// Assigning innerHTML on a live-document element runs <img onerror> even when the element is
// detached, so untrusted HTML must be sanitized first or parsed with parseInertHtml instead.
test("every innerHTML assignment receives sanitized HTML", async () => {
  const offenders: string[] = [];
  for (const file of await sourceFiles(srcDir)) {
    const lines = (await readFile(file, "utf8")).split("\n");
    lines.forEach((line, index) => {
      const assignment = /\.innerHTML\s*=\s*(.+)$/.exec(line);
      if (assignment && !/^(DOMPurify\.sanitize\(|sanitized\b)/.test(assignment[1].trim())) {
        offenders.push(`${file.slice(srcDir.length)}:${index + 1}`);
      }
    });
  }
  assert.deepEqual(offenders, []);
});

test("signature text extraction parses into an inert document", async () => {
  const [legacy, compose] = await Promise.all([
    readFile(new URL("./legacySignatureText.ts", import.meta.url), "utf8"),
    readFile(new URL("../components/ComposeModal.tsx", import.meta.url), "utf8"),
  ]);
  assert.match(legacy, /parseInertHtml\(html\)/);
  assert.match(compose, /function stripHtml[\s\S]{0,80}parseInertHtml\(html\)/);
});
