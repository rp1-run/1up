export type CanonicalTool = "shell" | "read";

const CANONICAL_MAP: Record<string, CanonicalTool> = {
  Bash: "shell",
  Read: "read",
  bash: "shell",
  read: "read",
};

export function toCanonical(
  providerToolName: string,
): CanonicalTool | undefined {
  return CANONICAL_MAP[providerToolName];
}
