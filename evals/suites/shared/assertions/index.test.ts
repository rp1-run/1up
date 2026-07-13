import { describe, expect, test } from "bun:test";

import {
  assert1upImpactUsed,
  assert1upUsed,
  assertExpectedFiles,
  assertImpactTrustInterpreted,
  assertNoFallbackTools,
  assertReadAfterSearch,
  assertReadinessWorkflowUsed,
  assertStructuredOneupMcpResponses,
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

type TestToolCall = ReturnType<typeof toolCall> & { output?: unknown };

function makeContext(toolCalls: TestToolCall[] = []) {
  return {
    providerResponse: {
      metadata: {
        toolCalls,
      },
    },
  };
}

function makeCodexContext(items: readonly object[]) {
  return {
    providerResponse: {
      raw: JSON.stringify({ items }),
    },
  };
}

function makeVariantContext(
  label: "1up" | "baseline",
  items: readonly object[] = [],
) {
  return {
    prompt: { label },
    provider: { label: `${label}-agent` },
    providerResponse: {
      raw: JSON.stringify({ items }),
    },
  };
}

describe("assert1upUsed", () => {
  test("does not require 1up workflow calls from the baseline variant", () => {
    const result = assert1upUsed("", makeVariantContext("baseline"));

    expect(result.pass).toBe(true);
    expect(result.reason).toContain("baseline variant");
  });

  test("still fails closed when the 1up variant omits search", () => {
    const result = assert1upUsed("", makeVariantContext("1up"));

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("oneup_search");
  });

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

describe("assertReadinessWorkflowUsed", () => {
  test("preserves ordered Codex MCP calls from raw trajectory items", () => {
    const result = assertReadinessWorkflowUsed(
      "",
      makeCodexContext([
        {
          id: "mcp-1",
          type: "mcp_tool_call",
          server: "oneup",
          tool: "oneup_status",
          arguments: {},
          status: "completed",
        },
        {
          id: "mcp-2",
          type: "mcp_tool_call",
          server: "oneup",
          tool: "oneup_search",
          arguments: { query: "daemon" },
          status: "completed",
        },
      ]),
    );

    expect(result.pass).toBe(true);
  });

  test("passes when status happens before retained discovery", () => {
    const result = assertReadinessWorkflowUsed(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_status", {}),
        toolCall("mcp__oneup__oneup_search", { query: "daemon" }),
      ]),
    );

    expect(result.pass).toBe(true);
    expect(result.score).toBe(1);
  });

  test("passes when start happens after status", () => {
    const result = assertReadinessWorkflowUsed(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_status", {}),
        toolCall("mcp__oneup__oneup_start", { mode: "index_if_needed" }),
        toolCall("mcp__oneup__oneup_search", { query: "daemon" }),
      ]),
    );

    expect(result.pass).toBe(true);
    expect(result.reason).toContain("oneup_start");
  });

  test("fails when status is skipped", () => {
    const result = assertReadinessWorkflowUsed(
      "",
      makeContext([toolCall("mcp__oneup__oneup_search", { query: "daemon" })]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("oneup_status");
  });

  test("fails when discovery happens before status", () => {
    const result = assertReadinessWorkflowUsed(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_search", { query: "daemon" }),
        toolCall("mcp__oneup__oneup_status", {}),
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("after discovery");
  });

  test("fails when start happens before status", () => {
    const result = assertReadinessWorkflowUsed(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_start", { mode: "index_if_needed" }),
        toolCall("mcp__oneup__oneup_status", {}),
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("oneup_start");
  });

  test("fails when start remediation happens after discovery", () => {
    const result = assertReadinessWorkflowUsed(
      "",
      makeContext([
        toolCall("mcp__oneup__oneup_status", {}),
        toolCall("mcp__oneup__oneup_search", { query: "daemon" }),
        toolCall("mcp__oneup__oneup_start", { mode: "index_if_needed" }),
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("oneup_start happened after discovery");
  });
});

describe("assertNoFallbackTools", () => {
  test("detects fallback discovery in Codex command execution items", () => {
    const result = assertNoFallbackTools(
      "",
      makeCodexContext([
        {
          id: "command-1",
          type: "command_execution",
          command: "rg daemon src",
          aggregated_output: "",
          exit_code: 0,
          status: "completed",
        },
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("rg before oneup_search");
  });

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

describe("assertStructuredOneupMcpResponses", () => {
  test("validates structured Codex MCP results from raw trajectory items", () => {
    const result = assertStructuredOneupMcpResponses(
      "",
      makeCodexContext([
        {
          id: "mcp-1",
          type: "mcp_tool_call",
          server: "oneup",
          tool: "oneup_status",
          arguments: {},
          result: {
            content: [],
            structured_content: {
              status: "ready",
              summary: "Ready for search.",
              data: { readiness: "ready" },
              next_actions: [],
            },
          },
          status: "completed",
        },
      ]),
    );

    expect(result.pass).toBe(true);
    expect(result.reason).toContain("ToolEnvelope");
  });

  test("fails when a captured Codex MCP result has null structured content", () => {
    const result = assertStructuredOneupMcpResponses(
      "",
      makeCodexContext([
        {
          id: "mcp-1",
          type: "mcp_tool_call",
          server: "oneup",
          tool: "oneup_status",
          arguments: {},
          result: {
            content: [],
            structured_content: null,
          },
          status: "completed",
        },
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("missing structured_content object");
  });

  test("fails when a captured Codex MCP result omits structured content", () => {
    const result = assertStructuredOneupMcpResponses(
      "",
      makeCodexContext([
        {
          id: "mcp-1",
          type: "mcp_tool_call",
          server: "oneup",
          tool: "oneup_search",
          arguments: { query: "daemon" },
          result: { content: [] },
          status: "completed",
        },
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("missing structured_content object");
  });

  test("reports failed Codex MCP items as errored calls", () => {
    const result = assertValidOneupMcpCalls(
      "",
      makeCodexContext([
        {
          id: "mcp-1",
          type: "mcp_tool_call",
          server: "oneup",
          tool: "oneup_get",
          arguments: { handles: [":bad"] },
          error: { message: "invalid handle" },
          status: "failed",
        },
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("errored MCP call oneup_get");
  });

  test("does not treat a completed Codex MCP item with null error as failed", () => {
    const result = assertValidOneupMcpCalls(
      "",
      makeCodexContext([
        {
          id: "mcp-1",
          type: "mcp_tool_call",
          server: "oneup",
          tool: "oneup_status",
          arguments: {},
          error: null,
          result: {
            structured_content: {
              status: "ready",
              summary: "Ready.",
              data: {},
              next_actions: [],
            },
          },
          status: "completed",
        },
      ]),
    );

    expect(result.pass).toBe(true);
  });

  test("passes when provider metadata omits MCP outputs", () => {
    const result = assertStructuredOneupMcpResponses(
      "",
      makeContext([toolCall("mcp__oneup__oneup_status", {})]),
    );

    expect(result.pass).toBe(true);
    expect(result.reason).toContain("did not include captured MCP outputs");
  });

  test("passes for captured structuredContent envelopes", () => {
    const call = toolCall("mcp__oneup__oneup_status", {});
    const result = assertStructuredOneupMcpResponses(
      "",
      makeContext([
        {
          ...call,
          output: {
            structuredContent: {
              status: "ready",
              summary: "The repository is ready for 1up MCP search.",
              data: { readiness: "ready" },
              next_actions: [
                {
                  tool: "oneup_search",
                  reason: "search indexed repository",
                  arguments: {},
                },
              ],
            },
          },
        },
      ]),
    );

    expect(result.pass).toBe(true);
    expect(result.reason).toContain("ToolEnvelope");
  });

  test("allows next actions that do not require arguments", () => {
    const call = toolCall("mcp__oneup__oneup_status", {});
    const result = assertStructuredOneupMcpResponses(
      "",
      makeContext([
        {
          ...call,
          output: {
            structuredContent: {
              status: "degraded",
              summary: "Search remains available.",
              data: { readiness: "degraded" },
              next_actions: [
                {
                  tool: "oneup_search",
                  reason: "Search the readable index.",
                },
              ],
            },
          },
        },
      ]),
    );

    expect(result.pass).toBe(true);
  });

  test("passes when captured provider output is text-only tool_result content", () => {
    const call = toolCall("mcp__oneup__oneup_status", {});
    const result = assertStructuredOneupMcpResponses(
      "",
      makeContext([
        {
          ...call,
          output: [
            {
              type: "text",
              text: "The repository is ready for 1up MCP search.",
            },
          ],
        },
      ]),
    );

    expect(result.pass).toBe(true);
    expect(result.reason).toContain(
      "did not include captured structured MCP outputs",
    );
  });

  test("fails for captured outputs without structured envelope fields", () => {
    const call = toolCall("mcp__oneup__oneup_search", { query: "daemon" });
    const result = assertStructuredOneupMcpResponses(
      "",
      makeContext([
        {
          ...call,
          output: {
            structuredContent: {
              status: "ok",
              summary: "\u001b[32mok\u001b[0m",
              next_actions: [{ tool: "1up_search", arguments: {} }],
            },
          },
        },
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("ANSI");
    expect(result.reason).toContain("missing data");
    expect(result.reason).toContain("non-canonical tool");
  });

  test("fails for captured outputs with malformed envelope object fields", () => {
    const call = toolCall("mcp__oneup__oneup_status", {});
    const result = assertStructuredOneupMcpResponses(
      "",
      makeContext([
        {
          ...call,
          output: {
            structuredContent: {
              status: "ok",
              summary: "ok",
              data: null,
              next_actions: [{ tool: "oneup_search" }],
            },
          },
        },
      ]),
    );

    expect(result.pass).toBe(false);
    expect(result.reason).toContain("data must be an object");
    expect(result.reason).toContain("without string reason");
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
  test("reports Codex usage when Claude turn metadata is absent", () => {
    const result = reportEfficiency("", {
      providerResponse: {
        raw: JSON.stringify({
          items: [{ type: "agent_message", id: "message-1", text: "done" }],
          usage: {
            input_tokens: 900,
            cached_input_tokens: 600,
            output_tokens: 120,
            reasoning_output_tokens: 80,
          },
        }),
      },
    });

    expect(result.pass).toBe(true);
    expect(result.reason).toContain("duration n/a");
    expect(result.reason).toContain("1 turn");
    expect(result.reason).toContain("cached:600");
    expect(result.reason).toContain("reasoning:80");
    expect(result.score).toBe(0);
    expect(result.namedScores).toBeUndefined();
  });

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
