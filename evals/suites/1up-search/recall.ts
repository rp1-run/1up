/**
 * Deterministic recall@k harness for 1up semantic search.
 *
 * Contract:
 * - Reads a curated gold corpus from `recall-corpus.jsonl` (one JSON object per line).
 *   Each row: { query: string, expected_anchors?: Anchor[], expected_segment_ids?: string[],
 *               expected_files?: string[] }.
 * - For each row, executes `1up search <query> -n <max_k>` against the target repo. The
 *   core `search` command emits the lean row grammar
 *   `<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>` on stdout
 *   (no more `-f json`). The harness parses rows into a lightweight result shape and
 *   lazily hydrates full segment bodies through `1up get <handle>` when an anchor
 *   requires content-based matching (line_contains or Rust-definition content heuristic).
 * - Scores each retrieved result against the gold list. Recall@k = |matched_gold| /
 *   |gold| per query, averaged across queries.
 * - Writes `recall-results.json` next to this file with per-k entries { k, recall, per_query }.
 *
 * KEEP: anchor-based matching -- gold is expressed as (file, symbol) or (file, line_contains)
 * pairs rather than segment-hash IDs. Segment IDs are SHA-256 of "file:line_start:line_end",
 * so ANY edit that shifts line numbers in a referenced file invalidates hash gold and
 * destroys the recall signal for reasons unrelated to storage format or ranker quality.
 * Anchors survive line drift because they bind to semantic identity (symbol definitions
 * or invariant text fragments), not to line ranges. A corpus row may still include
 * `expected_segment_ids` for legacy rows -- when both are present, the anchor match is
 * used and hash gold is ignored. When only `expected_segment_ids` is present, the
 * harness falls back to exact-hash matching.
 *
 * Match predicate for a single anchor `a = { file, symbol? , line_contains? }` against a
 * single search result `r`:
 *   1. `r.file_path === a.file` (required).
 *   2. If `a.symbol` is set: any of
 *        - `r.defined_symbols` contains exactly `a.symbol`
 *        - `r.breadcrumb` split on "::" / "." / "/" contains `a.symbol`
 *        - `r.content` contains a word-boundary occurrence of `a.symbol` on a line that
 *          also contains a Rust definition keyword (`fn`, `struct`, `enum`, `const`,
 *          `impl`, `trait`, `type`, `mod`, `static`, `macro_rules!`). This catches
 *          segments whose primary defined symbol is not in `defined_symbols` (e.g. DDL
 *          strings named via `pub const FOO: &str = "..."` where the content-visible
 *          symbol is FOO).
 *   3. If `a.line_contains` is set: `r.content` substring-contains `a.line_contains`.
 *   4. If both `a.symbol` and `a.line_contains` are set: both must match (AND).
 *
 * Recall per query: count how many DISTINCT anchors were matched by any retrieved result
 * in top-k, divided by total anchors. (We dedupe on anchor identity, not on result
 * identity, so several results pointing at the same anchor count once -- this matches the
 * intent "how much of the gold did we surface".)
 *
 * Resilience requirements (from feature task T3):
 * - Rows with empty gold (no anchors and no segment IDs) are skipped for recall but still
 *   recorded with status="skipped_no_gold" so the harness never produces NaN.
 * - An empty corpus produces recall = 0 (not NaN) with empty per_query.
 *
 * Quality gate (feature task T2):
 * - Before scoring, a semantic-path preflight asserts (via `1up status <repo> -f json`)
 *   that the index has embeddings (`vector_rows > 0`), reports a current `schema_version`,
 *   and serves the expected `embedding_model` variant (`ONEUP_MODEL_VARIANT`, default int8).
 *   A vectorless/schema-less/wrong-variant run fails closed instead of scoring silently.
 * - Per-query search stderr is captured (no longer discarded) and any degraded / FTS-only
 *   wording fails the run.
 * - After scoring, recall is compared against the pinned `recall-baseline.json` within an
 *   absolute per-k tolerance (default 0.02, override `ONEUP_RECALL_TOLERANCE`). An
 *   out-of-tolerance regression sets `process.exitCode = 1` and prints a `FAIL:` line;
 *   a missing baseline or corpus/config mismatch is an explicit gate error.
 * - `recall-results.json` gains `gates{}` and `delta_vs_baseline`. Setting
 *   `RECALL_CAPTURE_BASELINE=1` writes a fresh structured `recall-baseline.json` from the
 *   current run (the only sanctioned way to move the baseline — see evals/README.md) and
 *   skips the regression comparison.
 *
 * Target repo selection (in priority order):
 *   1. `RECALL_TARGET_REPO` env var (absolute path)
 *   2. Git toplevel of this file (the 1up repo root)
 * Binary selection:
 *   1. `RECALL_ONEUP_BIN` env var
 *   2. `ONEUP_BENCH_BIN` env var (reused from search-bench.ts convention)
 *   3. `1up` on PATH
 */

import { execFileSync, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
  type CorpusIdentity,
  type ModelVariant,
  type PreflightResult,
  type RecallBaseline,
  type RecallCandidate,
  RecallCompareError,
  type RecallComparison,
  compareRecall,
  detectDegradedStderr,
  evaluateStatusPreflight,
  resolveExpectedVariant,
  resolveTolerance,
} from "./recall-compare.ts";

interface Anchor {
  file: string;
  symbol?: string;
  line_contains?: string;
}

interface CorpusRow {
  query: string;
  expected_anchors?: Anchor[];
  expected_segment_ids?: string[];
  expected_files?: string[];
}

/**
 * Lean discovery row reshaped into an object so the rest of the harness can
 * score against named fields. `content` and full `defined_symbols` are
 * populated lazily through `hydrateSegment` only when an anchor requires
 * content-based matching.
 */
interface SearchResultJson {
  segment_id?: string;
  file_path?: string;
  breadcrumb?: string;
  defined_symbols?: string[];
  line_number?: number;
  line_end?: number;
}

type MatchMode = "anchor" | "segment_id";

interface PerQueryResult {
  query: string;
  status: "scored" | "skipped_no_gold";
  match_mode: MatchMode | null;
  retrieved_top_k: Array<{
    segment_id?: string;
    file_path?: string;
    breadcrumb?: string;
  }>;
  gold_size: number;
  matched_indices: number[];
  hit_count: number;
  recall: number;
}

interface RecallReport {
  k: number;
  recall: number;
  scored_queries: number;
  total_queries: number;
  per_query: PerQueryResult[];
}

interface RecallGates {
  expected_variant: ModelVariant;
  tolerance: number;
  preflight: PreflightResult;
  degraded_stderr_queries: Array<{ query: string; markers: string[] }>;
  recall: {
    verdict: "pass" | "fail" | "error";
    error?: string;
    regressions?: RecallComparison["regressions"];
  } | null;
}

interface HarnessOutput {
  schema_version: number | null;
  vector_rows: number | null;
  embedding_model: string | null;
  model_id: string | null;
  max_tokens: number | null;
  target_repo: string;
  binary: string;
  captured_at: string;
  corpus_size: number;
  corpus: CorpusIdentity;
  corpus_match_mode_counts: Record<MatchMode | "none", number>;
  recall_at_10: number | null;
  recall_at_20: number | null;
  reports: RecallReport[];
  gates: RecallGates;
  delta_vs_baseline: { recall_at_10: number; recall_at_20: number } | null;
}

const __dirname = dirname(fileURLToPath(import.meta.url));
const CORPUS_PATH = join(__dirname, "recall-corpus.jsonl");
const RESULTS_PATH = join(__dirname, "recall-results.json");
const BASELINE_PATH = join(__dirname, "recall-baseline.json");
const K_VALUES = [10, 20] as const;
const MAX_K = Math.max(...K_VALUES);

const RUST_DEFINITION_KEYWORDS = [
  "fn",
  "struct",
  "enum",
  "const",
  "impl",
  "trait",
  "type",
  "mod",
  "static",
  "macro_rules!",
];

function resolveBinary(): string {
  // Resolve env-provided paths against CWD so a `cd evals && ONEUP_BENCH_BIN=../target/debug/1up`
  // invocation does what the caller expects; absolute paths pass through
  // unchanged. PATH-lookup fallback is intentionally disabled: the harness
  // must run against a repo-local build so regressions in this tree cannot be
  // masked by an older installed release.
  const override = process.env.RECALL_ONEUP_BIN ?? process.env.ONEUP_BENCH_BIN;
  if (override && override.length > 0) {
    const resolved = resolve(process.cwd(), override);
    if (!existsSync(resolved)) {
      throw new Error(
        `binary override resolved to ${resolved} (from ${override}), but file does not exist.`,
      );
    }
    return resolved;
  }
  const repoRoot = resolve(__dirname, "..", "..", "..");
  const repoLocal = join(repoRoot, "target", "debug", "1up");
  if (!existsSync(repoLocal)) {
    throw new Error(
      `expected repo-local binary at ${repoLocal}; run \`cargo build --bin 1up\` or set RECALL_ONEUP_BIN/ONEUP_BENCH_BIN.`,
    );
  }
  return repoLocal;
}

function resolveTargetRepo(): string {
  const envRepo = process.env.RECALL_TARGET_REPO;
  if (envRepo && envRepo.length > 0) {
    return resolve(envRepo);
  }
  try {
    const toplevel = execFileSync(
      "git",
      ["-C", __dirname, "rev-parse", "--show-toplevel"],
      { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
    ).trim();
    if (toplevel.length > 0) {
      return toplevel;
    }
  } catch {
    // fall through
  }
  return process.cwd();
}

function readCorpus(): CorpusRow[] {
  if (!existsSync(CORPUS_PATH)) {
    throw new Error(`corpus not found at ${CORPUS_PATH}`);
  }
  const raw = readFileSync(CORPUS_PATH, "utf8");
  const rows: CorpusRow[] = [];
  for (const rawLine of raw.split("\n")) {
    const line = rawLine.trim();
    if (line.length === 0 || line.startsWith("//")) {
      continue;
    }
    const parsed = JSON.parse(line) as CorpusRow;
    if (typeof parsed.query !== "string" || parsed.query.length === 0) {
      throw new Error(`corpus row missing query: ${line}`);
    }
    rows.push(parsed);
  }
  return rows;
}

/**
 * Parse one lean discovery row into a search result object. Grammar:
 *   `<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>`.
 * Fields are separated by two ASCII spaces; we split on the fixed separator
 * to keep single spaces inside breadcrumbs from being misread.
 */
function parseLeanRow(line: string): SearchResultJson | null {
  if (line.length === 0) {
    return null;
  }
  const parts = line.split("  ");
  if (parts.length < 5) {
    return null;
  }
  const pathAndLines = parts[1];
  const lastColon = pathAndLines.lastIndexOf(":");
  if (lastColon <= 0) {
    return null;
  }
  const filePath = pathAndLines.slice(0, lastColon);
  const lineSpan = pathAndLines.slice(lastColon + 1);
  const dash = lineSpan.indexOf("-");
  const lineNumber = dash > 0 ? Number(lineSpan.slice(0, dash)) : undefined;
  const lineEnd = dash > 0 ? Number(lineSpan.slice(dash + 1)) : undefined;
  const breadcrumbSymbol = parts[3];
  const sep = breadcrumbSymbol.indexOf("::");
  const breadcrumb = sep >= 0 ? breadcrumbSymbol.slice(0, sep) : undefined;
  const symbol = sep >= 0 ? breadcrumbSymbol.slice(sep + 2) : undefined;
  const segmentToken = parts[4];
  const segmentId = segmentToken.startsWith(":")
    ? segmentToken.slice(1)
    : segmentToken;
  return {
    segment_id: segmentId,
    file_path: filePath,
    breadcrumb: breadcrumb === "-" ? undefined : breadcrumb,
    defined_symbols: symbol && symbol !== "-" ? [symbol] : undefined,
    line_number: Number.isFinite(lineNumber) ? lineNumber : undefined,
    line_end: Number.isFinite(lineEnd) ? lineEnd : undefined,
  };
}

/**
 * Run one search, returning parsed rows *and* the captured stderr. stderr is no
 * longer discarded (as the old `execFileSync` did): the preflight gate inspects
 * it for degraded / FTS-only wording so a vectorless run cannot score silently.
 */
function runSearch(
  binary: string,
  query: string,
  repoDir: string,
  k: number,
): { rows: SearchResultJson[]; stderr: string } {
  const result = spawnSync(
    binary,
    ["search", "-n", String(k), "--path", repoDir, query],
    {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      cwd: repoDir,
    },
  );
  if (result.error) {
    throw result.error;
  }
  if (typeof result.status === "number" && result.status !== 0) {
    throw new Error(
      `1up search exited ${result.status} for query "${query}": ${result.stderr ?? ""}`,
    );
  }
  const rows: SearchResultJson[] = [];
  for (const rawLine of (result.stdout ?? "").split("\n")) {
    const parsed = parseLeanRow(rawLine);
    if (parsed !== null) {
      rows.push(parsed);
    }
  }
  return { rows, stderr: result.stderr ?? "" };
}

/**
 * Hydrate a segment handle through `1up get <handle>` and return the body plus
 * `defined_symbols` parsed from the tab-delimited metadata line. The `get`
 * record shape is `segment <id>\n<tab-meta>\n\n<body>\n\n---\n` (design §2.3);
 * `not_found\t<raw>\n---\n` signals an unresolved handle.
 *
 * Returns `null` when the handle does not resolve, so callers can treat
 * content-based matching as a miss without throwing.
 */
function hydrateSegment(
  binary: string,
  handle: string,
  repoDir: string,
): { content: string; defined_symbols: string[]; breadcrumb?: string } | null {
  if (!handle) {
    return null;
  }
  const raw = execFileSync(binary, ["get", handle, "--path", repoDir], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    cwd: repoDir,
  });
  const lines = raw.split("\n");
  if (lines[0] === undefined || !lines[0].startsWith("segment ")) {
    return null;
  }
  const metaLine = lines[1] ?? "";
  const metaTokens = metaLine.split("\t");
  const meta = new Map<string, string>();
  for (let i = 0; i + 1 < metaTokens.length; i += 2) {
    meta.set(metaTokens[i], metaTokens[i + 1]);
  }
  // The blank line after metadata precedes the body; find it and collect body
  // until the `---` sentinel (or previous blank line).
  let idx = 2;
  if (lines[idx] === "") {
    idx += 1;
  }
  const bodyLines: string[] = [];
  for (; idx < lines.length; idx += 1) {
    const current = lines[idx];
    if (current === "---") {
      break;
    }
    if (current === "" && lines[idx + 1] === "---") {
      idx += 1;
      break;
    }
    bodyLines.push(current);
  }
  const defines = (meta.get("defines") ?? "")
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
  const breadcrumb = meta.get("breadcrumb");
  return {
    content: bodyLines.join("\n"),
    defined_symbols: defines,
    breadcrumb: breadcrumb && breadcrumb !== "-" ? breadcrumb : undefined,
  };
}

function escapeRegex(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function contentHasSymbolDefinition(content: string, symbol: string): boolean {
  const symbolPattern = new RegExp(`\\b${escapeRegex(symbol)}\\b`);
  for (const rawLine of content.split("\n")) {
    if (!symbolPattern.test(rawLine)) {
      continue;
    }
    for (const keyword of RUST_DEFINITION_KEYWORDS) {
      const keywordPattern = new RegExp(`\\b${escapeRegex(keyword)}`);
      if (keywordPattern.test(rawLine)) {
        return true;
      }
    }
  }
  return false;
}

function breadcrumbContainsSymbol(
  breadcrumb: string | undefined,
  symbol: string,
): boolean {
  if (!breadcrumb) {
    return false;
  }
  const parts = breadcrumb.split(/[:./]/).filter((s) => s.length > 0);
  return parts.includes(symbol);
}

/**
 * Lazy content hydrator: calls `1up get` once per segment handle and memoizes
 * the body + defined_symbols + breadcrumb so each scored query pays at most
 * one hydration per unique retrieved result. Misses (unresolved handles, empty
 * id) are cached as `null` to avoid repeat-on-miss.
 */
type HydrateFn = (
  handle: string | undefined,
) => ReturnType<typeof hydrateSegment>;

function makeHydrator(binary: string, repoDir: string): HydrateFn {
  const cache = new Map<string, ReturnType<typeof hydrateSegment>>();
  return (handle: string | undefined) => {
    if (!handle) {
      return null;
    }
    if (cache.has(handle)) {
      return cache.get(handle) ?? null;
    }
    let hydrated: ReturnType<typeof hydrateSegment> = null;
    try {
      hydrated = hydrateSegment(binary, handle, repoDir);
    } catch {
      hydrated = null;
    }
    cache.set(handle, hydrated);
    return hydrated;
  };
}

interface HydratedView {
  content: string;
  defined_symbols: string[];
  breadcrumb?: string;
}

function hydrateOrEmpty(
  result: SearchResultJson,
  hydrate: HydrateFn,
): HydratedView {
  const hit = hydrate(result.segment_id);
  if (hit === null) {
    return {
      content: "",
      defined_symbols: result.defined_symbols ?? [],
      breadcrumb: result.breadcrumb,
    };
  }
  return {
    content: hit.content,
    defined_symbols: hit.defined_symbols,
    breadcrumb: hit.breadcrumb ?? result.breadcrumb,
  };
}

function resultMatchesAnchor(
  result: SearchResultJson,
  anchor: Anchor,
  hydrate: HydrateFn,
): boolean {
  if ((result.file_path ?? "") !== anchor.file) {
    return false;
  }
  const needsContent =
    anchor.line_contains !== undefined && anchor.line_contains.length > 0;
  const leanSymbolsHit =
    anchor.symbol !== undefined &&
    anchor.symbol.length > 0 &&
    ((result.defined_symbols ?? []).includes(anchor.symbol) ||
      breadcrumbContainsSymbol(result.breadcrumb, anchor.symbol));
  const needsHydration =
    needsContent ||
    (anchor.symbol !== undefined &&
      anchor.symbol.length > 0 &&
      !leanSymbolsHit);
  const hydrated: HydratedView | null = needsHydration
    ? hydrateOrEmpty(result, hydrate)
    : null;
  if (anchor.symbol !== undefined && anchor.symbol.length > 0) {
    const symbol = anchor.symbol;
    const defined = hydrated?.defined_symbols ?? result.defined_symbols ?? [];
    const content = hydrated?.content ?? "";
    const breadcrumb = hydrated?.breadcrumb ?? result.breadcrumb;
    const matched =
      defined.includes(symbol) ||
      breadcrumbContainsSymbol(breadcrumb, symbol) ||
      (content.length > 0 && contentHasSymbolDefinition(content, symbol));
    if (!matched) {
      return false;
    }
  }
  if (anchor.line_contains !== undefined && anchor.line_contains.length > 0) {
    const content = hydrated?.content ?? "";
    if (!content.includes(anchor.line_contains)) {
      return false;
    }
  }
  return true;
}

function collectSegmentIds(results: SearchResultJson[], k: number): string[] {
  const ids: string[] = [];
  for (const row of results) {
    if (ids.length >= k) {
      break;
    }
    if (typeof row.segment_id === "string" && row.segment_id.length > 0) {
      ids.push(row.segment_id);
    }
  }
  return ids;
}

function scoreAnchorRow(
  topK: SearchResultJson[],
  anchors: Anchor[],
  hydrate: HydrateFn,
): { matched_indices: number[]; hit_count: number; recall: number } {
  if (anchors.length === 0) {
    return { matched_indices: [], hit_count: 0, recall: 0 };
  }
  const matched: number[] = [];
  for (let i = 0; i < anchors.length; i += 1) {
    const anchor = anchors[i];
    if (topK.some((r) => resultMatchesAnchor(r, anchor, hydrate))) {
      matched.push(i);
    }
  }
  return {
    matched_indices: matched,
    hit_count: matched.length,
    recall: matched.length / anchors.length,
  };
}

function scoreSegmentIdRow(
  topKIds: string[],
  gold: string[],
): { matched_indices: number[]; hit_count: number; recall: number } {
  if (gold.length === 0) {
    return { matched_indices: [], hit_count: 0, recall: 0 };
  }
  // Lean rows carry a 12-char display prefix; legacy corpus rows list the full
  // 16-char segment id. Compare on a common prefix length so the fallback
  // still produces a meaningful signal when anchors are absent.
  const prefixLen = 12;
  const retrievedSet = new Set(topKIds.map((id) => id.slice(0, prefixLen)));
  const matched: number[] = [];
  for (let i = 0; i < gold.length; i += 1) {
    if (retrievedSet.has(gold[i].slice(0, prefixLen))) {
      matched.push(i);
    }
  }
  return {
    matched_indices: matched,
    hit_count: matched.length,
    recall: matched.length / gold.length,
  };
}

interface StatusSnapshot {
  schema_version: number | null;
  vector_rows: number | null;
  embedding_model: string | null;
}

function numberOrNull(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function stringOrNull(value: unknown): string | null {
  return typeof value === "string" && value.length > 0 ? value : null;
}

function readStatus(repoDir: string, binary: string): StatusSnapshot {
  // `status` remains a maintenance command that keeps the `-f json` envelope,
  // so we still parse it with JSON here (the lean grammar is scoped to core
  // commands only — design §2.1). `schema_version` and `embedding_model` are
  // surfaced by the status command itself (status variant surfacing); when the
  // running binary predates that surfacing the fields read as null and the
  // preflight fails closed.
  try {
    const raw = execFileSync(binary, ["status", repoDir, "-f", "json"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    });
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    return {
      schema_version: numberOrNull(parsed.schema_version),
      vector_rows: numberOrNull(parsed.vector_rows),
      embedding_model: stringOrNull(parsed.embedding_model),
    };
  } catch {
    return { schema_version: null, vector_rows: null, embedding_model: null };
  }
}

function computeCorpusIdentity(
  rawCorpus: string,
  rowCount: number,
): CorpusIdentity {
  const sha256 = createHash("sha256").update(rawCorpus, "utf8").digest("hex");
  return { size: rowCount, sha256 };
}

function readBaseline(baselinePath: string): RecallBaseline | null {
  if (!existsSync(baselinePath)) {
    return null;
  }
  const parsed = JSON.parse(readFileSync(baselinePath, "utf8")) as Partial<
    RecallBaseline & { corpus?: Partial<CorpusIdentity> }
  >;
  // A structurally invalid baseline (e.g. the legacy schema-v11 prose form that
  // predates the gate) is treated as absent so the gate reports a clear
  // "capture a baseline" error rather than throwing on a missing field.
  if (
    typeof parsed.schema_version !== "number" ||
    typeof parsed.model_id !== "string" ||
    typeof parsed.recall_at_10 !== "number" ||
    typeof parsed.recall_at_20 !== "number" ||
    typeof parsed.corpus?.sha256 !== "string" ||
    typeof parsed.corpus?.size !== "number"
  ) {
    return null;
  }
  return {
    captured_at: parsed.captured_at,
    schema_version: parsed.schema_version,
    model_id: parsed.model_id,
    max_tokens: numberOrNull(parsed.max_tokens),
    corpus: { size: parsed.corpus.size, sha256: parsed.corpus.sha256 },
    recall_at_10: parsed.recall_at_10,
    recall_at_20: parsed.recall_at_20,
  };
}

function reportRecall(reports: RecallReport[], k: number): number | null {
  const report = reports.find((r) => r.k === k);
  return report ? report.recall : null;
}

function formatRecall(value: number): string {
  return `${(value * 100).toFixed(2)}%`;
}

function rowMatchMode(row: CorpusRow): MatchMode | null {
  if (row.expected_anchors && row.expected_anchors.length > 0) {
    return "anchor";
  }
  if (row.expected_segment_ids && row.expected_segment_ids.length > 0) {
    return "segment_id";
  }
  return null;
}

interface ScoredHarness {
  reports: RecallReport[];
  modeCounts: Record<MatchMode | "none", number>;
  degradedQueries: Array<{ query: string; markers: string[] }>;
}

function runHarness(
  binary: string,
  targetRepo: string,
  corpus: CorpusRow[],
): ScoredHarness {
  const hydrate = makeHydrator(binary, targetRepo);

  const modeCounts: Record<MatchMode | "none", number> = {
    anchor: 0,
    segment_id: 0,
    none: 0,
  };
  for (const row of corpus) {
    const mode = rowMatchMode(row);
    if (mode === null) {
      modeCounts.none += 1;
    } else {
      modeCounts[mode] += 1;
    }
  }

  // Pre-fetch top-MAX_K once per query, then slice per k. Keep the raw result objects so
  // anchor matching can inspect content / defined_symbols / breadcrumb. Capture each
  // query's stderr so a degraded / FTS-only response fails the run instead of scoring.
  const perQueryTopK = new Map<string, SearchResultJson[]>();
  const degradedQueries: Array<{ query: string; markers: string[] }> = [];
  for (const row of corpus) {
    const { rows, stderr } = runSearch(binary, row.query, targetRepo, MAX_K);
    perQueryTopK.set(row.query, rows.slice(0, MAX_K));
    const markers = detectDegradedStderr(stderr);
    if (markers.length > 0) {
      degradedQueries.push({ query: row.query, markers });
    }
  }

  const reports: RecallReport[] = [];
  for (const k of K_VALUES) {
    const perQuery: PerQueryResult[] = [];
    let scoredQueries = 0;
    let recallSum = 0;

    for (const row of corpus) {
      const topMax = perQueryTopK.get(row.query) ?? [];
      const topK = topMax.slice(0, k);
      const summarizedRetrieved = topK.map((r) => ({
        segment_id: r.segment_id,
        file_path: r.file_path,
        breadcrumb: r.breadcrumb,
      }));
      const mode = rowMatchMode(row);

      if (mode === null) {
        perQuery.push({
          query: row.query,
          status: "skipped_no_gold",
          match_mode: null,
          retrieved_top_k: summarizedRetrieved,
          gold_size: 0,
          matched_indices: [],
          hit_count: 0,
          recall: 0,
        });
        continue;
      }

      let score: {
        matched_indices: number[];
        hit_count: number;
        recall: number;
      };
      let goldSize: number;
      if (mode === "anchor") {
        const anchors = row.expected_anchors ?? [];
        goldSize = anchors.length;
        score = scoreAnchorRow(topK, anchors, hydrate);
      } else {
        const gold = row.expected_segment_ids ?? [];
        goldSize = gold.length;
        score = scoreSegmentIdRow(collectSegmentIds(topK, k), gold);
      }

      recallSum += score.recall;
      scoredQueries += 1;
      perQuery.push({
        query: row.query,
        status: "scored",
        match_mode: mode,
        retrieved_top_k: summarizedRetrieved,
        gold_size: goldSize,
        matched_indices: score.matched_indices,
        hit_count: score.hit_count,
        recall: score.recall,
      });
    }

    const recall = scoredQueries === 0 ? 0 : recallSum / scoredQueries;
    reports.push({
      k,
      recall,
      scored_queries: scoredQueries,
      total_queries: corpus.length,
      per_query: perQuery,
    });
  }

  return { reports, modeCounts, degradedQueries };
}

function emptyModeCounts(): Record<MatchMode | "none", number> {
  return { anchor: 0, segment_id: 0, none: 0 };
}

function writeResults(output: HarnessOutput): void {
  writeFileSync(RESULTS_PATH, `${JSON.stringify(output, null, 2)}\n`);
}

function printSummary(output: HarnessOutput): void {
  console.log(
    `1up recall@k harness: schema=v${output.schema_version ?? "?"} model=${output.embedding_model ?? "?"} corpus=${output.corpus_size} (anchor=${output.corpus_match_mode_counts.anchor} segment_id=${output.corpus_match_mode_counts.segment_id} none=${output.corpus_match_mode_counts.none}) target=${output.target_repo}`,
  );
  for (const report of output.reports) {
    console.log(
      `  recall@${report.k} = ${formatRecall(report.recall)}  (${report.scored_queries}/${report.total_queries} scored)`,
    );
  }
  console.log(`Wrote ${RESULTS_PATH}`);
}

function main(): void {
  const binary = resolveBinary();
  const targetRepo = resolveTargetRepo();
  const rawCorpus = readFileSync(CORPUS_PATH, "utf8");
  const corpus = readCorpus();
  const corpus_identity = computeCorpusIdentity(rawCorpus, corpus.length);
  const status = readStatus(targetRepo, binary);
  const expectedVariant = resolveExpectedVariant(
    process.env.ONEUP_MODEL_VARIANT,
  );
  const tolerance = resolveTolerance(process.env.ONEUP_RECALL_TOLERANCE);
  const captureBaseline = process.env.RECALL_CAPTURE_BASELINE === "1";
  // A/B mode compares two variant legs (fp32 vs int8) for parity, so the
  // model_id is expected to differ between baseline and candidate.
  const abMode = process.env.ONEUP_RECALL_AB === "1";
  // The A/B recipe points leg 2 at leg 1's captured baseline via this override
  // so the pinned recall-baseline.json is never touched by A/B runs.
  const baselinePath =
    process.env.ONEUP_RECALL_BASELINE_PATH !== undefined &&
    process.env.ONEUP_RECALL_BASELINE_PATH.length > 0
      ? resolve(process.cwd(), process.env.ONEUP_RECALL_BASELINE_PATH)
      : BASELINE_PATH;
  const maxTokens = numberOrNull(
    process.env.ONEUP_RECALL_MAX_TOKENS !== undefined
      ? Number(process.env.ONEUP_RECALL_MAX_TOKENS)
      : undefined,
  );
  const capturedAt = new Date().toISOString();

  const preflight = evaluateStatusPreflight(status, expectedVariant);

  const baseOutput: HarnessOutput = {
    schema_version: status.schema_version,
    vector_rows: status.vector_rows,
    embedding_model: status.embedding_model,
    model_id: status.embedding_model,
    max_tokens: maxTokens,
    target_repo: targetRepo,
    binary,
    captured_at: capturedAt,
    corpus_size: corpus.length,
    corpus: corpus_identity,
    corpus_match_mode_counts: emptyModeCounts(),
    recall_at_10: null,
    recall_at_20: null,
    reports: [],
    gates: {
      expected_variant: expectedVariant,
      tolerance,
      preflight,
      degraded_stderr_queries: [],
      recall: null,
    },
    delta_vs_baseline: null,
  };

  // Fail-closed before scoring: a vectorless / schema-less / wrong-variant index
  // makes recall numbers meaningless, so never score it.
  if (!preflight.ok) {
    writeResults(baseOutput);
    printSummary(baseOutput);
    console.error(
      `FAIL: recall preflight failed — ${preflight.failures.join("; ")}`,
    );
    process.exitCode = 1;
    return;
  }

  const scored = runHarness(binary, targetRepo, corpus);
  const recallAt10 = reportRecall(scored.reports, 10);
  const recallAt20 = reportRecall(scored.reports, 20);

  const output: HarnessOutput = {
    ...baseOutput,
    corpus_match_mode_counts: scored.modeCounts,
    recall_at_10: recallAt10,
    recall_at_20: recallAt20,
    reports: scored.reports,
    gates: {
      ...baseOutput.gates,
      degraded_stderr_queries: scored.degradedQueries,
    },
  };

  // A degraded / FTS-only response on any query means the semantic path wasn't
  // exercised for that query; fail the run and skip the (meaningless) compare.
  if (scored.degradedQueries.length > 0) {
    writeResults(output);
    printSummary(output);
    const summary = scored.degradedQueries
      .map((d) => `"${d.query}" [${d.markers.join(", ")}]`)
      .join("; ");
    console.error(`FAIL: degraded search responses detected — ${summary}`);
    process.exitCode = 1;
    return;
  }

  const candidate: RecallCandidate = {
    schema_version: status.schema_version,
    model_id: status.embedding_model,
    max_tokens: maxTokens,
    corpus: corpus_identity,
    recall_at_10: recallAt10 ?? 0,
    recall_at_20: recallAt20 ?? 0,
  };

  if (captureBaseline) {
    const baseline: RecallBaseline = {
      captured_at: capturedAt,
      schema_version: candidate.schema_version ?? 0,
      model_id: candidate.model_id ?? "",
      max_tokens: candidate.max_tokens,
      corpus: candidate.corpus,
      recall_at_10: candidate.recall_at_10,
      recall_at_20: candidate.recall_at_20,
    };
    writeFileSync(baselinePath, `${JSON.stringify(baseline, null, 2)}\n`);
    writeResults(output);
    printSummary(output);
    console.log(`Captured recall baseline -> ${baselinePath}`);
    return;
  }

  const baseline = readBaseline(baselinePath);
  try {
    const comparison = compareRecall(baseline, candidate, tolerance, {
      allowModelIdMismatch: abMode,
    });
    output.delta_vs_baseline = comparison.deltas;
    output.gates.recall = {
      verdict: comparison.verdict,
      regressions: comparison.regressions,
    };
    writeResults(output);
    printSummary(output);
    console.log(
      `  delta vs baseline: @10 ${comparison.deltas.recall_at_10.toFixed(4)}  @20 ${comparison.deltas.recall_at_20.toFixed(4)}  (tolerance ${tolerance})`,
    );
    if (comparison.verdict === "fail") {
      const detail = comparison.regressions
        .map(
          (r) =>
            `recall@${r.k} ${formatRecall(r.candidate)} vs baseline ${formatRecall(r.baseline)} (delta ${r.delta.toFixed(4)})`,
        )
        .join("; ");
      console.error(
        `FAIL: recall regression beyond tolerance ${tolerance} — ${detail}`,
      );
      process.exitCode = 1;
    }
  } catch (error) {
    const message =
      error instanceof RecallCompareError
        ? error.message
        : error instanceof Error
          ? error.message
          : String(error);
    output.gates.recall = { verdict: "error", error: message };
    writeResults(output);
    printSummary(output);
    console.error(`FAIL: recall gate error — ${message}`);
    process.exitCode = 1;
  }
}

main();
