/**
 * MCP tool-call based assertions for 1up eval tests.
 * Uses provider metadata to inspect tool calls made by the agent.
 */

import {
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

interface ToolCall {
  readonly id?: string;
  readonly name: string;
  readonly input: unknown;
  readonly output?: unknown;
  readonly is_error?: boolean;
  readonly parentToolUseId?: string | null;
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
  vars?: Record<string, string | number | boolean | object>;
  providerResponse?: {
    metadata?: ProviderMetadata;
    tokenUsage?: TokenUsage;
    cost?: number;
    raw?: string;
  };
}

function getToolCalls(context: EvalContext): readonly ToolCall[] {
  return context.providerResponse?.metadata?.toolCalls ?? [];
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

function getBashCommands(context: EvalContext): string[] {
  return getToolCalls(context)
    .filter((tc) => toCanonical(tc.name) === "shell")
    .map((tc) => (tc.input as { command?: string })?.command ?? "")
    .filter((cmd) => cmd.length > 0);
}

const FALLBACK_TOOLS = ["rg", "grep", "find"] as const;

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

export function assert1upUsed(
  _output: string,
  context: EvalContext,
): GradingResult {
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

export function assert1upImpactUsed(
  _output: string,
  context: EvalContext,
): GradingResult {
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

export function assertReadAfterSearch(
  _output: string,
  context: EvalContext,
): GradingResult {
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
  const impactCalls = getOneupCalls(context, "oneup_impact");
  const interpretedTrust =
    /\b(primary|contextual|lower-confidence|confidence|advisory|likely-impact)\b/i.test(
      output,
    );
  const pass = impactCalls.length > 0 && interpretedTrust;

  return {
    pass,
    score: pass ? 1 : impactCalls.length > 0 ? 0.5 : 0,
    reason: pass
      ? "Agent interpreted impact output with explicit trust-boundary language"
      : impactCalls.length > 0
        ? "Agent called oneup_impact but did not distinguish primary/contextual or confidence boundaries in the answer"
        : "Agent did not call oneup_impact",
  };
}

export function assertValidOneupMcpCalls(
  _output: string,
  context: EvalContext,
): GradingResult {
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

export function reportEfficiency(
  _output: string,
  context: EvalContext,
): GradingResult {
  const meta = context.providerResponse?.metadata;
  const cost = context.providerResponse?.cost;

  const turns = meta?.numTurns ?? 0;
  const durationMs = meta?.durationMs ?? 0;

  // Parse the raw SDK response to get full token counts including cache.
  // promptfoo's tokenUsage only captures input_tokens + output_tokens,
  // missing cache_read and cache_creation which are the bulk of usage.
  let inputTokens = 0;
  let outputTokens = 0;
  let cacheCreation = 0;
  let debugInfo = "";

  const rawStr = context.providerResponse?.raw;
  if (rawStr) {
    try {
      const raw = typeof rawStr === "string" ? JSON.parse(rawStr) : rawStr;
      const usage = raw.usage ?? {};
      inputTokens = usage.input_tokens ?? 0;
      outputTokens = usage.output_tokens ?? 0;
      cacheCreation = usage.cache_creation_input_tokens ?? 0;
    } catch {
      debugInfo = " [raw parse failed]";
    }
  } else {
    // No raw — try tokenUsage as fallback
    const tu = context.providerResponse?.tokenUsage;
    inputTokens = tu?.prompt ?? 0;
    outputTokens = tu?.completion ?? 0;
    debugInfo = ` [no raw, keys: ${Object.keys(context.providerResponse ?? {}).join(",")}]`;
  }

  const durationS = Math.round(durationMs / 1000);
  const costVal = cost ?? 0;

  // Efficiency scores: higher = better, 0-100 scale for readability.
  // Speed: 200s baseline → 0, 0s → 100. e.g. 50s → 75, 100s → 50
  const speedScore = Math.max(
    0,
    Math.min(100, Math.round((1 - durationS / 200) * 100)),
  );
  // Cost: $0.50 baseline → 0, $0 → 100. e.g. $0.25 → 50, $0.10 → 80
  const costScore = Math.max(
    0,
    Math.min(100, Math.round((1 - costVal / 0.5) * 100)),
  );

  return {
    pass: true,
    score: costScore / 100,
    namedScores: {
      Speed: speedScore,
      "Cost Efficiency": costScore,
    },
    reason: `${durationS}s | $${costVal.toFixed(2)} | ${turns} turns | tokens in:${inputTokens.toLocaleString()} out:${outputTokens.toLocaleString()} cache_create:${cacheCreation.toLocaleString()}${debugInfo}`,
  };
}

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
