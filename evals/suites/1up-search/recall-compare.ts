/**
 * Pure recall-gate primitives: baseline comparison and semantic-path preflight
 * evaluation. Kept free of any I/O or `1up` invocation so the gate logic is
 * unit-testable with bun and needs no model or index (see recall-compare.test.ts).
 *
 * `recall.ts` is the impure driver: it reads status JSON, runs searches, scores
 * recall, then delegates the pass/fail decisions here.
 */

/** INT8 model-identity suffix; mirrors `MODEL_VARIANT_INT8_SUFFIX` in src/shared/constants.rs. */
export const MODEL_VARIANT_INT8_SUFFIX = "@int8";

/** Absolute recall-delta tolerance per k when `ONEUP_RECALL_TOLERANCE` is unset. */
export const DEFAULT_RECALL_TOLERANCE = 0.02;

export type ModelVariant = "int8" | "fp32";

export interface CorpusIdentity {
  size: number;
  sha256: string;
}

/**
 * Structured baseline the gate compares against. Captured only via
 * `just eval-recall-baseline` (README policy: never regenerated to make the
 * gate pass).
 */
export interface RecallBaseline {
  captured_at?: string;
  schema_version: number;
  model_id: string;
  max_tokens: number | null;
  corpus: CorpusIdentity;
  recall_at_10: number;
  recall_at_20: number;
}

/**
 * Candidate produced by the current harness run. `schema_version`, `model_id`,
 * and `max_tokens` may be null when status does not surface them; a null where
 * the baseline carries a value is treated as a config mismatch (fail-closed).
 */
export interface RecallCandidate {
  schema_version: number | null;
  model_id: string | null;
  max_tokens: number | null;
  corpus: CorpusIdentity;
  recall_at_10: number;
  recall_at_20: number;
}

export interface RecallRegression {
  k: number;
  baseline: number;
  candidate: number;
  delta: number;
}

export interface RecallComparison {
  verdict: "pass" | "fail";
  tolerance: number;
  deltas: { recall_at_10: number; recall_at_20: number };
  regressions: RecallRegression[];
}

/**
 * Thrown for a comparison that cannot be trusted: a missing baseline or a
 * corpus/config mismatch between baseline and candidate. This is deliberately
 * distinct from a regression `fail` verdict — an untrustworthy comparison is an
 * operator error to fix, not a recall regression.
 */
export class RecallCompareError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "RecallCompareError";
  }
}

/** Resolve the absolute per-k tolerance from a raw env value, falling back to the default. */
export function resolveTolerance(raw: string | undefined): number {
  if (raw === undefined || raw.trim().length === 0) {
    return DEFAULT_RECALL_TOLERANCE;
  }
  const parsed = Number(raw);
  if (!Number.isFinite(parsed) || parsed < 0) {
    throw new RecallCompareError(
      `ONEUP_RECALL_TOLERANCE must be a non-negative number, got "${raw}"`,
    );
  }
  return parsed;
}

export interface CompareOptions {
  /**
   * Allow the baseline and candidate to name different model variants. Used by
   * the A/B recipe, which deliberately compares fp32 against int8 recall for
   * parity — a `model_id` difference is expected there, not a config error.
   */
  allowModelIdMismatch?: boolean;
}

function configMismatches(
  baseline: RecallBaseline,
  candidate: RecallCandidate,
  options: CompareOptions,
): string[] {
  const mismatches: string[] = [];
  if (baseline.corpus.sha256 !== candidate.corpus.sha256) {
    mismatches.push(
      `corpus.sha256 (baseline ${baseline.corpus.sha256} != candidate ${candidate.corpus.sha256})`,
    );
  }
  if (baseline.corpus.size !== candidate.corpus.size) {
    mismatches.push(
      `corpus.size (baseline ${baseline.corpus.size} != candidate ${candidate.corpus.size})`,
    );
  }
  if (baseline.schema_version !== candidate.schema_version) {
    mismatches.push(
      `schema_version (baseline ${baseline.schema_version} != candidate ${candidate.schema_version})`,
    );
  }
  if (
    !options.allowModelIdMismatch &&
    baseline.model_id !== candidate.model_id
  ) {
    mismatches.push(
      `model_id (baseline ${baseline.model_id} != candidate ${candidate.model_id})`,
    );
  }
  // max_tokens is not always observable from the candidate run; only enforce it
  // when both sides carry a value.
  if (
    baseline.max_tokens !== null &&
    candidate.max_tokens !== null &&
    baseline.max_tokens !== candidate.max_tokens
  ) {
    mismatches.push(
      `max_tokens (baseline ${baseline.max_tokens} != candidate ${candidate.max_tokens})`,
    );
  }
  return mismatches;
}

/**
 * Compare a candidate recall run against the pinned baseline.
 *
 * - Missing baseline (`null`/`undefined`) -> {@link RecallCompareError}.
 * - Corpus/config mismatch -> {@link RecallCompareError}.
 * - Any k regressing by more than `tolerance` -> `fail` verdict.
 * - Otherwise -> `pass`.
 *
 * A candidate scoring *above* baseline never fails; the gate guards against
 * regression only.
 */
export function compareRecall(
  baseline: RecallBaseline | null | undefined,
  candidate: RecallCandidate,
  tolerance: number,
  options: CompareOptions = {},
): RecallComparison {
  if (baseline === null || baseline === undefined) {
    throw new RecallCompareError(
      "no recall baseline found; capture one with `just eval-recall-baseline` before gating",
    );
  }
  if (tolerance < 0) {
    throw new RecallCompareError(
      `tolerance must be non-negative, got ${tolerance}`,
    );
  }
  const mismatches = configMismatches(baseline, candidate, options);
  if (mismatches.length > 0) {
    throw new RecallCompareError(
      `baseline/candidate config mismatch — comparison is not meaningful: ${mismatches.join("; ")}`,
    );
  }

  const deltaAt10 = candidate.recall_at_10 - baseline.recall_at_10;
  const deltaAt20 = candidate.recall_at_20 - baseline.recall_at_20;
  const regressions: RecallRegression[] = [];
  if (deltaAt10 < -tolerance) {
    regressions.push({
      k: 10,
      baseline: baseline.recall_at_10,
      candidate: candidate.recall_at_10,
      delta: deltaAt10,
    });
  }
  if (deltaAt20 < -tolerance) {
    regressions.push({
      k: 20,
      baseline: baseline.recall_at_20,
      candidate: candidate.recall_at_20,
      delta: deltaAt20,
    });
  }

  return {
    verdict: regressions.length > 0 ? "fail" : "pass",
    tolerance,
    deltas: { recall_at_10: deltaAt10, recall_at_20: deltaAt20 },
    regressions,
  };
}

/** True when `modelId` names the expected variant (INT8 identities carry the `@int8` suffix). */
export function variantModelIdMatches(
  variant: ModelVariant,
  modelId: string,
): boolean {
  const isInt8 = modelId.endsWith(MODEL_VARIANT_INT8_SUFFIX);
  return variant === "int8" ? isInt8 : !isInt8;
}

/** Parse `ONEUP_MODEL_VARIANT` into the expected preflight variant (default INT8). */
export function resolveExpectedVariant(raw: string | undefined): ModelVariant {
  if (raw === undefined || raw.trim().length === 0) {
    return "int8";
  }
  const normalized = raw.trim().toLowerCase();
  if (normalized === "int8" || normalized === "fp32") {
    return normalized;
  }
  throw new RecallCompareError(
    `ONEUP_MODEL_VARIANT must be "int8" or "fp32", got "${raw}"`,
  );
}

export interface StatusPreflightInput {
  vector_rows: number | null | undefined;
  schema_version: number | null | undefined;
  embedding_model: string | null | undefined;
}

export interface PreflightResult {
  ok: boolean;
  failures: string[];
}

/**
 * Assert the semantic path is actually exercised before recall is scored. A run
 * that scores over a vectorless or schema-less index is meaningless, so every
 * failing signal fails the gate closed rather than silently reporting recall.
 */
export function evaluateStatusPreflight(
  input: StatusPreflightInput,
  expectedVariant: ModelVariant,
): PreflightResult {
  const failures: string[] = [];

  if (typeof input.vector_rows !== "number" || input.vector_rows <= 0) {
    failures.push(
      `vector_rows is ${input.vector_rows ?? "null"}; expected > 0 (index has no embeddings — semantic path not exercised)`,
    );
  }

  if (typeof input.schema_version !== "number" || input.schema_version <= 0) {
    failures.push(
      `schema_version is ${input.schema_version ?? "null"}; expected a current positive schema (status did not report an indexed schema)`,
    );
  }

  const model = input.embedding_model;
  if (typeof model !== "string" || model.length === 0) {
    failures.push(
      "embedding_model missing from status (requires status variant surfacing); cannot confirm the serving model",
    );
  } else if (!variantModelIdMatches(expectedVariant, model)) {
    failures.push(
      `embedding_model "${model}" does not match expected variant "${expectedVariant}"`,
    );
  }

  return { ok: failures.length === 0, failures };
}

/**
 * Substrings (case-insensitive) that mark a degraded / FTS-only search response
 * on stderr. Sourced from `NO_INDEXED_EMBEDDINGS_REASON` and
 * `STALE_REBUILD_REASON` in src/shared/constants.rs.
 */
export const DEGRADED_STDERR_MARKERS = [
  "semantic ranking disabled",
  "fts-only",
  "no embeddings for this context",
  "results may be stale",
] as const;

/** Return the degraded markers present in a captured stderr blob (empty when clean). */
export function detectDegradedStderr(stderr: string): string[] {
  const haystack = stderr.toLowerCase();
  return DEGRADED_STDERR_MARKERS.filter((marker) => haystack.includes(marker));
}
