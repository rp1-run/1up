/**
 * Versioned, hashed Luna benchmark manifest (REQ-003).
 *
 * A trustworthy comparison across phases requires proof that two runs were
 * measured against the same benchmark rules. This generator freezes the warm
 * Luna benchmark contract — the prompt files and suite YAMLs finalized by T1/T2,
 * the fixture repo+commit the suites index, the llm-rubric grader identity, the
 * per-axis mapping, the cost transform, and the trial count — into a single
 * committed `suites/luna-manifest.json`. The `contract_hash` is the sha256 of
 * the canonical (recursively sorted-key) JSON of the `contract`, so any change
 * to a contract input flips the hash and a `luna-baseline.json` stamped with an
 * old hash is visibly incomparable.
 *
 * The manifest lives under tracked `evals/suites/` and hashes only tracked
 * benchmark-definition inputs; no gitignored run-output directory feeds it
 * (REQ-003 AC2). `manifest.test.ts` drift-guards the committed hash against a
 * live recomputation.
 *
 * Usage:
 *   npm run manifest:luna   # regenerate suites/luna-manifest.json (biome-formatted)
 *
 * The generator emits JSON.stringify output; the npm script runs biome over it
 * so the committed file stays stable under the repo formatter. The file's
 * on-disk layout does not affect `contract_hash` (hashed over inputs, not text).
 */

import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { EFFICIENCY_MEASURES, GRADED_AXES } from "./axes-report.ts";

/** Bump when the manifest field shape changes (not when contract inputs move). */
export const MANIFEST_VERSION = "1";

const __dirname = dirname(fileURLToPath(import.meta.url));
/** Everything is resolved from the evals root so the generator is cwd-independent. */
const EVALS_ROOT = resolve(__dirname, "../..");

/** Manifest output path, relative to the evals root. */
export const MANIFEST_RELATIVE_PATH = "suites/luna-manifest.json";

/**
 * The emdash fixture the warm suites index. Mirrors the (unexported) constants
 * in `extension.ts`; the manifest declares the fixture identity it was frozen
 * against, so a fixture bump must be reflected here and regenerated.
 */
const FIXTURE_REPO = "https://github.com/emdash-cms/emdash.git";
const FIXTURE_COMMIT = "5beb0dd";

/**
 * The llm-rubric grader both warm suites judge factual axes with (the
 * `defaultTest.options.provider` in each `evals-luna.yaml`).
 */
const GRADER_PROVIDER = "openai:codex-sdk";
const GRADER_MODEL = "gpt-5.6-luna";

/** Warm-suite prompt files (content-hashed), relative to the evals root. */
const PROMPT_FILES = [
  "suites/1up-search/prompt-1up-warm.txt",
  "suites/1up-search/prompt-baseline.txt",
  "suites/1up-impact/prompt-1up-warm.txt",
  "suites/1up-impact/prompt-baseline.txt",
];

/**
 * Warm-suite YAMLs (content-hashed), relative to the evals root. Graders,
 * rubrics, and weights live inside these files, so their hashes capture any
 * grading change. The cold-start suite is a separate reliability check and is
 * intentionally not part of the warm baseline contract.
 */
const SUITE_FILES = [
  "suites/1up-search/evals-luna.yaml",
  "suites/1up-impact/evals-luna.yaml",
];

/** The cost transform baselines applied when normalizing efficiency axes. */
const COST_TRANSFORM = {
  speed_baseline_s: 200,
  cost_baseline_usd: 0.5,
} as const;

/** How many trials each case is measured over (promptfoo `repeat`, default 1). */
const TRIAL_COUNT = 1;

/** A content-addressed benchmark input: its evals-relative path and file sha256. */
export interface FileHash {
  path: string;
  sha256: string;
}

/** The frozen benchmark contract; its canonical hash is `contract_hash`. */
export interface Contract {
  fixture: { repo: string; commit: string };
  prompts: FileHash[];
  suites: FileHash[];
  grader: { provider: string; model: string };
  axes: { graded: string[]; efficiency: string[] };
  cost_transform: { speed_baseline_s: number; cost_baseline_usd: number };
  trial_count: number;
}

export interface Manifest {
  manifest_version: string;
  contract_hash: string;
  contract: Contract;
}

/**
 * Serialize a value to canonical JSON: object keys are sorted recursively so the
 * hash is independent of key insertion order; arrays keep their order (the file
 * lists are pre-sorted by path). Primitives serialize as ordinary JSON.
 */
export function canonicalize(value: unknown): string {
  if (value === null || typeof value !== "object") {
    return JSON.stringify(value) ?? "null";
  }
  if (Array.isArray(value)) {
    return `[${value.map(canonicalize).join(",")}]`;
  }
  const record = value as Record<string, unknown>;
  const entries = Object.keys(record)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${canonicalize(record[key])}`);
  return `{${entries.join(",")}}`;
}

/** The contract hash: sha256 of the canonical sorted-key JSON of the contract. */
export function contractHash(contract: Contract): string {
  return createHash("sha256").update(canonicalize(contract)).digest("hex");
}

function fileHash(relativePath: string): FileHash {
  const bytes = readFileSync(resolve(EVALS_ROOT, relativePath));
  return {
    path: relativePath,
    sha256: createHash("sha256").update(bytes).digest("hex"),
  };
}

function byPath(a: FileHash, b: FileHash): number {
  return a.path < b.path ? -1 : a.path > b.path ? 1 : 0;
}

/** Assemble the contract by content-hashing the live benchmark-definition files. */
export function buildContract(): Contract {
  return {
    fixture: { repo: FIXTURE_REPO, commit: FIXTURE_COMMIT },
    prompts: PROMPT_FILES.map(fileHash).sort(byPath),
    suites: SUITE_FILES.map(fileHash).sort(byPath),
    grader: { provider: GRADER_PROVIDER, model: GRADER_MODEL },
    axes: { graded: [...GRADED_AXES], efficiency: [...EFFICIENCY_MEASURES] },
    cost_transform: {
      speed_baseline_s: COST_TRANSFORM.speed_baseline_s,
      cost_baseline_usd: COST_TRANSFORM.cost_baseline_usd,
    },
    trial_count: TRIAL_COUNT,
  };
}

/** Build the full manifest (contract + its canonical hash + version). */
export function buildManifest(): Manifest {
  const contract = buildContract();
  return {
    manifest_version: MANIFEST_VERSION,
    contract_hash: contractHash(contract),
    contract,
  };
}

function main(): void {
  const manifest = buildManifest();
  const outPath = resolve(EVALS_ROOT, MANIFEST_RELATIVE_PATH);
  writeFileSync(outPath, `${JSON.stringify(manifest, null, 2)}\n`);
  console.log(
    `Manifest written to ${outPath} (contract_hash=${manifest.contract_hash})`,
  );
}

if (import.meta.main) {
  main();
}
