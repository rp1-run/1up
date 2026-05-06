import { describe, expect, test } from "bun:test";

import {
  assert1upImpactUsed,
  assert1upUsed,
  assertExpectedFiles,
  assertImpactTrustInterpreted,
  assertNoFallbackTools,
  assertReadAfterSearch,
  assertSymbolVerificationUsed,
  assertValidOneupMcpCalls,
  reportEfficiency,
} from "./index.ts";

let toolId = 0;

function toolCall(name: string, input: unknown = {}, is_error = false) {
  toolId += 1;
  return { id: `tool-${toolId}`, name, input, is_error };
}

function bash(command: string) {
  return toolCall("Bash", { command });
}

function makeContext(toolCalls: Array<ReturnType<typeof toolCall>> = []) {
  return {
    providerResponse: {
      metadata: {
        toolCalls,
      },
    },
  };
}

describe("assert1upUsed", () => {
  test("passes when canonical MCP search is present", () => {
    const result = assert1upUsed(
      "",
      makeContext([toolCall("mcp__oneup__oneup_search", { query: "daemon" })]),
    );

    expect(result.pass).toBe(true);
    expect(result.score).toBe(1);
  });

  test("fails when only shell CLI 1up is present", () => {
    const result = assert1upUsed(
      "",
      makeContext([bash('1up search "daemon"')]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("oneup_search");
  });
});

describe("assert1upImpactUsed", () => {
  test("passes when canonical MCP impact is present", () => {
    const result = assert1upImpactUsed(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_impact", { symbol: "FTSManager" }),
      ]),
    );

    expect(result.pass).toBe(true);
    expect(result.score).toBe(1);
  });

  test("fails when 1up impact is not present", () => {
    const result = assert1upImpactUsed(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_search", { query: "FTSManager" }),
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("oneup_impact");
  });
});

describe("assertNoFallbackTools", () => {
  test("passes when fallback search tools are absent", () => {
    const result = assertNoFallbackTools(
      "",
      makeContext([toolCall("mcp__oneup__oneup_symbol", { name: "Pipeline" })]),
    );

    expect(result.pass).toBe(true);
    expect(result.score).toBe(1);
  });

  test("allows exact literal shell verification after MCP search", () => {
    const result = assertNoFallbackTools(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_search", { query: "daemon worker" }),
        bash('rg -n "Worker" src/daemon/worker.rs'),
      ]),
    );

    expect(result.pass).toBe(true);
  });

  test("allows exact literal Grep verification after MCP search", () => {
    const result = assertNoFallbackTools(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_search", { query: "daemon worker" }),
        toolCall("Grep", {
          pattern: "Worker",
          path: "src/daemon/worker.rs",
        }),
      ]),
    );

    expect(result.pass).toBe(true);
  });

  test("fails broad shell rg discovery after MCP search", () => {
    const result = assertNoFallbackTools(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_search", { query: "daemon" }),
        bash("rg daemon src"),
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("rg");
    expect(result.reason).toContain("exact literal file verification");
  });

  test("fails broad Grep discovery after MCP search", () => {
    const result = assertNoFallbackTools(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_search", { query: "daemon" }),
        toolCall("Grep", { pattern: "daemon", path: "src" }),
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("Grep");
    expect(result.reason).toContain("exact literal file verification");
  });

  test("fails when fallback search tools are used before MCP discovery", () => {
    const result = assertNoFallbackTools(
      "",
      makeContext([
        bash("rg daemon src"),
        bash("grep -R watcher src"),
        toolCall("mcp__oneup__oneup_search", { query: "daemon" }),
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("rg");
    expect(result.reason).toContain("grep");
  });

  test("fails when find is used for discovery even after search", () => {
    const result = assertNoFallbackTools(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_search", { query: "daemon" }),
        bash("find src -name '*worker*'"),
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("find");
  });

  test("fails when the direct Find tool is used for discovery", () => {
    const result = assertNoFallbackTools(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_search", { query: "daemon" }),
        toolCall("Find", { path: "src", pattern: "*worker*" }),
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("Find");
  });
});

describe("assertReadAfterSearch", () => {
  test("passes when oneup_get hydrates a handle after search", () => {
    const result = assertReadAfterSearch(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_search", { query: "daemon" }),
        toolCall("mcp__oneup__oneup_get", { handles: [":abc123def456"] }),
      ]),
    );

    expect(result.pass).toBe(true);
    expect(result.score).toBe(1);
  });

  test("passes when oneup_context hydrates a precise location after search", () => {
    const result = assertReadAfterSearch(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_search", { query: "daemon worker" }),
        toolCall("mcp__oneup__oneup_context", {
          locations: [{ path: "src/daemon/worker.rs", line: 42 }],
        }),
      ]),
    );

    expect(result.pass).toBe(true);
    expect(result.score).toBe(1);
  });

  test("fails when search is not followed by targeted read", () => {
    const result = assertReadAfterSearch(
      "",
      makeContext([toolCall("mcp__oneup__oneup_search", { query: "daemon" })]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("oneup_get");
    expect(result.reason).toContain("oneup_context");
  });
});

describe("assertSymbolVerificationUsed", () => {
  test("passes when oneup_symbol is present", () => {
    const result = assertSymbolVerificationUsed(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_symbol", {
          name: "FTSManager",
          include: "both",
        }),
      ]),
    );

    expect(result.pass).toBe(true);
  });
});

describe("assertImpactTrustInterpreted", () => {
  test("passes when impact output trust buckets are interpreted", () => {
    const result = assertImpactTrustInterpreted(
      "Primary likely-impact files are query.ts and registry.ts. Contextual lower-confidence callers should be verified.",
      makeContext([
        toolCall("mcp__oneup__oneup_impact", { symbol: "FTSManager" }),
      ]),
    );

    expect(result.pass).toBe(true);
  });

  test("gives partial credit when impact is called without trust language", () => {
    const result = assertImpactTrustInterpreted(
      "Files: query.ts and registry.ts.",
      makeContext([
        toolCall("mcp__oneup__oneup_impact", { symbol: "FTSManager" }),
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.score).toBe(0.5);
  });
});

describe("assertValidOneupMcpCalls", () => {
  test("passes for canonical oneup MCP calls", () => {
    const result = assertValidOneupMcpCalls(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_status", {}),
        toolCall("mcp__oneup__oneup_search", { query: "daemon" }),
      ]),
    );

    expect(result.pass).toBe(true);
  });

  test("passes for every canonical oneup MCP tool name form", () => {
    const canonicalTools = [
      "oneup_status",
      "oneup_start",
      "oneup_search",
      "oneup_get",
      "oneup_symbol",
      "oneup_context",
      "oneup_impact",
      "oneup_structural",
    ];
    const calls = canonicalTools.flatMap((tool) => [
      toolCall(tool, {}),
      toolCall(`mcp__oneup__${tool}`, {}),
      toolCall(`mcp.oneup.${tool}`, {}),
      toolCall(`mcp:oneup:${tool}`, {}),
    ]);

    const result = assertValidOneupMcpCalls("", makeContext(calls));

    expect(result.pass).toBe(true);
    expect(result.reason).toContain("canonical oneup_* MCP tool names");
  });

  test("fails on digit-leading aliases and errored calls", () => {
    const result = assertValidOneupMcpCalls(
      "",
      makeContext([
        toolCall("mcp__oneup__1up_search", { query: "daemon" }),
        toolCall("mcp__oneup__oneup_get", { handles: [":bad"] }, true),
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("digit-leading");
    expect(result.reason).toContain("errored MCP call oneup_get");
  });

  test("fails on unknown oneup MCP server tools", () => {
    const result = assertValidOneupMcpCalls(
      "",
      makeContext([toolCall("mcp__oneup__oneup_probe", {})]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("unknown oneup MCP tool");
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
