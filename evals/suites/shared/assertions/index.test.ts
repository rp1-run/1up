import { describe, expect, test } from "bun:test";

import {
  assert1upUsed,
  assert1upImpactUsed,
  assertExpectedFiles,
  assertNoFallbackTools,
  reportEfficiency,
} from "./index.ts";

function makeContext(commands: string[] = []) {
  return {
    providerResponse: {
      metadata: {
        toolCalls: commands.map((command, index) => ({
          id: `tool-${index}`,
          name: "Bash",
          input: { command },
        })),
      },
    },
  };
}

describe("assert1upUsed", () => {
  test("passes when a 1up command is present", () => {
    const result = assert1upUsed("", makeContext(['1up search "daemon" -n 5']));

    expect(result.pass).toBe(true);
    expect(result.score).toBe(1);
  });

  test("fails when no 1up command is present", () => {
    const result = assert1upUsed("", makeContext(["rg daemon src"]));

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("rg daemon src");
  });
});

describe("assert1upImpactUsed", () => {
  test("passes when 1up impact is present", () => {
    const result = assert1upImpactUsed(
      "",
      makeContext(["1up impact --from-symbol FTSManager"]),
    );

    expect(result.pass).toBe(true);
    expect(result.score).toBe(1);
  });

  test("fails when 1up impact is not present", () => {
    const result = assert1upImpactUsed(
      "",
      makeContext(['1up search "FTSManager"']),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("did not invoke 1up impact");
  });
});

describe("assertNoFallbackTools", () => {
  test("passes when fallback search tools are absent", () => {
    const result = assertNoFallbackTools(
      "",
      makeContext(["1up symbol Pipeline"]),
    );

    expect(result.pass).toBe(true);
    expect(result.score).toBe(1);
  });

  test("fails when fallback search tools are used", () => {
    const result = assertNoFallbackTools(
      "",
      makeContext(["rg daemon src", "grep -R watcher src"]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("rg");
    expect(result.reason).toContain("grep");
  });
});

describe("reportEfficiency", () => {
  test("prefers token counts from raw provider usage", () => {
    const result = reportEfficiency("", {
      providerResponse: {
        metadata: {
          numTurns: 3,
          durationMs: 42_000,
        },
        cost: 0.12,
        raw: JSON.stringify({
          usage: {
            input_tokens: 1_200,
            output_tokens: 340,
            cache_creation_input_tokens: 560,
          },
        }),
      },
    });

    expect(result.pass).toBe(true);
    expect(result.namedScores).toEqual({
      Speed: 79,
      "Cost Efficiency": 76,
    });
    expect(result.reason).toContain("42s");
    expect(result.reason).toContain("$0.12");
    expect(result.reason).toContain("in:1,200");
    expect(result.reason).toContain("out:340");
    expect(result.reason).toContain("cache_create:560");
  });
});

describe("assertExpectedFiles", () => {
  test("matches expected files by basename", () => {
    const grader = assertExpectedFiles([
      "src/daemon/worker.rs",
      "src/indexer/pipeline.rs",
    ]);

    const result = grader(
      "Updated worker.rs and pipeline.rs to support scoped runs.",
      makeContext(),
    );

    expect(result.pass).toBe(true);
    expect(result.score).toBe(1);
  });

  test("reports missing basenames", () => {
    const grader = assertExpectedFiles([
      "src/daemon/worker.rs",
      "src/indexer/pipeline.rs",
    ]);

    const result = grader("Touched worker.rs only.", makeContext());

    expect(result.pass).toBe(false);
    expect(result.score).toBe(0.5);
    expect(result.reason).toContain("pipeline.rs");
  });
});
