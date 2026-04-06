import { execSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const EMDASH_REPO = "https://github.com/emdash-cms/emdash.git";
const EMDASH_COMMIT = "5beb0dd";
const __dirname = dirname(fileURLToPath(import.meta.url));
const CACHE_DIR = join(__dirname, "../../.cache/emdash");
const INDEX_DB_PATH = join(CACHE_DIR, ".1up/index.db");
const PROJECT_ID_PATH = join(CACHE_DIR, ".1up/project_id");
const TEMP_BASE = "/tmp/1up-evals";

interface HookContext {
  test: {
    vars?: Record<string, string | number | boolean | object>;
    options?: Record<string, unknown>;
  };
  result?: {
    success: boolean;
  };
}

function ensureFixtureCache(): void {
  if (existsSync(INDEX_DB_PATH)) {
    return;
  }

  mkdirSync(CACHE_DIR, { recursive: true });

  if (!existsSync(join(CACHE_DIR, ".git"))) {
    execSync(
      `git clone --single-branch --branch main ${EMDASH_REPO} "${CACHE_DIR}"`,
      { stdio: "pipe" },
    );
    execSync(`git -C "${CACHE_DIR}" checkout ${EMDASH_COMMIT}`, {
      stdio: "pipe",
    });
  }

  execSync("1up index", { cwd: CACHE_DIR, stdio: "pipe" });

  if (existsSync(PROJECT_ID_PATH)) {
    rmSync(PROJECT_ID_PATH);
  }
}

function createWorkspace(): string {
  const uuid = crypto.randomUUID();
  const workspaceDir = join(TEMP_BASE, uuid);
  const homeDir = join(workspaceDir, "home");

  mkdirSync(homeDir, { recursive: true });
  mkdirSync(join(homeDir, ".local/share"), { recursive: true });
  mkdirSync(join(homeDir, ".config"), { recursive: true });

  cpSync(CACHE_DIR, join(workspaceDir, "emdash"), { recursive: true });

  return workspaceDir;
}

function cleanupWorkspace(workspaceDir: string): void {
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
    const workspaceDir = createWorkspace();
    const emdashDir = join(workspaceDir, "emdash");
    const homeDir = join(workspaceDir, "home");

    if (!context.test.vars) {
      context.test.vars = {};
    }
    context.test.vars.WORKSPACE_DIR = emdashDir;
    context.test.vars.EVAL_BASE_DIR = workspaceDir;

    if (!context.test.options) {
      context.test.options = {};
    }
    context.test.options.working_dir = emdashDir;

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
