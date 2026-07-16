import { existsSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, test } from "bun:test";

import { EFFICIENCY_MEASURES, GRADED_AXES } from "./axes-report.ts";
import { MANIFEST_RELATIVE_PATH } from "./manifest.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const EVALS_ROOT = resolve(__dirname, "../..");
const BASELINE_RELATIVE_PATH = "suites/luna-baseline.json";
const BASELINE_PATH = resolve(EVALS_ROOT, BASELINE_RELATIVE_PATH);
const MANIFEST_PATH = resolve(EVALS_ROOT, MANIFEST_RELATIVE_PATH);

/** The eight per-axis fields every provider record must carry. */
const AXIS_KEYS = [...GRADED_AXES, ...EFFICIENCY_MEASURES] as const;

interface BaselineFile {
  pending_capture?: boolean;
  contract_hash?: string;
  captured_at?: string | null;
  providers?: Record<string, Record<string, unknown>>;
}

function readBaseline(): BaselineFile {
  return JSON.parse(readFileSync(BASELINE_PATH, "utf8")) as BaselineFile;
}

/**
 * Drift guard for the committed warm per-axis baseline. The real
 * per-axis figures come from a manual credentialed warm run
 * (`npm run eval:parallel:luna` then `axes-report --record-baseline`), which is
 * never run in-agent, so a placeholder is committed first. These invariants hold
 * both before capture (placeholder, `pending_capture`, empty providers) and
 * after capture (real per-provider axis values), so the guard is green in both
 * states — the same pattern the recall drift guard uses across the schema
 * recapture.
 */
describe("committed luna-baseline drift guard", () => {
  test("baseline lives under the tracked suites path and exists", () => {
    // Benchmark artifacts live under tracked evals/suites/,
    // never a gitignored run-output directory.
    expect(BASELINE_RELATIVE_PATH.startsWith("suites/")).toBe(true);
    expect(existsSync(BASELINE_PATH)).toBe(true);
  });

  test("contract_hash is stamped with the current frozen contract", () => {
    // If the manifest contract changes, this pin fails until the baseline is
    // recaptured — a recorded baseline is only comparable against runs of the
    // same frozen contract.
    const baseline = readBaseline();
    const manifest = JSON.parse(readFileSync(MANIFEST_PATH, "utf8")) as {
      contract_hash?: string;
    };

    expect(typeof baseline.contract_hash).toBe("string");
    expect(baseline.contract_hash).toBe(manifest.contract_hash);
  });

  test("every present provider carries the full per-axis shape", () => {
    const baseline = readBaseline();
    const providers = baseline.providers ?? {};

    for (const [name, axes] of Object.entries(providers)) {
      for (const key of AXIS_KEYS) {
        expect(axes, `${name}.${key} present`).toHaveProperty(key);
        const value = axes[key];
        expect(
          value === null || typeof value === "number",
          `${name}.${key} is number|null`,
        ).toBe(true);
      }
      expect(Array.isArray(axes.source_attempt_ids)).toBe(true);
    }
  });

  test("a pending placeholder is unmistakable and uncaptured", () => {
    const baseline = readBaseline();
    if (!baseline.pending_capture) {
      return; // captured baseline is exercised by the next test
    }
    expect(baseline.captured_at).toBeNull();
    expect(Object.keys(baseline.providers ?? {})).toHaveLength(0);
  });

  test("once captured, providers record axes with source lineage", () => {
    const baseline = readBaseline();
    if (baseline.pending_capture) {
      return; // not yet captured; covered by the placeholder test above
    }
    const providers = baseline.providers ?? {};
    expect(typeof baseline.captured_at).toBe("string");
    expect(Object.keys(providers).length).toBeGreaterThan(0);

    for (const axes of Object.values(providers)) {
      expect((axes.source_attempt_ids as unknown[]).length).toBeGreaterThan(0);
    }
  });
});
