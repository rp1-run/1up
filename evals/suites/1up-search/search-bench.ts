import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, symlinkSync } from "node:fs";
import { join } from "node:path";

import {
  cleanupWorkspace,
  createWorkspace,
  ensureFixtureCache,
} from "../shared/extension";

interface BenchmarkCase {
  name: string;
  oneupQuery: string;
  baselineCommand: string;
}

interface SearchResult {
  file_path: string;
}

interface CommandSummary {
  resultCount: number;
  meanMs: number;
  minMs: number;
  maxMs: number;
}

interface CaseSummary {
  name: string;
  oneup: CommandSummary;
  baseline: CommandSummary;
}

const BENCHMARK_CASES: BenchmarkCase[] = [
  {
    name: "Search Stack",
    oneupQuery:
      "Trace how emdash content search is enabled and queried, from a field becoming searchable through to admin search results. Identify the key files involved in each step.",
    baselineCommand:
      'rg -n -S "FTSManager|searchSingleCollection|AdminCommandPalette" .',
  },
  {
    name: "WordPress Import",
    oneupQuery:
      "Explain the WordPress import pipeline from the admin wizard through schema preparation, WXR execution, and Gutenberg-to-Portable-Text conversion. Identify the key files involved in each step.",
    baselineCommand:
      'rg -n -S "WordPressImport|prepare\\.ts|execute\\.ts|gutenberg-to-portable-text" .',
  },
  {
    name: "Plugin Architecture",
    oneupQuery:
      "Trace how a sandboxed emdash plugin is registered, capability-gated, loaded into Cloudflare Worker isolation, and given controlled access to content, storage, and network. Identify the key files involved in each step.",
    baselineCommand:
      'rg -n -S "manager\\.ts|hooks\\.ts|runner\\.ts|wrapper\\.ts|bridge\\.ts" .',
  },
  {
    name: "Live Content Query",
    oneupQuery:
      "Explain how emdash stores schema in the database and exposes typed live content queries through Astro. Identify the key files involved in each step.",
    baselineCommand:
      'rg -n -S "registry\\.ts|loader\\.ts|query\\.ts|live\\.config\\.ts" .',
  },
];

const SEARCH_LIMIT = 5;
const WARMUP_RUNS = positiveIntegerEnv("ONEUP_SEARCH_BENCH_WARMUP", 1);
const MEASURED_RUNS = positiveIntegerEnv("ONEUP_SEARCH_BENCH_RUNS", 3);
const PASSING_TOTAL_MEAN_MS = positiveNumberEnv(
  "ONEUP_SEARCH_BENCH_THRESHOLD_MS",
  200,
);
const BENCHMARK_BINARY = process.env.ONEUP_BENCH_BIN ?? "1up";
const BASELINE_LABEL = "rg";

function positiveIntegerEnv(name: string, fallback: number): number {
  const rawValue = process.env[name];
  if (!rawValue) {
    return fallback;
  }

  const parsedValue = Number.parseInt(rawValue, 10);
  if (!Number.isInteger(parsedValue) || parsedValue < 1) {
    throw new Error(`${name} must be a positive integer, got "${rawValue}"`);
  }

  return parsedValue;
}

function positiveNumberEnv(name: string, fallback: number): number {
  const rawValue = process.env[name];
  if (!rawValue) {
    return fallback;
  }

  const parsedValue = Number(rawValue);
  if (!Number.isFinite(parsedValue) || parsedValue <= 0) {
    throw new Error(`${name} must be a positive number, got "${rawValue}"`);
  }

  return parsedValue;
}

function currentDataHome(): string | null {
  if (process.env.XDG_DATA_HOME) {
    return process.env.XDG_DATA_HOME;
  }

  if (!process.env.HOME) {
    return null;
  }

  return join(process.env.HOME, ".local/share");
}

function linkModelCache(homeDir: string): string | null {
  const dataHome = currentDataHome();
  if (!dataHome) {
    return null;
  }

  const sourceModelsDir = join(dataHome, "1up/models");
  if (!existsSync(sourceModelsDir)) {
    return null;
  }

  const targetOneupDir = join(homeDir, ".local/share/1up");
  const targetModelsDir = join(targetOneupDir, "models");
  mkdirSync(targetOneupDir, { recursive: true });

  if (!existsSync(targetModelsDir)) {
    symlinkSync(sourceModelsDir, targetModelsDir, "dir");
  }

  return sourceModelsDir;
}

function benchmarkEnv(homeDir: string): NodeJS.ProcessEnv {
  return {
    ...process.env,
    HOME: homeDir,
    XDG_DATA_HOME: join(homeDir, ".local/share"),
    XDG_CONFIG_HOME: join(homeDir, ".config"),
  };
}

function runOneupSearch(
  query: string,
  repoDir: string,
  homeDir: string,
): SearchResult[] {
  const rawOutput = execFileSync(
    BENCHMARK_BINARY,
    [
      "search",
      "-n",
      String(SEARCH_LIMIT),
      "--path",
      repoDir,
      "-f",
      "json",
      query,
    ],
    {
      encoding: "utf8",
      env: benchmarkEnv(homeDir),
      stdio: ["ignore", "pipe", "pipe"],
    },
  );

  const parsedOutput = JSON.parse(rawOutput) as unknown;
  if (!Array.isArray(parsedOutput) || parsedOutput.length === 0) {
    throw new Error(`search returned no results for benchmark query: ${query}`);
  }

  return parsedOutput as SearchResult[];
}

function runBaselineSearch(
  command: string,
  repoDir: string,
  homeDir: string,
): string[] {
  const rawOutput = execFileSync("bash", ["-lc", command], {
    cwd: repoDir,
    encoding: "utf8",
    env: benchmarkEnv(homeDir),
    stdio: ["ignore", "pipe", "pipe"],
  });

  const lines = rawOutput
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
  if (lines.length === 0) {
    throw new Error(
      `baseline search returned no results for command: ${command}`,
    );
  }

  return lines;
}

function measureCommand(run: () => number): CommandSummary {
  for (let runIndex = 0; runIndex < WARMUP_RUNS; runIndex += 1) {
    run();
  }

  const samplesMs: number[] = [];
  let resultCount = 0;

  for (let runIndex = 0; runIndex < MEASURED_RUNS; runIndex += 1) {
    const startedAt = process.hrtime.bigint();
    resultCount = run();
    const elapsedMs = Number(process.hrtime.bigint() - startedAt) / 1_000_000;
    samplesMs.push(elapsedMs);
  }

  const totalMs = samplesMs.reduce((sum, sample) => sum + sample, 0);

  return {
    resultCount,
    meanMs: totalMs / samplesMs.length,
    minMs: Math.min(...samplesMs),
    maxMs: Math.max(...samplesMs),
  };
}

function measureCase(
  benchCase: BenchmarkCase,
  repoDir: string,
  homeDir: string,
): CaseSummary {
  return {
    name: benchCase.name,
    oneup: measureCommand(
      () => runOneupSearch(benchCase.oneupQuery, repoDir, homeDir).length,
    ),
    baseline: measureCommand(
      () =>
        runBaselineSearch(benchCase.baselineCommand, repoDir, homeDir).length,
    ),
  };
}

function formatMs(value: number): string {
  return `${value.toFixed(1)} ms`;
}

function printSummary(
  summaries: CaseSummary[],
  aggregateOneupMeanMs: number,
  aggregateBaselineMeanMs: number,
  modelCachePath: string | null,
): void {
  const perQueryBudgetMs = PASSING_TOTAL_MEAN_MS / BENCHMARK_CASES.length;
  const outcome =
    aggregateOneupMeanMs <= PASSING_TOTAL_MEAN_MS ? "PASS" : "FAIL";

  console.log(`1up search comparison bench: ${outcome}`);
  console.log(`1up binary: ${BENCHMARK_BINARY}`);
  console.log(`Baseline: raw ${BASELINE_LABEL}`);
  console.log(`Queries: ${BENCHMARK_CASES.length}`);
  console.log(`Warmup runs/query: ${WARMUP_RUNS}`);
  console.log(`Measured runs/query: ${MEASURED_RUNS}`);
  console.log(
    `Passing goal: 1up aggregate mean <= ${formatMs(PASSING_TOTAL_MEAN_MS)} (${formatMs(perQueryBudgetMs)} average/query)`,
  );
  console.log(
    `Model cache: ${modelCachePath ?? "not linked; searches will run in FTS-only fallback mode"}`,
  );
  console.log("");

  for (const summary of summaries) {
    const speedup = summary.baseline.meanMs / summary.oneup.meanMs;
    console.log(
      `- ${summary.name}: 1up mean ${formatMs(summary.oneup.meanMs)} | ${BASELINE_LABEL} mean ${formatMs(summary.baseline.meanMs)} | speedup ${speedup.toFixed(2)}x | 1up results ${summary.oneup.resultCount} | ${BASELINE_LABEL} hits ${summary.baseline.resultCount}`,
    );
  }

  console.log("");
  console.log(`1up aggregate mean: ${formatMs(aggregateOneupMeanMs)}`);
  console.log(
    `${BASELINE_LABEL} aggregate mean: ${formatMs(aggregateBaselineMeanMs)}`,
  );
  console.log(
    `Aggregate speedup: ${(aggregateBaselineMeanMs / aggregateOneupMeanMs).toFixed(2)}x`,
  );
}

let workspaceDir: string | null = null;
let benchmarkFailed = false;
let preserveWorkspace = false;

try {
  ensureFixtureCache();

  const workspace = createWorkspace();
  workspaceDir = workspace.workspaceDir;

  const modelCachePath = linkModelCache(workspace.homeDir);
  const summaries = BENCHMARK_CASES.map((benchCase) =>
    measureCase(benchCase, workspace.repoDir, workspace.homeDir),
  );
  const aggregateOneupMeanMs = summaries.reduce(
    (sum, summary) => sum + summary.oneup.meanMs,
    0,
  );
  const aggregateBaselineMeanMs = summaries.reduce(
    (sum, summary) => sum + summary.baseline.meanMs,
    0,
  );

  printSummary(
    summaries,
    aggregateOneupMeanMs,
    aggregateBaselineMeanMs,
    modelCachePath,
  );

  if (aggregateOneupMeanMs > PASSING_TOTAL_MEAN_MS) {
    benchmarkFailed = true;
    preserveWorkspace = process.env.PRESERVE_EVAL_WORKSPACES === "true";
    process.exitCode = 1;
  }
} catch (error) {
  benchmarkFailed = true;
  preserveWorkspace = process.env.PRESERVE_EVAL_WORKSPACES === "true";
  throw error;
} finally {
  if (workspaceDir) {
    if (preserveWorkspace && benchmarkFailed) {
      console.log(`Preserving workspace for failed benchmark: ${workspaceDir}`);
    } else {
      cleanupWorkspace(workspaceDir);
    }
  }
}
