export const ONEUP_MCP_TOOLS = [
  "oneup_prepare",
  "oneup_search",
  "oneup_read",
  "oneup_symbol",
  "oneup_impact",
] as const;

export type OneupMcpTool = (typeof ONEUP_MCP_TOOLS)[number];
export type CanonicalTool =
  | "shell"
  | "read"
  | "grep"
  | "glob"
  | "find"
  | OneupMcpTool;

const ONEUP_MCP_TOOL_SET = new Set<string>(ONEUP_MCP_TOOLS);

const CANONICAL_MAP: Record<string, CanonicalTool> = {
  Bash: "shell",
  Read: "read",
  Grep: "grep",
  Glob: "glob",
  Find: "find",
  bash: "shell",
  read: "read",
  grep: "grep",
  glob: "glob",
  find: "find",
};

function extractMcpToolName(providerToolName: string): string | undefined {
  if (providerToolName.startsWith("mcp__")) {
    return providerToolName.split("__").at(-1);
  }

  const dotted = providerToolName.match(/^mcp[:.]([^:.]+)[:.](.+)$/);
  if (dotted) {
    return dotted[2];
  }

  return undefined;
}

export function toCanonical(
  providerToolName: string,
): CanonicalTool | undefined {
  const mcpToolName = extractMcpToolName(providerToolName);
  if (mcpToolName && ONEUP_MCP_TOOL_SET.has(mcpToolName)) {
    return mcpToolName as OneupMcpTool;
  }

  if (ONEUP_MCP_TOOL_SET.has(providerToolName)) {
    return providerToolName as OneupMcpTool;
  }

  return CANONICAL_MAP[providerToolName];
}

export function toOneupMcpTool(
  providerToolName: string,
): OneupMcpTool | undefined {
  const canonical = toCanonical(providerToolName);
  return canonical && ONEUP_MCP_TOOL_SET.has(canonical)
    ? (canonical as OneupMcpTool)
    : undefined;
}

export function usesDigitLeadingOneupAlias(providerToolName: string): boolean {
  const mcpToolName = extractMcpToolName(providerToolName);
  return !!(
    providerToolName.startsWith("1up_") || mcpToolName?.startsWith("1up_")
  );
}
