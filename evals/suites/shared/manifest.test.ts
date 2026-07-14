import { existsSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, test } from "bun:test";

import {
  type Contract,
  MANIFEST_RELATIVE_PATH,
  buildContract,
  buildManifest,
  canonicalize,
  contractHash,
} from "./manifest.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const EVALS_ROOT = resolve(__dirname, "../..");
const MANIFEST_PATH = resolve(EVALS_ROOT, MANIFEST_RELATIVE_PATH);

describe("committed manifest drift guard (REQ-003)", () => {
  test("suites/luna-manifest.json exists under the tracked suites path", () => {
    // REQ-003 AC2: the contract lives under tracked evals/suites/, never a
    // gitignored run-output directory.
    expect(MANIFEST_RELATIVE_PATH.startsWith("suites/")).toBe(true);
    expect(existsSync(MANIFEST_PATH)).toBe(true);
  });

  test("committed contract_hash and contract match a live recomputation", () => {
    const committed = JSON.parse(readFileSync(MANIFEST_PATH, "utf8"));
    const rebuilt = buildManifest();

    expect(committed.manifest_version).toBe(rebuilt.manifest_version);
    expect(committed.contract_hash).toBe(rebuilt.contract_hash);
    expect(committed.contract).toEqual(rebuilt.contract);
  });

  test("committed contract_hash equals sha256 of its own canonical contract", () => {
    const committed = JSON.parse(readFileSync(MANIFEST_PATH, "utf8")) as {
      contract_hash: string;
      contract: Contract;
    };
    expect(committed.contract_hash).toBe(contractHash(committed.contract));
    expect(committed.contract_hash).toMatch(/^[0-9a-f]{64}$/);
  });
});

describe("contract hashing", () => {
  test("hash is independent of object key insertion order", () => {
    const contract = buildContract();
    const reordered = {
      trial_count: contract.trial_count,
      suites: contract.suites,
      cost_transform: contract.cost_transform,
      prompts: contract.prompts,
      axes: contract.axes,
      grader: contract.grader,
      fixture: contract.fixture,
    } as Contract;

    expect(contractHash(reordered)).toBe(contractHash(contract));
  });

  test("canonicalize sorts nested keys deterministically", () => {
    expect(canonicalize({ b: 1, a: { d: 2, c: 3 } })).toBe(
      '{"a":{"c":3,"d":2},"b":1}',
    );
  });

  test.each([
    [
      "prompt content",
      (c: Contract) => {
        c.prompts[0].sha256 = "0".repeat(64);
      },
    ],
    [
      "suite content",
      (c: Contract) => {
        c.suites[0].sha256 = "0".repeat(64);
      },
    ],
    [
      "fixture commit",
      (c: Contract) => {
        c.fixture.commit = "deadbee";
      },
    ],
    [
      "grader model",
      (c: Contract) => {
        c.grader.model = "some-other-model";
      },
    ],
    [
      "axes mapping",
      (c: Contract) => {
        c.axes.graded = [...c.axes.graded, "extra-axis"];
      },
    ],
    [
      "cost transform",
      (c: Contract) => {
        c.cost_transform.speed_baseline_s = 999;
      },
    ],
    [
      "trial count",
      (c: Contract) => {
        c.trial_count = 2;
      },
    ],
  ])("changing the %s flips the contract hash", (_label, mutate) => {
    const base = buildContract();
    const baseHash = contractHash(base);
    const mutated = structuredClone(base);
    mutate(mutated);

    expect(contractHash(mutated)).not.toBe(baseHash);
  });
});
