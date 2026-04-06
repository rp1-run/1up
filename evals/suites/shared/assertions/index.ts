import type { GradingResult } from "promptfoo";
import { toCanonical } from "../tool-names.ts";

interface AssertionContext {
  vars?: Record<string, string | number | boolean | object>;
  test?: Record<string, unknown>;
  logProbs?: number[];
  config?: Record<string, unknown>;
  provider?: unknown;
  providerResponse?: {
    raw?: unknown;
    output?: unknown;
    metadata?: Record<string, unknown>;
  };
}

const ONEUP_COMMANDS = ["1up search", "1up symbol", "1up context"];

const FALLBACK_PATTERNS = [
  /(?:^|\s|&&|\|\||;)rg\s/m,
  /(?:^|\s|&&|\|\||;)grep\s/m,
  /(?:^|\s|&&|\|\||;)find\s/m,
];

function isShellTool(toolName: string): boolean {
  return toCanonical(toolName) === "shell";
}

function extractToolLog(output: string, context: AssertionContext): string {
  const parts: string[] = [output];

  if (context.providerResponse?.raw) {
    parts.push(
      typeof context.providerResponse.raw === "string"
        ? context.providerResponse.raw
        : JSON.stringify(context.providerResponse.raw),
    );
  }

  if (
    context.providerResponse?.output &&
    context.providerResponse.output !== output
  ) {
    parts.push(
      typeof context.providerResponse.output === "string"
        ? context.providerResponse.output
        : JSON.stringify(context.providerResponse.output),
    );
  }

  return parts.join("\n");
}

function extractShellCommands(toolLog: string): string[] {
  const commands: string[] = [];

  const shellToolNames = ["Bash", "bash"].filter(isShellTool);
  const namePattern = shellToolNames.join("|");

  const toolUsePattern = new RegExp(
    `"(?:tool_name|name)"\\s*:\\s*"(?:${namePattern})"\\s*[\\s\\S]*?"(?:command|input)"\\s*:\\s*"([^"]*)"`,
    "g",
  );
  let match: RegExpExecArray | null;
  match = toolUsePattern.exec(toolLog);
  while (match !== null) {
    commands.push(match[1]);
    match = toolUsePattern.exec(toolLog);
  }

  const inlinePattern = /(?:1up\s+(?:search|symbol|context))\b[^\n]*/g;
  match = inlinePattern.exec(toolLog);
  while (match !== null) {
    commands.push(match[0]);
    match = inlinePattern.exec(toolLog);
  }

  return commands;
}

export function assert1upUsed(
  output: string,
  context: AssertionContext,
): GradingResult {
  const toolLog = extractToolLog(output, context);
  const shellCommands = extractShellCommands(toolLog);

  const found = shellCommands.some((cmd) =>
    ONEUP_COMMANDS.some((oneup) => cmd.includes(oneup)),
  );

  if (!found) {
    const directMatch = ONEUP_COMMANDS.some((cmd) => toolLog.includes(cmd));
    if (directMatch) {
      return {
        pass: true,
        score: 1,
        reason: "Agent invoked at least one 1up command",
      };
    }
  }

  return {
    pass: found,
    score: found ? 1 : 0,
    reason: found
      ? "Agent invoked at least one 1up command"
      : "Agent did not invoke any 1up commands (search, symbol, or context)",
  };
}

export function assertNoFallbackTools(
  output: string,
  context: AssertionContext,
): GradingResult {
  const toolLog = extractToolLog(output, context);
  const shellCommands = extractShellCommands(toolLog);

  const violations: string[] = [];

  for (const cmd of shellCommands) {
    for (const pattern of FALLBACK_PATTERNS) {
      if (pattern.test(cmd)) {
        const tool = cmd.match(pattern)?.[0]?.trim();
        if (tool) {
          violations.push(tool);
        }
      }
    }
  }

  if (violations.length === 0) {
    for (const pattern of FALLBACK_PATTERNS) {
      if (pattern.test(toolLog)) {
        const tool = toolLog.match(pattern)?.[0]?.trim();
        if (tool) {
          violations.push(tool);
        }
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
): (output: string, context: AssertionContext) => GradingResult {
  return (output: string, _context: AssertionContext): GradingResult => {
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
