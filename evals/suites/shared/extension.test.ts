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
  establishWarmReadiness,
  stripColdState,
} from "./extension.ts";

const roots: string[] = [];

afterEach(() => {
  for (const root of roots.splice(0)) {
    rmSync(root, { recursive: true, force: true });
  }
});

function writeFakeOneup(binDir: string): void {
  mkdirSync(binDir, { recursive: true });
  const fake = join(binDir, "1up");
  writeFileSync(
    fake,
    [
      "#!/bin/sh",
      'if [ "$1" = "index" ]; then',
      '  printf "index %s\\n" "$PWD" >> "$ONEUP_INDEX_LOG"',
      "  exit 0",
      "fi",
      'if [ "$1" = "status" ]; then',
      '  printf "%s" "$ONEUP_STATUS_JSON"',
      "  exit 0",
      "fi",
      "exit 0",
      "",
    ].join("\n"),
  );
  chmodSync(fake, 0o755);
}

function setEnv(overrides: Record<string, string>): () => void {
  const previous: Record<string, string | undefined> = {};
  for (const [key, value] of Object.entries(overrides)) {
    previous[key] = process.env[key];
    process.env[key] = value;
  }
  return () => {
    for (const [key, value] of Object.entries(previous)) {
      if (value === undefined) {
        delete process.env[key];
      } else {
        process.env[key] = value;
      }
    }
  };
}

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

describe("establishWarmReadiness", () => {
  test("runs the setup index and passes the count gate on positive counts", () => {
    const root = mkdtempSync(join(tmpdir(), "1up-warm-ready-test-"));
    roots.push(root);
    const repo = join(root, "emdash");
    const home = join(root, "home");
    const bin = join(root, "bin");
    const indexLog = join(root, "index.log");
    mkdirSync(repo, { recursive: true });
    mkdirSync(join(home, ".codex"), { recursive: true });
    writeFakeOneup(bin);

    const restore = setEnv({
      PATH: `${bin}:${process.env.PATH ?? ""}`,
      ONEUP_STATUS_JSON:
        '{"lifecycle_state":"ready","indexed_files":5,"total_segments":42}',
      ONEUP_INDEX_LOG: indexLog,
    });
    try {
      const readiness = establishWarmReadiness(repo, home);

      expect(readiness).toEqual({ indexedFiles: 5, totalSegments: 42 });
      // The setup index ran (outside the measurement loop) before the gate.
      expect(existsSync(indexLog)).toBe(true);
      expect(readFileSync(indexLog, "utf8")).toContain("index ");
    } finally {
      restore();
    }
  });

  test("throws fail-closed on zero counts even when the status string reads ready", () => {
    const root = mkdtempSync(join(tmpdir(), "1up-warm-ready-hyp001-"));
    roots.push(root);
    const repo = join(root, "emdash");
    const home = join(root, "home");
    const bin = join(root, "bin");
    mkdirSync(repo, { recursive: true });
    mkdirSync(join(home, ".codex"), { recursive: true });
    writeFakeOneup(bin);

    const restore = setEnv({
      PATH: `${bin}:${process.env.PATH ?? ""}`,
      // Copied index reports a "ready" lifecycle string but zero current-context
      // rows — the gate must not trust the string.
      ONEUP_STATUS_JSON:
        '{"lifecycle_state":"ready","indexed_files":0,"total_segments":0}',
      ONEUP_INDEX_LOG: join(root, "index.log"),
    });
    try {
      expect(() => establishWarmReadiness(repo, home)).toThrow(
        /Warm readiness gate failed/,
      );
    } finally {
      restore();
    }
  });
});

describe("stripColdState", () => {
  test("removes the copied .1up index cache from the workspace", () => {
    const root = mkdtempSync(join(tmpdir(), "1up-cold-strip-test-"));
    roots.push(root);
    const repo = join(root, "emdash");
    const oneupDir = join(repo, ".1up");
    mkdirSync(oneupDir, { recursive: true });
    writeFileSync(join(oneupDir, "index.db"), "cache\n");
    writeFileSync(join(repo, "keep.ts"), "export const keep = true;\n");

    stripColdState(repo);

    expect(existsSync(oneupDir)).toBe(false);
    // Only the index cache is stripped; workspace source is untouched.
    expect(existsSync(join(repo, "keep.ts"))).toBe(true);
  });
});
