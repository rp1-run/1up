import { execFileSync, execSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdirSync,
  realpathSync,
  rmSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const EMDASH_REPO = "https://github.com/emdash-cms/emdash.git";
const EMDASH_COMMIT = "5beb0dd";
const __dirname = dirname(fileURLToPath(import.meta.url));
const CACHE_DIR = join(__dirname, "../../.cache/emdash");
const INDEX_DB_PATH = join(CACHE_DIR, ".1up/index.db");
const CACHE_LOCK_PATH = join(__dirname, "../../.cache/.lock");
const TEMP_BASE = join(realpathSync(tmpdir()), "1up-evals");

interface CacheStatus {
  indexed_files?: number | null;
  total_segments?: number | null;
}

export interface FixtureWorkspace {
  workspaceDir: string;
  repoDir: string;
  homeDir: string;
  codexHomeDir: string;
}

interface HookContext {
  test: {
    vars?: Record<string, string | number | boolean | object>;
    options?: Record<string, unknown>;
  };
  result?: {
    success: boolean;
  };
}

function cacheNeedsRefresh(): boolean {
  if (!existsSync(INDEX_DB_PATH)) {
    return true;
  }

  try {
    const rawStatus = execSync("1up status -f json .", {
      cwd: CACHE_DIR,
      stdio: "pipe",
    }).toString();
    const status = JSON.parse(rawStatus) as CacheStatus;

    return !(
      typeof status.indexed_files === "number" &&
      status.indexed_files > 0 &&
      typeof status.total_segments === "number" &&
      status.total_segments > 0
    );
  } catch {
    return true;
  }
}

export function ensureFixtureCache(): void {
  mkdirSync(CACHE_DIR, { recursive: true });

  // Simple lock to prevent parallel processes from cloning/indexing simultaneously.
  // If the lock exists, another process is setting up the cache — wait for it.
  if (existsSync(CACHE_LOCK_PATH)) {
    const maxWaitMs = 120_000;
    const startMs = Date.now();
    while (existsSync(CACHE_LOCK_PATH)) {
      if (Date.now() - startMs > maxWaitMs) {
        // Stale lock — remove and proceed
        try {
          unlinkSync(CACHE_LOCK_PATH);
        } catch {
          /* ignore */
        }
        break;
      }
      execSync("sleep 1", { stdio: "pipe" });
    }
    return;
  }

  const needsWork = !existsSync(join(CACHE_DIR, ".git")) || cacheNeedsRefresh();
  if (!needsWork) {
    return;
  }

  // Acquire lock
  try {
    writeFileSync(CACHE_LOCK_PATH, String(process.pid), { flag: "wx" });
  } catch {
    // Another process beat us — wait for it
    ensureFixtureCache();
    return;
  }

  try {
    if (!existsSync(join(CACHE_DIR, ".git"))) {
      execSync(
        `git clone --single-branch --branch main ${EMDASH_REPO} "${CACHE_DIR}"`,
        { stdio: "pipe" },
      );
      execSync(`git -C "${CACHE_DIR}" checkout ${EMDASH_COMMIT}`, {
        stdio: "pipe",
      });
    }

    if (cacheNeedsRefresh()) {
      const command = existsSync(INDEX_DB_PATH) ? "1up reindex" : "1up index";
      execSync(command, { cwd: CACHE_DIR, stdio: "pipe" });
    }
  } finally {
    // Release lock
    try {
      unlinkSync(CACHE_LOCK_PATH);
    } catch {
      /* ignore */
    }
  }
}

export function createIsolatedCodexHome(
  homeDir: string,
  sourceCodexHome = process.env.CODEX_HOME ??
    join(process.env.HOME ?? homeDir, ".codex"),
): string {
  const codexHomeDir = join(homeDir, ".codex");
  mkdirSync(codexHomeDir, { recursive: true });

  const sourceAuth = join(sourceCodexHome, "auth.json");
  if (existsSync(sourceAuth)) {
    cpSync(sourceAuth, join(codexHomeDir, "auth.json"));
  }

  return codexHomeDir;
}

export function createWorkspace(): FixtureWorkspace {
  const uuid = crypto.randomUUID();
  const workspaceDir = join(TEMP_BASE, uuid);
  const homeDir = join(workspaceDir, "home");
  const repoDir = join(workspaceDir, "emdash");

  mkdirSync(homeDir, { recursive: true });
  mkdirSync(join(homeDir, ".local/share"), { recursive: true });
  mkdirSync(join(homeDir, ".config"), { recursive: true });
  const codexHomeDir = createIsolatedCodexHome(homeDir);

  cpSync(CACHE_DIR, repoDir, { recursive: true });

  return { workspaceDir, repoDir, homeDir, codexHomeDir };
}

function workspaceEnv(homeDir: string): NodeJS.ProcessEnv {
  return {
    ...process.env,
    HOME: homeDir,
    CODEX_HOME: join(homeDir, ".codex"),
  };
}

export interface WarmReadiness {
  indexedFiles: number;
  totalSegments: number;
}

/**
 * Establish real warm state for a copied workspace before the measured agent
 * starts. Runs an unconditional synchronous `1up index` (setup cost lives
 * outside every measured axis, REQ-001), then verifies readiness fail-closed
 * from `1up status -f json` counts.
 *
 * The gate consults only the current-context `indexed_files`/`total_segments`
 * counts and never the lifecycle/`index_status` string: a copied index reads
 * `"ready"` even when the current context has zero rows (HYP-001), so only the
 * counts prove the setup index produced a readable current-context index on the
 * current schema. Throws on verification failure.
 */
export function establishWarmReadiness(
  repoDir: string,
  homeDir: string,
): WarmReadiness {
  const env = workspaceEnv(homeDir);

  execFileSync("1up", ["index"], { cwd: repoDir, env, stdio: "pipe" });

  const rawStatus = execFileSync("1up", ["status", "-f", "json", "."], {
    cwd: repoDir,
    env,
    stdio: "pipe",
  }).toString();

  let status: CacheStatus;
  try {
    status = JSON.parse(rawStatus) as CacheStatus;
  } catch {
    throw new Error(
      `Warm readiness gate: could not parse '1up status -f json' output for ${repoDir}`,
    );
  }

  const indexedFiles = status.indexed_files;
  const totalSegments = status.total_segments;

  if (
    typeof indexedFiles !== "number" ||
    indexedFiles <= 0 ||
    typeof totalSegments !== "number" ||
    totalSegments <= 0
  ) {
    throw new Error(
      `Warm readiness gate failed for ${repoDir}: indexed_files=${
        indexedFiles ?? "null"
      } total_segments=${
        totalSegments ?? "null"
      } (setup index did not produce a readable current-context index; the index_status string is never trusted)`,
    );
  }

  return { indexedFiles, totalSegments };
}

/**
 * Strip the copied `.1up/` index cache so the cold-start suite exercises
 * bounded readiness waiting itself. The warm setup index and readiness gate are
 * skipped for cold cases.
 */
export function stripColdState(repoDir: string): void {
  rmSync(join(repoDir, ".1up"), { recursive: true, force: true });
}

export function cleanupWorkspace(workspaceDir: string): void {
  if (existsSync(workspaceDir)) {
    const repoDir = join(workspaceDir, "emdash");
    const homeDir = join(workspaceDir, "home");
    if (existsSync(repoDir)) {
      try {
        execFileSync("1up", ["stop", "--plain", "."], {
          cwd: repoDir,
          env: {
            ...process.env,
            HOME: homeDir,
            CODEX_HOME: join(homeDir, ".codex"),
          },
          stdio: "pipe",
        });
      } catch {
        // Workspace cleanup must still run when no daemon is registered.
      }
    }
    rmSync(workspaceDir, { recursive: true, force: true });
  }
}

export default async function (
  hookName: string,
  context: HookContext,
): Promise<void> {
  if (hookName === "beforeAll") {
    ensureFixtureCache();
    return;
  }

  if (hookName === "beforeEach") {
    const { workspaceDir, repoDir, homeDir, codexHomeDir } = createWorkspace();

    if (!context.test.vars) {
      context.test.vars = {};
    }
    context.test.vars.WORKSPACE_DIR = repoDir;
    context.test.vars.EVAL_BASE_DIR = workspaceDir;

    if (!context.test.options) {
      context.test.options = {};
    }
    context.test.options.working_dir = repoDir;

    context.test.vars._WORKSPACE_DIR = workspaceDir;
    context.test.vars._HOME = homeDir;
    context.test.vars._CODEX_HOME = codexHomeDir;
    context.test.vars._PATH = process.env.PATH ?? "";
    context.test.vars._NODE_EXTRA_CA_CERTS =
      process.env.NODE_EXTRA_CA_CERTS ?? "";

    if (context.test.vars.FIXTURE_MODE === "cold") {
      // Cold-start suite owns bounded readiness waiting itself: drop the copied
      // index cache and skip the warm setup index + readiness gate.
      stripColdState(repoDir);
      return;
    }

    // Warm suites: establish real ready state here, before the measured agent
    // starts, so it performs exactly one initial status check and never indexes
    // inside the measurement loop (REQ-001).
    const readiness = establishWarmReadiness(repoDir, homeDir);
    context.test.vars.WARM_READY = true;
    context.test.vars.WARM_INDEXED_FILES = readiness.indexedFiles;
    context.test.vars.WARM_TOTAL_SEGMENTS = readiness.totalSegments;

    return;
  }

  if (hookName === "afterEach") {
    const workspaceDir = context.test.vars?._WORKSPACE_DIR as
      | string
      | undefined;
    if (!workspaceDir) {
      return;
    }

    const preserve =
      process.env.PRESERVE_EVAL_WORKSPACES === "true" &&
      context.result?.success === false;

    if (preserve) {
      console.log(`Preserving workspace for failed test: ${workspaceDir}`);
      return;
    }

    cleanupWorkspace(workspaceDir);
  }
}
