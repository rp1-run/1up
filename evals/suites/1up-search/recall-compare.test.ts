import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  DEFAULT_RECALL_TOLERANCE,
  type RecallBaseline,
  type RecallCandidate,
  RecallCompareError,
  compareRecall,
  detectDegradedStderr,
  evaluateStatusPreflight,
  resolveExpectedVariant,
  resolveTolerance,
  variantModelIdMatches,
} from "./recall-compare.ts";

const CORPUS = { size: 15, sha256: "abc123" };

function baseline(overrides: Partial<RecallBaseline> = {}): RecallBaseline {
  return {
    captured_at: "2026-07-02T00:00:00.000Z",
    schema_version: 18,
    model_id: "sentence-transformers/all-MiniLM-L6-v2@int8",
    max_tokens: 256,
    corpus: CORPUS,
    recall_at_10: 0.5,
    recall_at_20: 0.6,
    ...overrides,
  };
}

function candidate(overrides: Partial<RecallCandidate> = {}): RecallCandidate {
  return {
    schema_version: 18,
    model_id: "sentence-transformers/all-MiniLM-L6-v2@int8",
    max_tokens: 256,
    corpus: CORPUS,
    recall_at_10: 0.5,
    recall_at_20: 0.6,
    ...overrides,
  };
}

describe("compareRecall", () => {
  test("parity candidate passes with zero deltas", () => {
    const result = compareRecall(baseline(), candidate(), 0.02);
    expect(result.verdict).toBe("pass");
    expect(result.regressions).toHaveLength(0);
    expect(result.deltas.recall_at_10).toBeCloseTo(0);
    expect(result.deltas.recall_at_20).toBeCloseTo(0);
  });

  test("candidate above baseline passes (gate guards regression only)", () => {
    const result = compareRecall(
      baseline(),
      candidate({ recall_at_10: 0.7, recall_at_20: 0.8 }),
      0.02,
    );
    expect(result.verdict).toBe("pass");
    expect(result.regressions).toHaveLength(0);
  });

  test("small drop within tolerance passes", () => {
    const result = compareRecall(
      baseline(),
      candidate({ recall_at_10: 0.49, recall_at_20: 0.59 }),
      0.02,
    );
    expect(result.verdict).toBe("pass");
  });

  test("injected out-of-tolerance regression fails and is identified per k", () => {
    const result = compareRecall(
      baseline(),
      candidate({ recall_at_10: 0.4, recall_at_20: 0.6 }),
      0.02,
    );
    expect(result.verdict).toBe("fail");
    expect(result.regressions).toHaveLength(1);
    expect(result.regressions[0].k).toBe(10);
    expect(result.regressions[0].delta).toBeCloseTo(-0.1);
  });

  test("missing baseline is an explicit error, not a verdict", () => {
    expect(() => compareRecall(null, candidate(), 0.02)).toThrow(
      RecallCompareError,
    );
    expect(() => compareRecall(undefined, candidate(), 0.02)).toThrow(
      /no recall baseline/i,
    );
  });

  test("corpus mismatch is an explicit error", () => {
    expect(() =>
      compareRecall(
        baseline(),
        candidate({ corpus: { size: 15, sha256: "different" } }),
        0.02,
      ),
    ).toThrow(/corpus\.sha256/);
  });

  test("config (schema/model) mismatch is an explicit error", () => {
    expect(() =>
      compareRecall(baseline(), candidate({ schema_version: 17 }), 0.02),
    ).toThrow(/schema_version/);
    expect(() =>
      compareRecall(
        baseline(),
        candidate({ model_id: "sentence-transformers/all-MiniLM-L6-v2" }),
        0.02,
      ),
    ).toThrow(/model_id/);
  });

  test("null candidate model_id/schema fail closed as config mismatch", () => {
    expect(() =>
      compareRecall(baseline(), candidate({ model_id: null }), 0.02),
    ).toThrow(RecallCompareError);
    expect(() =>
      compareRecall(baseline(), candidate({ schema_version: null }), 0.02),
    ).toThrow(RecallCompareError);
  });

  test("A/B mode (allowModelIdMismatch) compares parity across variants", () => {
    const fp32Baseline = baseline({
      model_id: "sentence-transformers/all-MiniLM-L6-v2",
    });
    // int8 candidate at parity passes even though model_id differs
    expect(
      compareRecall(fp32Baseline, candidate(), 0.02, {
        allowModelIdMismatch: true,
      }).verdict,
    ).toBe("pass");
    // int8 candidate regressing beyond tolerance still fails
    expect(
      compareRecall(fp32Baseline, candidate({ recall_at_20: 0.5 }), 0.02, {
        allowModelIdMismatch: true,
      }).verdict,
    ).toBe("fail");
    // without the flag the model_id difference is a config error
    expect(() => compareRecall(fp32Baseline, candidate(), 0.02)).toThrow(
      /model_id/,
    );
  });

  test("max_tokens only enforced when candidate provides it", () => {
    // null candidate max_tokens: skipped, comparison proceeds
    expect(
      compareRecall(baseline(), candidate({ max_tokens: null }), 0.02).verdict,
    ).toBe("pass");
    // divergent non-null max_tokens: mismatch error
    expect(() =>
      compareRecall(baseline(), candidate({ max_tokens: 128 }), 0.02),
    ).toThrow(/max_tokens/);
  });
});

describe("resolveTolerance", () => {
  test("defaults when unset or blank", () => {
    expect(resolveTolerance(undefined)).toBe(DEFAULT_RECALL_TOLERANCE);
    expect(resolveTolerance("")).toBe(DEFAULT_RECALL_TOLERANCE);
    expect(resolveTolerance("  ")).toBe(DEFAULT_RECALL_TOLERANCE);
  });

  test("parses a valid override", () => {
    expect(resolveTolerance("0.05")).toBe(0.05);
    expect(resolveTolerance("0")).toBe(0);
  });

  test("rejects junk", () => {
    expect(() => resolveTolerance("abc")).toThrow(RecallCompareError);
    expect(() => resolveTolerance("-0.1")).toThrow(RecallCompareError);
  });
});

describe("resolveExpectedVariant", () => {
  test("defaults to int8", () => {
    expect(resolveExpectedVariant(undefined)).toBe("int8");
    expect(resolveExpectedVariant("")).toBe("int8");
  });

  test("accepts int8/fp32 case-insensitively", () => {
    expect(resolveExpectedVariant("int8")).toBe("int8");
    expect(resolveExpectedVariant("FP32")).toBe("fp32");
  });

  test("rejects junk", () => {
    expect(() => resolveExpectedVariant("int4")).toThrow(RecallCompareError);
  });
});

describe("variantModelIdMatches", () => {
  test("int8 requires the @int8 suffix", () => {
    expect(
      variantModelIdMatches(
        "int8",
        "sentence-transformers/all-MiniLM-L6-v2@int8",
      ),
    ).toBe(true);
    expect(
      variantModelIdMatches("int8", "sentence-transformers/all-MiniLM-L6-v2"),
    ).toBe(false);
  });

  test("fp32 rejects the @int8 suffix", () => {
    expect(
      variantModelIdMatches("fp32", "sentence-transformers/all-MiniLM-L6-v2"),
    ).toBe(true);
    expect(
      variantModelIdMatches(
        "fp32",
        "sentence-transformers/all-MiniLM-L6-v2@int8",
      ),
    ).toBe(false);
  });
});

describe("evaluateStatusPreflight", () => {
  const okInput = {
    vector_rows: 1234,
    schema_version: 18,
    embedding_model: "sentence-transformers/all-MiniLM-L6-v2@int8",
  };

  test("passes a healthy int8 index", () => {
    expect(evaluateStatusPreflight(okInput, "int8").ok).toBe(true);
  });

  test("fails when vector_rows == 0", () => {
    const result = evaluateStatusPreflight(
      { ...okInput, vector_rows: 0 },
      "int8",
    );
    expect(result.ok).toBe(false);
    expect(result.failures.join(" ")).toMatch(/vector_rows/);
  });

  test("fails when schema_version is null", () => {
    const result = evaluateStatusPreflight(
      { ...okInput, schema_version: null },
      "int8",
    );
    expect(result.ok).toBe(false);
    expect(result.failures.join(" ")).toMatch(/schema_version/);
  });

  test("fails when embedding_model is missing", () => {
    const result = evaluateStatusPreflight(
      { ...okInput, embedding_model: null },
      "int8",
    );
    expect(result.ok).toBe(false);
    expect(result.failures.join(" ")).toMatch(/embedding_model missing/);
  });

  test("fails when embedding_model does not match the expected variant", () => {
    const result = evaluateStatusPreflight(
      { ...okInput, embedding_model: "sentence-transformers/all-MiniLM-L6-v2" },
      "int8",
    );
    expect(result.ok).toBe(false);
    expect(result.failures.join(" ")).toMatch(/does not match expected/);
  });

  test("accumulates every independent failure", () => {
    const result = evaluateStatusPreflight(
      { vector_rows: 0, schema_version: null, embedding_model: null },
      "int8",
    );
    expect(result.failures).toHaveLength(3);
  });
});

describe("detectDegradedStderr", () => {
  test("flags the vectorless FTS-only reason", () => {
    const stderr =
      "index contains no embeddings for this context; semantic ranking disabled (FTS-only)";
    const markers = detectDegradedStderr(stderr);
    expect(markers.length).toBeGreaterThan(0);
    expect(markers).toContain("semantic ranking disabled");
  });

  test("flags the stale-rebuild reason", () => {
    expect(
      detectDegradedStderr("index is rebuilding; results may be stale"),
    ).toContain("results may be stale");
  });

  test("clean stderr yields no markers", () => {
    expect(
      detectDegradedStderr("indexed 1200 segments\nsearch complete"),
    ).toEqual([]);
  });
});

// Drift guard over the committed recall fixtures (T6 / REQ-005). The pinned
// baseline and the pinned results must always describe the SAME recall epoch:
// the same schema version, the same audited corpus (size + sha256), and the same
// model. If they diverge — e.g. one is recaptured on schema 19 while the other
// is left on schema 18 — the runtime gate would raise a config-mismatch
// RecallCompareError instead of a meaningful verdict (the "incompatible-schema
// recall comparison" failure mode). This guard proves that parity structurally,
// using the production comparator, without pinning a literal schema epoch (the
// value moves 18 -> 19 at the manual, credentialed `RECALL_CAPTURE_BASELINE=1`
// recapture; see recall-audit.md). The corpus size is pinned to the live
// `recall-corpus.jsonl` so a row added/removed without a recapture is caught.
describe("committed recall fixtures (schema parity + corpus identity)", () => {
  const here = dirname(fileURLToPath(import.meta.url));
  const readJson = (name: string): Record<string, unknown> =>
    JSON.parse(readFileSync(join(here, name), "utf8"));

  const baseline = readJson(
    "recall-baseline.json",
  ) as unknown as RecallBaseline;
  const results = readJson("recall-results.json");
  const liveCorpusRaw = readFileSync(join(here, "recall-corpus.jsonl"), "utf8");
  const liveCorpusRows = liveCorpusRaw
    .split("\n")
    .filter((line) => line.trim().length > 0 && !line.trim().startsWith("//"));

  const resultsCorpus = results.corpus as { size: number; sha256: string };

  test("baseline and results share one schema version (parity)", () => {
    expect(typeof baseline.schema_version).toBe("number");
    expect(baseline.schema_version).toBe(results.schema_version as number);
  });

  test("baseline and results share corpus identity (size + sha256)", () => {
    expect(baseline.corpus.size).toBe(resultsCorpus.size);
    expect(baseline.corpus.sha256).toBe(resultsCorpus.sha256);
  });

  test("baseline and results name the same model", () => {
    expect(baseline.model_id).toBe(results.model_id as string);
  });

  test("baseline corpus size matches the live audited corpus row count", () => {
    expect(baseline.corpus.size).toBe(liveCorpusRows.length);
  });

  test("baseline and results are comparable (no config mismatch)", () => {
    // A candidate built from the committed results must compare cleanly against
    // the committed baseline: same schema, corpus, and model => a real verdict,
    // never a RecallCompareError. This is the schema-parity + corpus-identity
    // contract exercised through the exact code path the gate uses.
    const candidate: RecallCandidate = {
      schema_version: results.schema_version as number,
      model_id: results.model_id as string,
      max_tokens: (results.max_tokens as number | null) ?? null,
      corpus: { size: resultsCorpus.size, sha256: resultsCorpus.sha256 },
      recall_at_10: results.recall_at_10 as number,
      recall_at_20: results.recall_at_20 as number,
    };
    const comparison = compareRecall(
      baseline,
      candidate,
      DEFAULT_RECALL_TOLERANCE,
    );
    expect(["pass", "fail"]).toContain(comparison.verdict);
  });
});
