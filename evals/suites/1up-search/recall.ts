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
 * Target repo selection (in priority order):
 *   1. `RECALL_TARGET_REPO` env var (absolute path)
 *   2. Git toplevel of this file (the 1up repo root)
 * Binary selection:
 *   1. `RECALL_ONEUP_BIN` env var
 *   2. `ONEUP_BENCH_BIN` env var (reused from search-bench.ts convention)
 *   3. `1up` on PATH
 */

import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

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

interface HarnessOutput {
  schema_version: number | null;
  target_repo: string;
  binary: string;
  captured_at: string;
  corpus_size: number;
  corpus_match_mode_counts: Record<MatchMode | "none", number>;
  reports: RecallReport[];
}

const __dirname = dirname(fileURLToPath(import.meta.url));
const CORPUS_PATH = join(__dirname, "recall-corpus.jsonl");
const RESULTS_PATH = join(__dirname, "recall-results.json");
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

function runSearch(
  binary: string,
  query: string,
  repoDir: string,
  k: number,
): SearchResultJson[] {
  const rawOutput = execFileSync(
    binary,
    ["search", "-n", String(k), "--path", repoDir, query],
    {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      cwd: repoDir,
    },
  );
  const rows: SearchResultJson[] = [];
  for (const rawLine of rawOutput.split("\n")) {
    const parsed = parseLeanRow(rawLine);
    if (parsed !== null) {
      rows.push(parsed);
    }
  }
  return rows;
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

function readSchemaVersion(repoDir: string, binary: string): number | null {
  // `status` remains a maintenance command that keeps the `-f json` envelope,
  // so we still parse it with JSON here (the lean grammar is scoped to core
  // commands only — design §2.1).
  try {
    const raw = execFileSync(binary, ["status", repoDir, "-f", "json"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    });
    const parsed = JSON.parse(raw) as { schema_version?: number };
    return typeof parsed.schema_version === "number"
      ? parsed.schema_version
      : null;
  } catch {
    return null;
  }
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

function runHarness(): HarnessOutput {
  const binary = resolveBinary();
  const targetRepo = resolveTargetRepo();
  const corpus = readCorpus();
  const schemaVersion = readSchemaVersion(targetRepo, binary);
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
  // anchor matching can inspect content / defined_symbols / breadcrumb.
  const perQueryTopK = new Map<string, SearchResultJson[]>();
  for (const row of corpus) {
    const raw = runSearch(binary, row.query, targetRepo, MAX_K);
    perQueryTopK.set(row.query, raw.slice(0, MAX_K));
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

  return {
    schema_version: schemaVersion,
    target_repo: targetRepo,
    binary,
    captured_at: new Date().toISOString(),
    corpus_size: corpus.length,
    corpus_match_mode_counts: modeCounts,
    reports,
  };
}

const output = runHarness();
writeFileSync(RESULTS_PATH, `${JSON.stringify(output, null, 2)}\n`);

console.log(
  `1up recall@k harness: schema=v${output.schema_version ?? "?"} corpus=${output.corpus_size} (anchor=${output.corpus_match_mode_counts.anchor} segment_id=${output.corpus_match_mode_counts.segment_id} none=${output.corpus_match_mode_counts.none}) target=${output.target_repo}`,
);
for (const report of output.reports) {
  console.log(
    `  recall@${report.k} = ${formatRecall(report.recall)}  (${report.scored_queries}/${report.total_queries} scored)`,
  );
}
console.log(`Wrote ${RESULTS_PATH}`);
