import { afterEach, describe, expect, test } from "bun:test";
import {
  existsSync,
  chmodSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  cleanupWorkspace,
  createIsolatedCodexHome,
} from "./extension.ts";

const roots: string[] = [];

afterEach(() => {
  for (const root of roots.splice(0)) {
    rmSync(root, { recursive: true, force: true });
  }
});

describe("createIsolatedCodexHome", () => {
  test("copies only authenticated account state into the fixture", () => {
    const root = mkdtempSync(join(tmpdir(), "1up-codex-home-test-"));
    roots.push(root);
    const source = join(root, "source");
    const home = join(root, "fixture-home");
    mkdirSync(source, { recursive: true });
    writeFileSync(join(source, "auth.json"), '{"account":"fixture"}\n');
    writeFileSync(join(source, "config.toml"), "[mcp_servers.inherited]\n");

    const codexHome = createIsolatedCodexHome(home, source);

    expect(codexHome).toBe(join(home, ".codex"));
    expect(readdirSync(codexHome)).toEqual(["auth.json"]);
    expect(readFileSync(join(codexHome, "auth.json"), "utf8")).toBe(
      '{"account":"fixture"}\n',
    );
    expect(existsSync(join(codexHome, "config.toml"))).toBe(false);
  });
});

describe("cleanupWorkspace", () => {
  test("stops the fixture daemon with the isolated home before removal", () => {
    const root = mkdtempSync(join(tmpdir(), "1up-workspace-cleanup-test-"));
    roots.push(root);
    const workspace = join(root, "workspace");
    const home = join(workspace, "home");
    const repo = join(workspace, "emdash");
    const bin = join(root, "bin");
    const capture = join(root, "cleanup.txt");
    const fakeOneup = join(bin, "1up");
    mkdirSync(join(home, ".codex"), { recursive: true });
    mkdirSync(repo, { recursive: true });
    mkdirSync(bin, { recursive: true });
    writeFileSync(
      fakeOneup,
      '#!/bin/sh\nprintf "%s\\n%s\\n%s\\n" "$HOME" "$PWD" "$*" > "$CLEANUP_CAPTURE"\n',
    );
    chmodSync(fakeOneup, 0o755);
    const canonicalRepo = realpathSync(repo);

    const originalPath = process.env.PATH;
    const originalCapture = process.env.CLEANUP_CAPTURE;
    process.env.PATH = `${bin}:${originalPath ?? ""}`;
    process.env.CLEANUP_CAPTURE = capture;
    try {
      cleanupWorkspace(workspace);
    } finally {
      process.env.PATH = originalPath;
      if (originalCapture === undefined) {
        process.env.CLEANUP_CAPTURE = undefined;
      } else {
        process.env.CLEANUP_CAPTURE = originalCapture;
      }
    }

    expect(readFileSync(capture, "utf8").split("\n")).toEqual([
      home,
      canonicalRepo,
      "stop --plain .",
      "",
    ]);
    expect(existsSync(workspace)).toBe(false);
  });
});
