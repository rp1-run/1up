/**
 * Tool-call based assertions for 1up eval tests.
 * Uses provider metadata to inspect tool calls made by the agent.
 */

interface GradingResult {
  pass: boolean;
  score: number;
  reason: string;
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
}

interface EvalContext {
  vars?: Record<string, string | number | boolean | object>;
  providerResponse?: {
    metadata?: ProviderMetadata;
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
