import { execSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdirSync,
  rmSync,
  writeFileSync,
  unlinkSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const EMDASH_REPO = "https://github.com/emdash-cms/emdash.git";
const EMDASH_COMMIT = "5beb0dd";
const __dirname = dirname(fileURLToPath(import.meta.url));
const CACHE_DIR = join(__dirname, "../../.cache/emdash");
const INDEX_DB_PATH = join(CACHE_DIR, ".1up/index.db");
const PROJECT_ID_PATH = join(CACHE_DIR, ".1up/project_id");
const CACHE_LOCK_PATH = join(__dirname, "../../.cache/.lock");
const TEMP_BASE = "/tmp/1up-evals";

interface CacheStatus {
  indexed_files?: number | null;
  total_segments?: number | null;
}

export interface FixtureWorkspace {
  workspaceDir: string;
  repoDir: string;
  homeDir: string;
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
    if (existsSync(PROJECT_ID_PATH)) {
      rmSync(PROJECT_ID_PATH);
    }
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

    if (existsSync(PROJECT_ID_PATH)) {
      rmSync(PROJECT_ID_PATH);
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

export function createWorkspace(): FixtureWorkspace {
  const uuid = crypto.randomUUID();
  const workspaceDir = join(TEMP_BASE, uuid);
  const homeDir = join(workspaceDir, "home");
  const repoDir = join(workspaceDir, "emdash");

  mkdirSync(homeDir, { recursive: true });
  mkdirSync(join(homeDir, ".local/share"), { recursive: true });
  mkdirSync(join(homeDir, ".config"), { recursive: true });

  cpSync(CACHE_DIR, repoDir, { recursive: true });

  return { workspaceDir, repoDir, homeDir };
}

export function cleanupWorkspace(workspaceDir: string): void {
  if (existsSync(workspaceDir)) {
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
    const { workspaceDir, repoDir, homeDir } = createWorkspace();

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
