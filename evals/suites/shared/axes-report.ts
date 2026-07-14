/**
 * Per-axis Luna eval report and baseline recorder (REQ-002, REQ-006).
 *
 * Promptfoo's blended composite score is non-authoritative: it bundles every
 * assertion into one number and grants the isolated baseline automatic credit
 * for 1up-specific workflow assertions it never exercises. This report unbundles
 * scoring into independently interpretable axes computed directly from the
 * `componentResults` embedded in each per-case diagnostic JSON, tagged with the
 * `metric:` axis names that the suite YAMLs carry (T2). Each axis is an
 * independent mean over only its own tagged components, so a change to one axis'
 * assertions can never move another axis (REQ-002 AC2), and components marked
 * `NOT_APPLICABLE_REASON` are excluded and rendered `n/a` rather than counted as
 * a pass (so the baseline no longer inherits credit it did not earn).
 *
 * `--record-baseline` freezes the current per-axis figures into the committed
 * `suites/luna-baseline.json`, stamped with the manifest `contract_hash` (T5) so
 * a recorded baseline is only comparable against runs of the same frozen
 * contract.
 *
 * Usage:
 *   bun run suites/shared/axes-report.ts [--results-dir <dir>]
 *   bun run suites/shared/axes-report.ts --record-baseline [--manifest <path>] [--out <path>]
 */

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { isAbsolute, resolve } from "node:path";

import { NOT_APPLICABLE_REASON } from "./assertions/index.ts";

/** Graded axes: each is a mean over the `score` of its metric-tagged components. */
export const GRADED_AXES = [
  "factual",
  "retrieval",
  "adoption",
  "reliability",
] as const;
export type GradedAxis = (typeof GRADED_AXES)[number];

/**
 * Efficiency measures extracted from the `efficiency`-tagged reportRunMetrics
 * component's `namedScores` (raw usage, split into independent measures here).
 */
export const EFFICIENCY_MEASURES = [
  "latency_ms",
  "tokens",
  "cost_usd",
  "calls",
] as const;
export type EfficiencyMeasure = (typeof EFFICIENCY_MEASURES)[number];

/** A row of the append-only v2 `runs.tsv` produced by run-parallel.sh (T4). */
export interface RunRow {
  attemptId: string;
  label: string;
  role: string;
  retryOf: string;
  durationS: string;
  status: string;
  logPath: string;
  diagnosticPath: string;
}

interface ComponentResult {
  pass?: boolean;
  score?: number;
  reason?: string;
  namedScores?: Record<string, number> | null;
  assertion?: { type?: string; metric?: string } | null;
}

interface EvalResultRow {
  provider?: { label?: string; id?: string } | null;
  gradingResult?: { componentResults?: ComponentResult[] } | null;
  namedScores?: Record<string, number> | null;
  latencyMs?: number;
  cost?: number;
}

export interface DiagnosticJson {
  results?: { results?: EvalResultRow[] } | null;
}

/** A diagnostic JSON paired with the attempt that produced it. */
export interface DiagnosticInput {
  attemptId: string;
  diagnostic: DiagnosticJson;
}

/** Per-provider axis values; `null` means the axis does not apply (`n/a`). */
export interface ProviderAxes {
  factual: number | null;
  retrieval: number | null;
  adoption: number | null;
  reliability: number | null;
  latency_ms: number | null;
  tokens: number | null;
  cost_usd: number | null;
  calls: number | null;
  sourceAttemptIds: string[];
}

export type AxesReport = Record<string, ProviderAxes>;

interface Accumulator {
  graded: Record<GradedAxis, number[]>;
  efficiency: Record<EfficiencyMeasure, number[]>;
  attemptIds: Set<string>;
}

function newAccumulator(): Accumulator {
  return {
    graded: { factual: [], retrieval: [], adoption: [], reliability: [] },
    efficiency: { latency_ms: [], tokens: [], cost_usd: [], calls: [] },
    attemptIds: new Set<string>(),
  };
}

function isGradedAxis(metric: string | undefined): metric is GradedAxis {
  return (
    metric !== undefined && (GRADED_AXES as readonly string[]).includes(metric)
  );
}

function mean(values: number[]): number | null {
  if (values.length === 0) {
    return null;
  }
  const sum = values.reduce((acc, v) => acc + v, 0);
  return sum / values.length;
}

/**
 * Parse v2 `runs.tsv` (8 tab-separated columns, no header, append-only). Legacy
 * v1 rows (5 columns: label/duration/status/log/diagnostic, no attempt lineage)
 * are tolerated so older archived TSVs stay readable — they map to a synthetic
 * `aggregate` role so they still feed the report.
 */
export function parseRunsTsv(content: string): RunRow[] {
  const rows: RunRow[] = [];
  for (const line of content.split("\n")) {
    if (line.trim() === "") {
      continue;
    }
    const cols = line.split("\t");
    if (cols.length >= 8) {
      rows.push({
        attemptId: cols[0],
        label: cols[1],
        role: cols[2],
        retryOf: cols[3],
        durationS: cols[4],
        status: cols[5],
        logPath: cols[6],
        diagnosticPath: cols[7],
      });
    } else if (cols.length >= 5) {
      rows.push({
        attemptId: "",
        label: cols[0],
        role: "aggregate",
        retryOf: "-",
        durationS: cols[1],
        status: cols[2],
        logPath: cols[3],
        diagnosticPath: cols[4],
      });
    }
  }
  return rows;
}

/**
 * Read the number a reportRunMetrics component recorded for a measure, falling
 * back to the top-level result field when the named score is absent or zero
 * (the Codex/Luna SDK path leaves `durationMs` at 0, so real latency lives in
 * the result's `latencyMs`; cost likewise mirrors `result.cost`).
 */
function extractEfficiency(
  component: ComponentResult,
  result: EvalResultRow,
): Record<EfficiencyMeasure, number> {
  const ns = component.namedScores ?? {};
  const latencyNamed = typeof ns.latency_ms === "number" ? ns.latency_ms : 0;
  const costNamed = typeof ns.cost_usd === "number" ? ns.cost_usd : undefined;
  return {
    latency_ms: latencyNamed > 0 ? latencyNamed : (result.latencyMs ?? 0),
    tokens: typeof ns.tokens === "number" ? ns.tokens : 0,
    cost_usd: costNamed ?? result.cost ?? 0,
    calls: typeof ns.calls === "number" ? ns.calls : 0,
  };
}

/**
 * Fold per-case diagnostics into per-provider axis means. Each component is
 * routed by its `assertion.metric`: graded axes average the component `score`
 * (excluding `NOT_APPLICABLE_REASON`-marked components), the `efficiency` metric
 * contributes the raw latency/tokens/cost/calls measures. Axes with no
 * applicable component resolve to `null` (rendered `n/a`).
 */
export function aggregateAxes(inputs: DiagnosticInput[]): AxesReport {
  const byProvider = new Map<string, Accumulator>();

  for (const { attemptId, diagnostic } of inputs) {
    const resultRows = diagnostic.results?.results ?? [];
    for (const result of resultRows) {
      const provider = result.provider?.label ?? "unknown";
      let acc = byProvider.get(provider);
      if (acc === undefined) {
        acc = newAccumulator();
        byProvider.set(provider, acc);
      }
      if (attemptId !== "") {
        acc.attemptIds.add(attemptId);
      }

      const components = result.gradingResult?.componentResults ?? [];
      for (const component of components) {
        const metric = component.assertion?.metric;
        if (isGradedAxis(metric)) {
          const reason = component.reason ?? "";
          if (reason.startsWith(NOT_APPLICABLE_REASON)) {
            continue;
          }
          acc.graded[metric].push(component.score ?? 0);
        } else if (metric === "efficiency") {
          const measures = extractEfficiency(component, result);
          for (const key of EFFICIENCY_MEASURES) {
            acc.efficiency[key].push(measures[key]);
          }
        }
      }
    }
  }

  const report: AxesReport = {};
  for (const [provider, acc] of byProvider) {
    report[provider] = {
      factual: mean(acc.graded.factual),
      retrieval: mean(acc.graded.retrieval),
      adoption: mean(acc.graded.adoption),
      reliability: mean(acc.graded.reliability),
      latency_ms: mean(acc.efficiency.latency_ms),
      tokens: mean(acc.efficiency.tokens),
      cost_usd: mean(acc.efficiency.cost_usd),
      calls: mean(acc.efficiency.calls),
      sourceAttemptIds: [...acc.attemptIds].sort(),
    };
  }
  return report;
}

type DisplayAxis = Exclude<keyof ProviderAxes, "sourceAttemptIds">;

const DISPLAY_AXES: readonly DisplayAxis[] = [
  "factual",
  "retrieval",
  "adoption",
  "reliability",
  "latency_ms",
  "tokens",
  "cost_usd",
  "calls",
];

function formatCell(axis: DisplayAxis, value: number | null): string {
  if (value === null) {
    return "n/a";
  }
  switch (axis) {
    case "latency_ms":
      return `${Math.round(value)}`;
    case "tokens":
    case "calls":
      return value.toFixed(1);
    case "cost_usd":
      return `$${value.toFixed(4)}`;
    default:
      return value.toFixed(3);
  }
}

/** Render a per-axis Markdown table with one column per provider. */
export function renderTable(report: AxesReport): string {
  const providers = Object.keys(report).sort();
  if (providers.length === 0) {
    return "No results found (empty runs.tsv or no diagnostics).";
  }
  const header = ["axis", ...providers];
  const divider = header.map(() => "---");
  const lines = [`| ${header.join(" | ")} |`, `| ${divider.join(" | ")} |`];
  for (const axis of DISPLAY_AXES) {
    const cells = providers.map((p) => formatCell(axis, report[p][axis]));
    lines.push(`| ${axis} | ${cells.join(" | ")} |`);
  }
  return lines.join("\n");
}

export interface Baseline {
  contract_hash: string;
  captured_at: string;
  providers: Record<
    string,
    {
      factual: number | null;
      retrieval: number | null;
      adoption: number | null;
      reliability: number | null;
      latency_ms: number | null;
      tokens: number | null;
      cost_usd: number | null;
      calls: number | null;
      source_attempt_ids: string[];
    }
  >;
}

/** Freeze a report into the committed baseline shape, stamped with the contract hash. */
export function buildBaseline(
  report: AxesReport,
  contractHash: string,
  capturedAt: string,
): Baseline {
  const providers: Baseline["providers"] = {};
  for (const [name, axes] of Object.entries(report)) {
    providers[name] = {
      factual: axes.factual,
      retrieval: axes.retrieval,
      adoption: axes.adoption,
      reliability: axes.reliability,
      latency_ms: axes.latency_ms,
      tokens: axes.tokens,
      cost_usd: axes.cost_usd,
      calls: axes.calls,
      source_attempt_ids: axes.sourceAttemptIds,
    };
  }
  return { contract_hash: contractHash, captured_at: capturedAt, providers };
}

/**
 * Load the frozen contract hash from the T5 manifest. Absent until the manifest
 * lands; a placeholder keeps the report usable in-agent while flagging that the
 * recorded baseline is not yet contract-stamped.
 */
function readContractHash(manifestPath: string): string {
  if (!existsSync(manifestPath)) {
    console.warn(
      `[axes-report] manifest not found at ${manifestPath}; stamping contract_hash="unknown-no-manifest"`,
    );
    return "unknown-no-manifest";
  }
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8")) as {
    contract_hash?: string;
  };
  if (typeof manifest.contract_hash !== "string") {
    console.warn(
      `[axes-report] manifest at ${manifestPath} has no contract_hash; stamping "unknown-no-manifest"`,
    );
    return "unknown-no-manifest";
  }
  return manifest.contract_hash;
}

interface CliOptions {
  resultsDir: string;
  recordBaseline: boolean;
  manifestPath: string;
  outPath: string;
}

function parseArgs(argv: string[]): CliOptions {
  const opts: CliOptions = {
    resultsDir: "results/latest-luna",
    recordBaseline: false,
    manifestPath: "suites/luna-manifest.json",
    outPath: "suites/luna-baseline.json",
  };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    switch (arg) {
      case "--record-baseline":
        opts.recordBaseline = true;
        break;
      case "--results-dir":
        opts.resultsDir = argv[++i];
        break;
      case "--manifest":
        opts.manifestPath = argv[++i];
        break;
      case "--out":
        opts.outPath = argv[++i];
        break;
      default:
        throw new Error(`Unknown argument: ${arg}`);
    }
  }
  return opts;
}

/**
 * Load the diagnostics referenced by an aggregate-role run. Only `aggregate`
 * rows feed the canonical per-axis means; `planned-repeat`/`diagnostic` retries
 * are lineage, not the measured baseline, so they never skew the axes.
 */
function loadAggregateDiagnostics(resultsDir: string): DiagnosticInput[] {
  const runsPath = resolve(resultsDir, "runs.tsv");
  if (!existsSync(runsPath)) {
    throw new Error(`runs.tsv not found at ${runsPath}`);
  }
  const rows = parseRunsTsv(readFileSync(runsPath, "utf8"));
  const inputs: DiagnosticInput[] = [];
  for (const row of rows) {
    if (row.role !== "aggregate") {
      continue;
    }
    const path = isAbsolute(row.diagnosticPath)
      ? row.diagnosticPath
      : resolve(process.cwd(), row.diagnosticPath);
    if (!existsSync(path)) {
      console.warn(`[axes-report] diagnostic missing, skipping: ${path}`);
      continue;
    }
    const diagnostic = JSON.parse(readFileSync(path, "utf8")) as DiagnosticJson;
    inputs.push({ attemptId: row.attemptId, diagnostic });
  }
  return inputs;
}

function main(): void {
  const opts = parseArgs(process.argv.slice(2));
  const inputs = loadAggregateDiagnostics(opts.resultsDir);
  const report = aggregateAxes(inputs);

  console.log(renderTable(report));

  if (opts.recordBaseline) {
    const contractHash = readContractHash(opts.manifestPath);
    const baseline = buildBaseline(
      report,
      contractHash,
      new Date().toISOString(),
    );
    writeFileSync(opts.outPath, `${JSON.stringify(baseline, null, 2)}\n`);
    console.log(`\nBaseline written to ${opts.outPath}`);
  }
}

if (import.meta.main) {
  main();
}
