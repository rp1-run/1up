/**
 * Tool-call based assertions for 1up eval tests.
 * Uses provider metadata to inspect tool calls made by the agent.
 */

interface GradingResult {
  pass: boolean;
  score: number;
  reason: string;
  namedScores?: Record<string, number>;
}

interface ToolCall {
  readonly id: string;
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

function getBashCommands(context: EvalContext): string[] {
  return getToolCalls(context)
    .filter((tc) => tc.name === "Bash")
    .map((tc) => (tc.input as { command?: string })?.command ?? "")
    .filter((cmd) => cmd.length > 0);
}

const ONEUP_PATTERN = /\b1up\s+(?:search|symbol|context)\b/;

const FALLBACK_PATTERNS = [
  /(?:^|\s|&&|\|\||;)rg\s/,
  /(?:^|\s|&&|\|\||;)grep\s/,
  /(?:^|\s|&&|\|\||;)find\s/,
];

export function assert1upUsed(
  _output: string,
  context: EvalContext,
): GradingResult {
  const bashCommands = getBashCommands(context);
  const found = bashCommands.some((cmd) => ONEUP_PATTERN.test(cmd));

  return {
    pass: found,
    score: found ? 1 : 0,
    reason: found
      ? "Agent invoked at least one 1up command (search, symbol, or context)"
      : `Agent did not invoke any 1up commands. Bash commands seen: ${bashCommands.length === 0 ? "(none)" : bashCommands.map((c) => c.slice(0, 60)).join("; ")}`,
  };
}

export function assertNoFallbackTools(
  _output: string,
  context: EvalContext,
): GradingResult {
  const bashCommands = getBashCommands(context);
  const violations: string[] = [];

  for (const cmd of bashCommands) {
    for (const pattern of FALLBACK_PATTERNS) {
      if (pattern.test(cmd)) {
        const tool = cmd.match(pattern)?.[0]?.trim();
        if (tool) violations.push(tool);
      }
    }
  }

  const pass = violations.length === 0;
  return {
    pass,
    score: pass ? 1 : 0,
    reason: pass
      ? "Agent did not use fallback search tools"
      : `Agent used fallback tools: ${[...new Set(violations)].join(", ")}`,
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
  const durationSec = (durationMs / 1000).toFixed(1);
  const costStr = cost != null ? `$${cost.toFixed(4)}` : "unknown";

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


  // Score: lower cost = better. Normalize against a $0.50 baseline.
  const COST_BASELINE = 0.5;
  const score = cost != null ? Math.max(0, Math.min(1, 1 - cost / COST_BASELINE)) : 0;

  return {
    pass: true,
    score,
    namedScores: {
      "Duration (s)": Math.round(durationMs / 1000),
      "Cost ($)": cost ?? 0,
      "Turns": turns,
    },
    reason: `Duration: ${durationSec}s | Cost: ${costStr} | Turns: ${turns} | Tokens (in:${inputTokens.toLocaleString()} out:${outputTokens.toLocaleString()} cache_create:${cacheCreation.toLocaleString()})${debugInfo}`,
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
