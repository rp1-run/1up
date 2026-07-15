import { describe, expect, test } from "bun:test";

import { NOT_APPLICABLE_REASON } from "./assertions/index.ts";
import {
  type DiagnosticInput,
  type DiagnosticJson,
  aggregateAxes,
  buildBaseline,
  parseRunsTsv,
  renderTable,
} from "./axes-report.ts";

interface Component {
  score?: number;
  reason?: string;
  metric?: string;
  namedScores?: Record<string, number>;
}

function component(c: Component) {
  return {
    pass: true,
    score: c.score,
    reason: c.reason ?? "ok",
    namedScores: c.namedScores ?? null,
    assertion: { type: "javascript", metric: c.metric },
  };
}

/** Mirrors the shape of a promptfoo `results/latest-luna/search-1.json`. */
function oneupResult(components: Component[]) {
  return {
    provider: { id: "openai:codex-sdk", label: "1up-agent" },
    latencyMs: 162104,
    cost: 0.1483,
    gradingResult: { componentResults: components.map(component) },
  };
}

function baselineResult(components: Component[]) {
  return {
    provider: { id: "openai:codex-sdk", label: "baseline-agent" },
    latencyMs: 132307,
    cost: 0.1375,
    gradingResult: { componentResults: components.map(component) },
  };
}

const EFFICIENCY_NS = {
  latency_ms: 162104,
  tokens: 702234,
  cost_usd: 0.1483,
  calls: 12,
};

/** One case: 1up-agent earns credit on every axis; baseline gets N/A on 1up-only axes. */
function caseDiagnostic(): DiagnosticJson {
  return {
    results: {
      results: [
        oneupResult([
          { metric: "retrieval", score: 0.75 },
          { metric: "adoption", score: 1 },
          { metric: "adoption", score: 0.5 },
          { metric: "reliability", score: 1 },
          { metric: "efficiency", namedScores: EFFICIENCY_NS },
          { metric: "factual", score: 1 },
          { metric: "factual", score: 0.75 },
        ]),
        baselineResult([
          { metric: "retrieval", score: 1 },
          {
            metric: "adoption",
            score: 1,
            reason: `${NOT_APPLICABLE_REASON} 1up workflow assertion is not applicable to the isolated baseline variant`,
          },
          {
            metric: "reliability",
            score: 1,
            reason: `${NOT_APPLICABLE_REASON} 1up workflow assertion is not applicable to the isolated baseline variant`,
          },
          {
            metric: "efficiency",
            namedScores: {
              latency_ms: 132307,
              tokens: 367311,
              cost_usd: 0.1375,
              calls: 4,
            },
          },
          { metric: "factual", score: 0.5 },
        ]),
      ],
    },
  };
}

function inputs(
  diagnostic: DiagnosticJson,
  attemptId = "attempt-1",
): DiagnosticInput[] {
  return [{ attemptId, diagnostic }];
}

describe("aggregateAxes graded means", () => {
  test("averages each graded axis independently per provider", () => {
    const report = aggregateAxes(inputs(caseDiagnostic()));

    expect(report["1up-agent"].retrieval).toBeCloseTo(0.75);
    expect(report["1up-agent"].adoption).toBeCloseTo(0.75); // (1 + 0.5) / 2
    expect(report["1up-agent"].reliability).toBeCloseTo(1);
    expect(report["1up-agent"].factual).toBeCloseTo(0.875); // (1 + 0.75) / 2
  });

  test("baseline gets n/a (null), not credit, for NOT_APPLICABLE-marked axes", () => {
    const report = aggregateAxes(inputs(caseDiagnostic()));

    expect(report["baseline-agent"].adoption).toBeNull();
    expect(report["baseline-agent"].reliability).toBeNull();
    // Axes that genuinely apply to the baseline are still scored.
    expect(report["baseline-agent"].retrieval).toBeCloseTo(1);
    expect(report["baseline-agent"].factual).toBeCloseTo(0.5);
  });
});

describe("cross-axis independence (REQ-002 AC2)", () => {
  test("removing an adoption assertion leaves factual and retrieval unchanged", () => {
    const before = aggregateAxes(inputs(caseDiagnostic()));

    const stripped = caseDiagnostic();
    const oneup = stripped.results?.results?.[0];
    if (oneup?.gradingResult) {
      oneup.gradingResult.componentResults =
        oneup.gradingResult.componentResults?.filter(
          (c) => c.assertion?.metric !== "adoption",
        );
    }
    const after = aggregateAxes(inputs(stripped));

    expect(after["1up-agent"].adoption).toBeNull();
    expect(after["1up-agent"].factual).toBe(before["1up-agent"].factual);
    expect(after["1up-agent"].retrieval).toBe(before["1up-agent"].retrieval);
    expect(after["1up-agent"].reliability).toBe(
      before["1up-agent"].reliability,
    );
  });
});

describe("efficiency extraction", () => {
  test("reads latency/tokens/cost/calls from the efficiency namedScores", () => {
    const report = aggregateAxes(inputs(caseDiagnostic()));

    expect(report["1up-agent"].latency_ms).toBe(162104);
    expect(report["1up-agent"].tokens).toBe(702234);
    expect(report["1up-agent"].cost_usd).toBeCloseTo(0.1483);
    expect(report["1up-agent"].calls).toBe(12);
  });

  test("falls back to result.latencyMs/cost when namedScores are zero/absent", () => {
    const diag: DiagnosticJson = {
      results: {
        results: [
          {
            provider: { label: "1up-agent" },
            latencyMs: 55000,
            cost: 0.2,
            gradingResult: {
              componentResults: [
                component({
                  metric: "efficiency",
                  namedScores: { latency_ms: 0, tokens: 100, calls: 3 },
                }),
              ],
            },
          },
        ],
      },
    };
    const report = aggregateAxes(inputs(diag));

    expect(report["1up-agent"].latency_ms).toBe(55000); // fell back to result.latencyMs
    expect(report["1up-agent"].tokens).toBe(100);
    expect(report["1up-agent"].cost_usd).toBeCloseTo(0.2); // fell back to result.cost
    expect(report["1up-agent"].calls).toBe(3);
  });

  test("means efficiency measures across multiple cases", () => {
    const report = aggregateAxes([
      { attemptId: "a1", diagnostic: caseDiagnostic() },
      { attemptId: "a2", diagnostic: caseDiagnostic() },
    ]);

    expect(report["1up-agent"].tokens).toBe(702234); // identical cases -> same mean
    expect(report["1up-agent"].sourceAttemptIds).toEqual(["a1", "a2"]);
  });
});

describe("parseRunsTsv", () => {
  test("parses v2 8-column rows", () => {
    const tsv =
      "uuid-1\tSearch Stack\taggregate\t-\t267\tpass\tresults/x/search-0.log\tresults/x/search-0.json\n";
    const rows = parseRunsTsv(tsv);

    expect(rows).toHaveLength(1);
    expect(rows[0].attemptId).toBe("uuid-1");
    expect(rows[0].role).toBe("aggregate");
    expect(rows[0].diagnosticPath).toBe("results/x/search-0.json");
  });

  test("tolerates legacy v1 5-column rows as synthetic aggregates", () => {
    const tsv =
      "WordPress Import\t334\tpass\tresults/x/s.log\tresults/x/s.json\n";
    const rows = parseRunsTsv(tsv);

    expect(rows).toHaveLength(1);
    expect(rows[0].label).toBe("WordPress Import");
    expect(rows[0].role).toBe("aggregate");
    expect(rows[0].diagnosticPath).toBe("results/x/s.json");
  });

  test("ignores blank lines", () => {
    expect(parseRunsTsv("\n\n")).toHaveLength(0);
  });
});

describe("buildBaseline", () => {
  test("stamps contract_hash and preserves n/a as null", () => {
    const report = aggregateAxes(inputs(caseDiagnostic()));
    const baseline = buildBaseline(
      report,
      "sha256:deadbeef",
      "2026-07-14T00:00:00Z",
    );

    expect(baseline.contract_hash).toBe("sha256:deadbeef");
    expect(baseline.captured_at).toBe("2026-07-14T00:00:00Z");
    expect(baseline.providers["baseline-agent"].adoption).toBeNull();
    expect(baseline.providers["1up-agent"].adoption).toBeCloseTo(0.75);
    expect(baseline.providers["1up-agent"].source_attempt_ids).toEqual([
      "attempt-1",
    ]);
  });
});

describe("renderTable", () => {
  test("renders n/a for null axes and a column per provider", () => {
    const report = aggregateAxes(inputs(caseDiagnostic()));
    const table = renderTable(report);

    expect(table).toContain("| axis | 1up-agent | baseline-agent |");
    // baseline adoption row shows n/a, 1up-agent shows a number.
    expect(table).toMatch(/\| adoption \| 0\.750 \| n\/a \|/);
  });

  test("handles an empty report", () => {
    expect(renderTable({})).toContain("No results found");
  });
});
