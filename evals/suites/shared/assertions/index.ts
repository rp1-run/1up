/**
 * MCP tool-call based assertions for 1up eval tests.
 * Uses provider metadata to inspect tool calls made by the agent.
 */

import {
  ONEUP_MCP_TOOLS,
  type OneupMcpTool,
  toCanonical,
  toOneupMcpTool,
  usesDigitLeadingOneupAlias,
} from "../tool-names.ts";

interface GradingResult {
  pass: boolean;
  score: number;
  reason: string;
  namedScores?: Record<string, number>;
}

/**
 * Prefix stamped on the reason of any component that does not apply to the
 * variant under test (e.g. 1up-specific assertions scored against the isolated
 * baseline). The per-axis report (axes-report.ts) excludes marked components
 * from axis means and renders them `n/a` instead of granting automatic credit,
 * so the baseline no longer inherits the score of assertions it never exercises
 * (REQ-002).
 */
export const NOT_APPLICABLE_REASON = "not-applicable:";

interface ToolCall {
  readonly id?: string;
  readonly name: string;
  readonly input: unknown;
  readonly output?: unknown;
  readonly is_error?: boolean;
  readonly parentToolUseId?: string | null;
  readonly structuredOutputRequired?: boolean;
}

interface ProviderMetadata {
  readonly toolCalls?: readonly ToolCall[];
  readonly skillCalls?: readonly { name: string }[];
  readonly numTurns?: number;
  readonly durationMs?: number;
}

interface TokenUsage {
  readonly total?: number;
  readonly prompt?: number;
  readonly completion?: number;
  readonly numRequests?: number;
}

interface EvalContext {
  prompt?: string | { label?: string };
  provider?: { label?: string };
  vars?: Record<string, string | number | boolean | object>;
  providerResponse?: {
    metadata?: ProviderMetadata;
    tokenUsage?: TokenUsage;
    cost?: number;
    raw?: unknown;
  };
}

function getToolCalls(context: EvalContext): readonly ToolCall[] {
  const metadataCalls = context.providerResponse?.metadata?.toolCalls;
  if (metadataCalls !== undefined) {
    return metadataCalls;
  }

  const raw = context.providerResponse?.raw;
  let parsed: unknown = raw;
  if (typeof raw === "string") {
    try {
      parsed = JSON.parse(raw) as unknown;
    } catch {
      return [];
    }
  }

  if (!isRecord(parsed) || !Array.isArray(parsed.items)) {
    return [];
  }

  return parsed.items.flatMap((item): ToolCall[] => {
    if (!isRecord(item) || typeof item.type !== "string") {
      return [];
    }

    const id = typeof item.id === "string" ? item.id : undefined;
    if (
      item.type === "mcp_tool_call" &&
      typeof item.server === "string" &&
      typeof item.tool === "string"
    ) {
      return [
        {
          id,
          name: `mcp__${item.server}__${item.tool}`,
          input: item.arguments ?? item.args ?? item.input ?? {},
          output: item.result,
          is_error: item.status === "failed" || item.error != null,
          structuredOutputRequired:
            item.status === "completed" && item.result != null,
        },
      ];
    }

    if (item.type === "command_execution" && typeof item.command === "string") {
      return [
        {
          id,
          name: "Bash",
          input: { command: item.command },
          output: item.aggregated_output,
          is_error:
            item.status === "failed" ||
            (typeof item.exit_code === "number" && item.exit_code !== 0),
        },
      ];
    }

    return [];
  });
}

function getOneupCalls(
  context: EvalContext,
  tool?: OneupMcpTool,
): readonly ToolCall[] {
  return getToolCalls(context).filter((tc) => {
    const oneupTool = toOneupMcpTool(tc.name);
    return (
      oneupTool !== undefined && (tool === undefined || oneupTool === tool)
    );
  });
}

function baselineSkip(context: EvalContext): GradingResult | undefined {
  const promptLabel =
    typeof context.prompt === "object" ? context.prompt.label : undefined;
  const providerLabel = context.provider?.label;
  const baseline =
    promptLabel === "baseline" || providerLabel === "baseline-agent";
  if (!baseline) {
    return undefined;
  }

  const calls = getOneupCalls(context);
  const pass = calls.length === 0;
  return {
    pass,
    score: pass ? 1 : 0,
    reason: pass
      ? `${NOT_APPLICABLE_REASON} 1up workflow assertion is not applicable to the isolated baseline variant`
      : `Baseline variant unexpectedly invoked 1up MCP calls: ${formatToolNames(calls)}`,
  };
}

const FALLBACK_TOOLS = ["rg", "grep", "find"] as const;
const ONEUP_DISCOVERY_TOOLS: readonly OneupMcpTool[] = [
  "oneup_search",
  "oneup_get",
  "oneup_symbol",
  "oneup_context",
  "oneup_impact",
  "oneup_structural",
];
const ONEUP_MCP_TOOL_SET = new Set<string>(ONEUP_MCP_TOOLS);
const ANSI_ESCAPE_PATTERN = new RegExp(
  `${String.fromCharCode(27)}\\[[0-?]*[ -/]*[@-~]`,
);

type FallbackTool = (typeof FALLBACK_TOOLS)[number];

interface ShellToken {
  readonly value: string;
  readonly quoted: boolean;
}

interface FallbackInvocation {
  readonly tool: FallbackTool;
  readonly tokens: readonly ShellToken[];
  readonly raw: string;
}

const SHELL_SEGMENT_BOUNDARIES = new Set(["&&", "||", ";", "|"]);
const FALLBACK_TOOL_NAMES = new Set<string>(FALLBACK_TOOLS);
const OPTION_TAKES_VALUE = new Set([
  "-A",
  "-B",
  "-C",
  "-e",
  "-f",
  "-g",
  "-j",
  "-m",
  "-r",
  "-t",
  "-T",
  "--after-context",
  "--before-context",
  "--colors",
  "--context",
  "--encoding",
  "--engine",
  "--file",
  "--glob",
  "--iglob",
  "--max-count",
  "--regexp",
  "--replace",
  "--sort",
  "--threads",
  "--type",
  "--type-not",
]);
const REGEX_META_PATTERN = /[\\^$.*+?()[\]{}|]/;
const GLOB_META_PATTERN = /[*?[\]{}]/;

function tokenizeShell(command: string): ShellToken[] {
  const tokens: ShellToken[] = [];
  let value = "";
  let quoted = false;
  let quote: "'" | '"' | undefined;

  const push = () => {
    if (value.length > 0) {
      tokens.push({ value, quoted });
    }
    value = "";
    quoted = false;
  };

  for (let i = 0; i < command.length; i += 1) {
    const char = command[i];

    if (quote) {
      if (char === quote) {
        quote = undefined;
        quoted = true;
      } else if (quote === '"' && char === "\\" && i + 1 < command.length) {
        i += 1;
        value += command[i];
      } else {
        value += char;
      }
      continue;
    }

    if (char === "'" || char === '"') {
      quote = char;
      quoted = true;
      continue;
    }

    if (/\s/.test(char)) {
      push();
      continue;
    }

    if (char === ";" || char === "|") {
      push();
      if (char === "|" && command[i + 1] === "|") {
        tokens.push({ value: "||", quoted: false });
        i += 1;
      } else {
        tokens.push({ value: char, quoted: false });
      }
      continue;
    }

    if (char === "&" && command[i + 1] === "&") {
      push();
      tokens.push({ value: "&&", quoted: false });
      i += 1;
      continue;
    }

    value += char;
  }

  push();
  return tokens;
}

function fallbackToolName(value: string): FallbackTool | undefined {
  const name = value.split(/[\\/]/).at(-1) ?? value;
  return FALLBACK_TOOL_NAMES.has(name) ? (name as FallbackTool) : undefined;
}

function fallbackInvocations(command: string): FallbackInvocation[] {
  const tokens = tokenizeShell(command);
  const invocations: FallbackInvocation[] = [];
  let segment: ShellToken[] = [];

  const flush = () => {
    const toolIndex = segment.findIndex((token) =>
      fallbackToolName(token.value),
    );
    if (toolIndex !== -1) {
      const tool = fallbackToolName(segment[toolIndex].value);
      if (tool) {
        const invocationTokens = segment.slice(toolIndex);
        invocations.push({
          tool,
          tokens: invocationTokens,
          raw: invocationTokens.map((token) => token.value).join(" "),
        });
      }
    }
    segment = [];
  };

  for (const token of tokens) {
    if (SHELL_SEGMENT_BOUNDARIES.has(token.value)) {
      flush();
    } else {
      segment.push(token);
    }
  }

  flush();
  return invocations;
}

function optionTakesSeparateValue(option: string): boolean {
  if (option.includes("=")) {
    return false;
  }

  if (OPTION_TAKES_VALUE.has(option)) {
    return true;
  }

  return /^-[ABCegjmrtT]$/.test(option);
}

function isFixedStringOption(option: string): boolean {
  return (
    option === "--fixed-strings" ||
    option === "--fixed-string" ||
    option === "-F" ||
    /^-[A-Za-z]*F[A-Za-z]*$/.test(option)
  );
}

function parseGrepLikeInvocation(invocation: FallbackInvocation):
  | {
      readonly pattern: ShellToken;
      readonly paths: readonly string[];
      readonly fixedString: boolean;
    }
  | undefined {
  let pattern: ShellToken | undefined;
  const paths: string[] = [];
  let fixedString = false;
  let optionsDone = false;

  for (let i = 1; i < invocation.tokens.length; i += 1) {
    const token = invocation.tokens[i];
    const value = token.value;

    if (!optionsDone && value === "--") {
      optionsDone = true;
      continue;
    }

    if (!optionsDone && value.startsWith("-") && value !== "-") {
      fixedString = fixedString || isFixedStringOption(value);

      if (value === "-e" || value === "--regexp") {
        i += 1;
        pattern = invocation.tokens[i];
      } else if (value.startsWith("-e") && value.length > 2) {
        pattern = { value: value.slice(2), quoted: token.quoted };
      } else if (optionTakesSeparateValue(value)) {
        i += 1;
      }

      continue;
    }

    if (!pattern) {
      pattern = token;
    } else {
      paths.push(value);
    }
  }

  return pattern ? { pattern, paths, fixedString } : undefined;
}

function isPreciseFilePath(path: string): boolean {
  return (
    path.length > 0 &&
    path !== "." &&
    !path.endsWith("/") &&
    !GLOB_META_PATTERN.test(path) &&
    /(^|\/)[^/]+\.[A-Za-z0-9][A-Za-z0-9._-]*$/.test(path)
  );
}

function isExactLiteralPattern(
  pattern: ShellToken,
  fixedString: boolean,
): boolean {
  return (
    pattern.value.length > 0 &&
    (fixedString || (pattern.quoted && !REGEX_META_PATTERN.test(pattern.value)))
  );
}

function isAllowedGrepLikeInvocation(invocation: FallbackInvocation): boolean {
  const parsed = parseGrepLikeInvocation(invocation);
  if (!parsed) {
    return false;
  }

  return (
    isExactLiteralPattern(parsed.pattern, parsed.fixedString) &&
    parsed.paths.length > 0 &&
    parsed.paths.every(isPreciseFilePath)
  );
}

function stringInputField(input: unknown, field: string): string | undefined {
  if (!input || typeof input !== "object") {
    return undefined;
  }

  const value = (input as Record<string, unknown>)[field];
  return typeof value === "string" ? value : undefined;
}

function isAllowedDirectGrep(input: unknown): boolean {
  const pattern = stringInputField(input, "pattern");
  const path =
    stringInputField(input, "path") ?? stringInputField(input, "file");

  return !!(
    pattern &&
    path &&
    !REGEX_META_PATTERN.test(pattern) &&
    isPreciseFilePath(path)
  );
}

function formatToolNames(calls: readonly ToolCall[]): string {
  if (calls.length === 0) {
    return "(none)";
  }

  return calls.map((tc) => toOneupMcpTool(tc.name) ?? tc.name).join(", ");
}

function toolCallIndex(calls: readonly ToolCall[], tool: OneupMcpTool): number {
  return calls.findIndex((tc) => toOneupMcpTool(tc.name) === tool);
}

function firstToolCallIndex(
  calls: readonly ToolCall[],
  tools: readonly OneupMcpTool[],
): number {
  const indexes = tools
    .map((tool) => toolCallIndex(calls, tool))
    .filter((index) => index !== -1);

  return indexes.length > 0 ? Math.min(...indexes) : -1;
}

function hasGetTarget(input: unknown): boolean {
  if (!input || typeof input !== "object") {
    return false;
  }

  const request = input as {
    handles?: unknown;
  };
  return Array.isArray(request.handles) && request.handles.length > 0;
}

function hasContextTarget(input: unknown): boolean {
  if (!input || typeof input !== "object") {
    return false;
  }

  const request = input as {
    locations?: unknown;
  };
  return Array.isArray(request.locations) && request.locations.length > 0;
}

function fallbackViolations(context: EvalContext): string[] {
  const calls = getToolCalls(context);
  const firstSearchIndex = toolCallIndex(calls, "oneup_search");
  const violations: string[] = [];

  calls.forEach((tc, index) => {
    const canonical = toCanonical(tc.name);

    if (canonical === "grep") {
      if (firstSearchIndex === -1 || index < firstSearchIndex) {
        violations.push(`${tc.name} before oneup_search`);
      } else if (!isAllowedDirectGrep(tc.input)) {
        violations.push(`${tc.name} outside exact literal file verification`);
      }
      return;
    }

    if (canonical === "glob") {
      violations.push(`${tc.name} for discovery`);
      return;
    }

    if (canonical === "find") {
      violations.push(`${tc.name} for discovery`);
      return;
    }

    const command =
      canonical === "shell"
        ? ((tc.input as { command?: string })?.command ?? "")
        : "";

    for (const invocation of fallbackInvocations(command)) {
      const excerpt = invocation.raw.slice(0, 80);

      if (invocation.tool === "find") {
        violations.push(`find in Bash: ${excerpt}`);
      } else if (firstSearchIndex === -1 || index < firstSearchIndex) {
        violations.push(`${invocation.tool} before oneup_search: ${excerpt}`);
      } else if (!isAllowedGrepLikeInvocation(invocation)) {
        violations.push(
          `${invocation.tool} outside exact literal file verification: ${excerpt}`,
        );
      }
    }
  });

  return violations;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function parseJsonRecord(value: string): Record<string, unknown> | undefined {
  try {
    const parsed = JSON.parse(value) as unknown;
    return isRecord(parsed) ? parsed : undefined;
  } catch {
    return undefined;
  }
}

function outputRecord(output: unknown): Record<string, unknown> | undefined {
  if (isRecord(output)) {
    return output;
  }

  return typeof output === "string" ? parseJsonRecord(output) : undefined;
}

function extractStructuredEnvelope(
  output: unknown,
): Record<string, unknown> | undefined {
  const record = outputRecord(output);
  if (!record) {
    return undefined;
  }

  const structuredContent =
    record.structuredContent ?? record.structured_content;
  if (isRecord(structuredContent)) {
    return structuredContent;
  }

  return undefined;
}

function validateEnvelope(envelope: Record<string, unknown>): string[] {
  const problems: string[] = [];
  const status = envelope.status;
  const summary = envelope.summary;
  const nextActions = envelope.next_actions;

  if (typeof status !== "string" || status.length === 0) {
    problems.push("missing string status");
  }

  if (typeof summary !== "string" || summary.length === 0) {
    problems.push("missing string summary");
  } else if (ANSI_ESCAPE_PATTERN.test(summary)) {
    problems.push("summary contains ANSI terminal presentation");
  }

  if (!("data" in envelope)) {
    problems.push("missing data");
  } else if (!isRecord(envelope.data)) {
    problems.push("data must be an object");
  }

  if (!Array.isArray(nextActions)) {
    problems.push("missing next_actions array");
  } else {
    for (const action of nextActions) {
      if (!isRecord(action)) {
        problems.push("next_actions contains non-object action");
        continue;
      }

      const tool = action.tool;
      if (typeof tool !== "string" || !ONEUP_MCP_TOOL_SET.has(tool)) {
        problems.push(`next_actions contains non-canonical tool ${tool}`);
      }

      if (typeof action.reason !== "string" || action.reason.length === 0) {
        problems.push("next_actions contains action without string reason");
      }

      if (action.arguments !== undefined && !isRecord(action.arguments)) {
        problems.push("next_actions contains non-object arguments");
      }
    }
  }

  return problems;
}

export function assert1upUsed(
  _output: string,
  context: EvalContext,
): GradingResult {
  const skipped = baselineSkip(context);
  if (skipped) return skipped;

  const calls = getOneupCalls(context);
  const found = calls.some((tc) => toOneupMcpTool(tc.name) === "oneup_search");

  return {
    pass: found,
    score: found ? 1 : 0,
    reason: found
      ? "Agent invoked canonical MCP discovery tool oneup_search"
      : `Agent did not invoke oneup_search. MCP 1up calls seen: ${formatToolNames(calls)}`,
  };
}

export function assertReadinessWorkflowUsed(
  _output: string,
  context: EvalContext,
): GradingResult {
  const skipped = baselineSkip(context);
  if (skipped) return skipped;

  const calls = getToolCalls(context);
  const statusIndex = toolCallIndex(calls, "oneup_status");
  const startIndex = toolCallIndex(calls, "oneup_start");
  const firstDiscoveryIndex = firstToolCallIndex(calls, ONEUP_DISCOVERY_TOOLS);

  const problems: string[] = [];

  if (statusIndex === -1) {
    problems.push("missing oneup_status readiness check");
  }

  if (
    statusIndex !== -1 &&
    firstDiscoveryIndex !== -1 &&
    statusIndex > firstDiscoveryIndex
  ) {
    problems.push("oneup_status happened after discovery");
  }

  if (startIndex !== -1 && (statusIndex === -1 || startIndex < statusIndex)) {
    problems.push("oneup_start happened before oneup_status");
  }

  if (
    startIndex !== -1 &&
    firstDiscoveryIndex !== -1 &&
    startIndex > firstDiscoveryIndex
  ) {
    problems.push("oneup_start happened after discovery");
  }

  const pass = problems.length === 0;

  return {
    pass,
    score: pass ? 1 : 0,
    reason: pass
      ? startIndex === -1
        ? "Agent checked readiness with oneup_status before discovery"
        : "Agent checked readiness with oneup_status and used oneup_start after status"
      : `Agent did not follow the retained status/start readiness workflow: ${problems.join(", ")}`,
  };
}

const ONEUP_INDEXING_SUBCOMMANDS = new Set(["index", "reindex", "start"]);

function oneupIndexingCommands(command: string): string[] {
  const found: string[] = [];
  let segment: ShellToken[] = [];

  const flush = () => {
    const toolIndex = segment.findIndex(
      (token) => fallbackToolName(token.value) === undefined && isOneup(token),
    );
    if (toolIndex !== -1) {
      const subcommand = segment
        .slice(toolIndex + 1)
        .find((token) => !token.value.startsWith("-"));
      if (subcommand && ONEUP_INDEXING_SUBCOMMANDS.has(subcommand.value)) {
        found.push(
          segment
            .slice(toolIndex)
            .map((token) => token.value)
            .join(" "),
        );
      }
    }
    segment = [];
  };

  for (const token of tokenizeShell(command)) {
    if (SHELL_SEGMENT_BOUNDARIES.has(token.value)) {
      flush();
    } else {
      segment.push(token);
    }
  }
  flush();

  return found;
}

function isOneup(token: ShellToken): boolean {
  const name = token.value.split(/[\\/]/).at(-1) ?? token.value;
  return name === "1up";
}

/**
 * Warm-suite readiness contract (REQ-001): the fixture hook pre-warms the
 * index before the agent starts, so the measured trajectory must contain
 * exactly one initial `oneup_status` before any discovery call and must never
 * trigger indexing from the measurement loop — no `oneup_start`, no Bash
 * `1up index/reindex/start`. Any of those contaminates the token/latency/cost
 * axes with cold-start work. Cold-start readiness waiting lives in the separate
 * cold suite, which keeps using `assertReadinessWorkflowUsed`.
 */
export function assertWarmReadiness(
  _output: string,
  context: EvalContext,
): GradingResult {
  const skipped = baselineSkip(context);
  if (skipped) return skipped;

  const calls = getToolCalls(context);
  const statusCalls = calls.filter(
    (tc) => toOneupMcpTool(tc.name) === "oneup_status",
  );
  const statusIndex = toolCallIndex(calls, "oneup_status");
  const startIndex = toolCallIndex(calls, "oneup_start");
  const firstDiscoveryIndex = firstToolCallIndex(calls, ONEUP_DISCOVERY_TOOLS);

  const indexingCommands = calls.flatMap((tc) => {
    if (toCanonical(tc.name) !== "shell") {
      return [];
    }
    const command = (tc.input as { command?: string })?.command ?? "";
    return oneupIndexingCommands(command);
  });

  const problems: string[] = [];

  if (statusCalls.length === 0) {
    problems.push("missing initial oneup_status readiness check");
  } else if (statusCalls.length > 1) {
    problems.push(
      `expected exactly one oneup_status, saw ${statusCalls.length}`,
    );
  }

  if (
    statusIndex !== -1 &&
    firstDiscoveryIndex !== -1 &&
    statusIndex > firstDiscoveryIndex
  ) {
    problems.push("oneup_status happened after discovery");
  }

  if (startIndex !== -1) {
    problems.push("oneup_start triggered indexing in a warm case");
  }

  if (indexingCommands.length > 0) {
    problems.push(
      `Bash indexing in a warm case: ${indexingCommands
        .map((command) => command.slice(0, 60))
        .join(", ")}`,
    );
  }

  const pass = problems.length === 0;

  return {
    pass,
    score: pass ? 1 : 0,
    reason: pass
      ? "Agent performed exactly one initial oneup_status on the pre-warmed index and triggered no indexing"
      : `Agent violated the warm readiness contract: ${problems.join(", ")}`,
  };
}

export function assert1upImpactUsed(
  _output: string,
  context: EvalContext,
): GradingResult {
  const skipped = baselineSkip(context);
  if (skipped) return skipped;

  const calls = getOneupCalls(context, "oneup_impact");
  const found = calls.length > 0;

  return {
    pass: found,
    score: found ? 1 : 0,
    reason: found
      ? "Agent invoked canonical MCP impact tool oneup_impact"
      : `Agent did not invoke oneup_impact. MCP 1up calls seen: ${formatToolNames(getOneupCalls(context))}`,
  };
}

export function assertNoFallbackTools(
  _output: string,
  context: EvalContext,
): GradingResult {
  const skipped = baselineSkip(context);
  if (skipped) return skipped;

  const violations = fallbackViolations(context);

  const pass = violations.length === 0;
  return {
    pass,
    score: pass ? 1 : 0,
    reason: pass
      ? "Agent did not use raw discovery tools before oneup_search"
      : `Agent used raw discovery tools outside the allowed post-search verification path: ${[...new Set(violations)].join(", ")}`,
  };
}

export function assertStructuredOneupMcpResponses(
  _output: string,
  context: EvalContext,
): GradingResult {
  const skipped = baselineSkip(context);
  if (skipped) return skipped;

  const callsWithOutput = getOneupCalls(context).filter(
    (tc) => tc.output != null,
  );

  if (callsWithOutput.length === 0) {
    return {
      pass: true,
      score: 1,
      reason:
        "Provider metadata did not include captured MCP outputs; tool-call assertions cover retained API use and protocol smoke validates envelope shape",
    };
  }

  const missingEnvelopeProblems: string[] = [];
  const envelopes = callsWithOutput.flatMap((tc) => {
    const tool = toOneupMcpTool(tc.name) ?? tc.name;
    const envelope = extractStructuredEnvelope(tc.output);
    if (!envelope && tc.structuredOutputRequired) {
      missingEnvelopeProblems.push(
        `${tool}: missing structured_content object`,
      );
    }
    return envelope ? [{ tool, envelope }] : [];
  });

  if (envelopes.length === 0 && missingEnvelopeProblems.length === 0) {
    return {
      pass: true,
      score: 1,
      reason:
        "Provider metadata did not include captured structured MCP outputs; tool-call assertions cover retained API use and protocol smoke validates envelope shape",
    };
  }

  const problems = [
    ...missingEnvelopeProblems,
    ...envelopes.flatMap(({ tool, envelope }) => {
      return validateEnvelope(envelope).map((problem) => `${tool}: ${problem}`);
    }),
  ];

  const pass = problems.length === 0;

  return {
    pass,
    score: pass ? 1 : 0,
    reason: pass
      ? "Captured oneup MCP outputs used structured ToolEnvelope fields"
      : `Captured oneup MCP outputs were not valid structured envelopes: ${problems.join(", ")}`,
  };
}

export function assertReadAfterSearch(
  _output: string,
  context: EvalContext,
): GradingResult {
  const skipped = baselineSkip(context);
  if (skipped) return skipped;

  const calls = getToolCalls(context);
  const searchIndex = toolCallIndex(calls, "oneup_search");
  const hydrationIndex = calls.findIndex((tc, index) => {
    if (index <= searchIndex) {
      return false;
    }

    const tool = toOneupMcpTool(tc.name);
    return (
      (tool === "oneup_get" && hasGetTarget(tc.input)) ||
      (tool === "oneup_context" && hasContextTarget(tc.input))
    );
  });
  const pass = searchIndex !== -1 && hydrationIndex !== -1;

  return {
    pass,
    score: pass ? 1 : 0,
    reason: pass
      ? "Agent hydrated a search result with oneup_get or oneup_context"
      : `Agent did not call oneup_get with handles or oneup_context with locations after oneup_search. MCP 1up calls seen: ${formatToolNames(getOneupCalls(context))}`,
  };
}

export function assertSymbolVerificationUsed(
  _output: string,
  context: EvalContext,
): GradingResult {
  const skipped = baselineSkip(context);
  if (skipped) return skipped;

  const calls = getOneupCalls(context, "oneup_symbol");
  const pass = calls.length > 0;

  return {
    pass,
    score: pass ? 1 : 0,
    reason: pass
      ? "Agent used oneup_symbol for completeness-oriented verification"
      : `Agent did not invoke oneup_symbol. MCP 1up calls seen: ${formatToolNames(getOneupCalls(context))}`,
  };
}

export function assertImpactTrustInterpreted(
  output: string,
  context: EvalContext,
): GradingResult {
  const skipped = baselineSkip(context);
  if (skipped) return skipped;

  const impactCalls = getOneupCalls(context, "oneup_impact");
  const hasPrimaryBoundary =
    /\b(?:primary|direct)\b(?:\s|[/:(—-]){1,8}\b(?:high|higher)[ -]confidence\b/i.test(
      output,
    );
  const hasContextualBoundary =
    /\b(?:contextual|indirect)\b(?:\s|[/:(—-]){1,8}\b(?:low|lower)[ -]confidence\b/i.test(
      output,
    );
  const interpretedTrust = hasPrimaryBoundary && hasContextualBoundary;
  const pass = impactCalls.length > 0 && interpretedTrust;

  return {
    pass,
    score: pass ? 1 : impactCalls.length > 0 ? 0.5 : 0,
    reason: pass
      ? "Agent separated primary/direct higher-confidence impact from contextual/indirect lower-confidence guidance"
      : impactCalls.length > 0
        ? "Agent called oneup_impact but did not explicitly separate primary/direct higher-confidence findings from contextual/indirect lower-confidence guidance"
        : "Agent did not call oneup_impact",
  };
}

export function assertValidOneupMcpCalls(
  _output: string,
  context: EvalContext,
): GradingResult {
  const skipped = baselineSkip(context);
  if (skipped) return skipped;

  const calls = getToolCalls(context);
  const badAliases = calls
    .filter((tc) => usesDigitLeadingOneupAlias(tc.name))
    .map((tc) => tc.name);
  const badOneupServerTools = calls
    .filter((tc) => tc.name.startsWith("mcp__oneup__"))
    .filter((tc) => !toOneupMcpTool(tc.name))
    .map((tc) => tc.name);
  const erroredOneupCalls = calls
    .filter((tc) => toOneupMcpTool(tc.name) && tc.is_error)
    .map((tc) => toOneupMcpTool(tc.name) ?? tc.name);
  const problems = [
    ...badAliases.map((name) => `digit-leading alias ${name}`),
    ...badOneupServerTools.map((name) => `unknown oneup MCP tool ${name}`),
    ...erroredOneupCalls.map((name) => `errored MCP call ${name}`),
  ];
  const pass = problems.length === 0;

  return {
    pass,
    score: pass ? 1 : 0,
    reason: pass
      ? "Agent used canonical oneup_* MCP tool names without MCP call errors"
      : `Invalid MCP tool-use signals: ${[...new Set(problems)].join(", ")}`,
  };
}

/**
 * Neutral run-metrics reporter (REQ-002): always passes with a fixed neutral
 * score so it can never advantage either provider in Promptfoo's
 * (non-authoritative) composite, and emits the raw latency/tokens/cost/calls
 * measures as namedScores. The per-axis report (axes-report.ts) splits these
 * into the independent latency, tokens, and cost axes; graded scoring lives
 * there, not here.
 */
export function reportRunMetrics(
  _output: string,
  context: EvalContext,
): GradingResult {
  const meta = context.providerResponse?.metadata;
  const cost = context.providerResponse?.cost;

  let rawRecord: Record<string, unknown> | undefined;
  const rawValue = context.providerResponse?.raw;
  if (isRecord(rawValue)) {
    rawRecord = rawValue;
  } else if (typeof rawValue === "string") {
    rawRecord = parseJsonRecord(rawValue);
  }

  const turns = meta?.numTurns ?? (Array.isArray(rawRecord?.items) ? 1 : 0);
  const durationMs = meta?.durationMs ?? 0;

  // Parse the raw SDK response to get full token counts including cache.
  // promptfoo's tokenUsage only captures input_tokens + output_tokens,
  // missing cache_read and cache_creation which are the bulk of usage.
  let inputTokens = 0;
  let outputTokens = 0;
  let cacheCreation = 0;
  let cachedInput = 0;
  let reasoningOutput = 0;
  let debugInfo = "";

  if (rawRecord) {
    const usage = isRecord(rawRecord.usage) ? rawRecord.usage : {};
    inputTokens =
      typeof usage.input_tokens === "number" ? usage.input_tokens : 0;
    outputTokens =
      typeof usage.output_tokens === "number" ? usage.output_tokens : 0;
    cacheCreation =
      typeof usage.cache_creation_input_tokens === "number"
        ? usage.cache_creation_input_tokens
        : 0;
    cachedInput =
      typeof usage.cached_input_tokens === "number"
        ? usage.cached_input_tokens
        : 0;
    reasoningOutput =
      typeof usage.reasoning_output_tokens === "number"
        ? usage.reasoning_output_tokens
        : 0;
  } else if (rawValue !== undefined) {
    debugInfo = " [raw parse failed]";
  } else {
    // No raw — try tokenUsage as fallback
    const tu = context.providerResponse?.tokenUsage;
    inputTokens = tu?.prompt ?? 0;
    outputTokens = tu?.completion ?? 0;
    debugInfo = ` [no raw, keys: ${Object.keys(context.providerResponse ?? {}).join(",")}]`;
  }

  const tokens =
    inputTokens + outputTokens + cacheCreation + cachedInput + reasoningOutput;
  const costUsd = cost ?? 0;
  const calls = getToolCalls(context).length;
  const durationS = Math.round(durationMs / 1000);

  const namedScores: Record<string, number> = {
    latency_ms: durationMs,
    tokens,
    cost_usd: costUsd,
    calls,
  };

  const usageDetails = [
    `in:${inputTokens.toLocaleString()}`,
    `out:${outputTokens.toLocaleString()}`,
    `cache_create:${cacheCreation.toLocaleString()}`,
    ...(cachedInput > 0 ? [`cached:${cachedInput.toLocaleString()}`] : []),
    ...(reasoningOutput > 0
      ? [`reasoning:${reasoningOutput.toLocaleString()}`]
      : []),
  ].join(" ");

  return {
    pass: true,
    score: 1,
    namedScores,
    reason: `${durationMs > 0 ? `${durationS}s` : "duration n/a"} | ${cost === undefined ? "cost n/a" : `$${costUsd.toFixed(2)}`} | ${calls} ${calls === 1 ? "call" : "calls"} | ${turns} ${turns === 1 ? "turn" : "turns"} | tokens ${usageDetails}${debugInfo}`,
  };
}

/**
 * Back-compat alias for the untouched Claude-path suites (`evals.yaml`), which
 * still reference `reportEfficiency`. The Luna warm and cold suites reference
 * `reportRunMetrics` directly.
 */
export const reportEfficiency = reportRunMetrics;

export function assertExpectedFiles(
  expectedFiles: string[],
): (output: string, context: EvalContext) => GradingResult {
  return (output: string, _context: EvalContext): GradingResult => {
    const missing: string[] = [];

    for (const file of expectedFiles) {
      const basename = file.split("/").pop() ?? file;
      if (!output.includes(basename)) {
        missing.push(basename);
      }
    }

    const pass = missing.length === 0;
    const found = expectedFiles.length - missing.length;
    return {
      pass,
      score: found / expectedFiles.length,
      reason: pass
        ? `Agent referenced all expected files: ${expectedFiles.join(", ")}`
        : `Agent missing references to: ${missing.join(", ")}`,
    };
  };
}
