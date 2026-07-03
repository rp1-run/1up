# List available recipes
default:
    @just --list

# Build release binary and copy to ~/.local/bin
install:
    cargo build --release
    mkdir -p ~/.local/bin
    cp target/release/1up ~/.local/bin/1up
    codesign -f -s - ~/.local/bin/1up

# Search comparison against the pinned emdash corpus.
bench:
    cargo build --release
    @cd evals && ONEUP_BENCH_BIN=../target/release/1up bun run bench

# Retained evidence for semantic indexing, incremental indexing, and daemon refresh.
bench-parallel:
    ./scripts/benchmark_parallel_indexing.sh

# Retained Criterion evidence for local search latency.
bench-search-latency:
    cargo bench --bench search_bench

# Fresh-reindexes the 1up repo and gates index.db <= 80 MiB,
# indexing_ms <= 90000, and current schema. Pinned baseline for delta reporting
# lives at scripts/baselines/vector_index_size_baseline.json.
# Semantic index storage + throughput guard for retained release evidence.
bench-vector-index-size *flags:
    ./scripts/benchmark_vector_index_size.sh {{flags}}

security-check:
    ./scripts/security_check.sh

# Local verification gate. Runs formatter, linter, the full test surface,
# and the install-script smoke required for release readiness.
# Uses `cargo test` (no per-suite filters) so any integration test crate added
# later -- including the security gate's existing targets -- is picked up
# automatically and verify cannot pass while CI fails.
verify:
    cargo fmt --all -- --check
    cargo clippy --all-targets -- -D warnings
    cargo test
    if command -v shellcheck >/dev/null 2>&1; then shellcheck --severity=style scripts/install/setup.sh; else echo "shellcheck not found; skipping shell style lint"; fi
    bash -n scripts/install/setup.sh

eval *flags:
    @cd evals && bun run eval; if echo "{{flags}}" | grep -q -- '--summary'; then just eval-summary; fi

# Run eval tests in parallel (separate promptfoo process per test)
eval-parallel *flags:
    @cd evals && ./run-parallel.sh; if echo "{{flags}}" | grep -q -- '--summary'; then just eval-summary; fi

eval-summary:
    @cd evals && ./summary.sh

# Gated recall@k run: build the repo-local `1up`, reindex, then run the harness
# which (1) preflights the semantic path (vector_rows > 0, current schema,
# expected embedding_model variant), (2) fails on degraded/FTS-only responses,
# and (3) compares recall against the pinned baseline within tolerance
# (ONEUP_RECALL_TOLERANCE, default 0.02), exiting non-zero on regression.
# MODEL-ENABLED: never run in-agent (it hangs); manual/scheduled DoD only.
# `reindex` (not `index`) is used so the model-identity gate cannot fail closed
# on a stale index. Writes evals/suites/1up-search/recall-results.json.
# The harness's own output artifacts (recall-*.json) are excluded from the
# index: they are rewritten between runs, and indexing them perturbs the
# corpus enough to flap the gate (observed: +1 segment -> -2.2pp recall@20).
eval-recall:
    cargo build --bin 1up
    ./target/debug/1up reindex . --exclude-glob 'evals/suites/1up-search/recall-*.json'
    @cd evals && ONEUP_BENCH_BIN="$PWD/../target/debug/1up" bun run suites/1up-search/recall.ts

# Capture (move) the pinned recall baseline from a fresh run, with metadata.
# This is the ONLY sanctioned way to change recall-baseline.json; never
# regenerate it to make the gate pass (see evals/README.md). MODEL-ENABLED:
# manual DoD only, never in-agent.
eval-recall-baseline:
    cargo build --bin 1up
    ./target/debug/1up reindex . --exclude-glob 'evals/suites/1up-search/recall-*.json'
    @cd evals && RECALL_CAPTURE_BASELINE=1 ONEUP_BENCH_BIN="$PWD/../target/debug/1up" bun run suites/1up-search/recall.ts

# A/B recall parity: reindex + score the fp32 leg (captured as a temp baseline),
# then reindex + score the int8 leg gated against it within tolerance, exiting
# non-zero beyond it. Each leg runs `1up stop` first so a live daemon holding
# the other variant cannot serve query embeddings for the wrong leg, and
# `reindex` between legs is required because the model-identity gate fails
# closed on a variant change with existing vectors. The pinned
# recall-baseline.json is never touched (a temp file is used). MODEL-ENABLED:
# manual pre-merge DoD only (records INT8-vs-FP32 parity), never in-agent.
eval-recall-ab:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --bin 1up
    BIN="$PWD/target/debug/1up"
    AB_BASELINE="$(mktemp -t recall-ab-baseline.XXXXXX.json)"
    trap 'rm -f "$AB_BASELINE"' EXIT
    echo "== A/B leg 1: fp32 =="
    "$BIN" stop . || true
    ONEUP_MODEL_VARIANT=fp32 "$BIN" reindex . --exclude-glob 'evals/suites/1up-search/recall-*.json'
    (cd evals && RECALL_CAPTURE_BASELINE=1 ONEUP_MODEL_VARIANT=fp32 \
        ONEUP_RECALL_BASELINE_PATH="$AB_BASELINE" \
        ONEUP_BENCH_BIN="$BIN" bun run suites/1up-search/recall.ts)
    echo "== A/B leg 2: int8 (gated vs fp32 within tolerance) =="
    "$BIN" stop . || true
    ONEUP_MODEL_VARIANT=int8 "$BIN" reindex . --exclude-glob 'evals/suites/1up-search/recall-*.json'
    (cd evals && ONEUP_RECALL_AB=1 ONEUP_MODEL_VARIANT=int8 \
        ONEUP_RECALL_BASELINE_PATH="$AB_BASELINE" \
        ONEUP_BENCH_BIN="$BIN" bun run suites/1up-search/recall.ts)

# Exercise the local binary against a manifest URL.
update-test url="":
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ -z "{{url}}" ]]; then
      echo "usage: just update-test url=<manifest-url>"
      echo "example: just update-test url=http://127.0.0.1:8000/update-manifest.json"
      exit 0
    fi
    cargo build --bin 1up
    ONEUP_UPDATE_MANIFEST_URL="{{url}}" ./target/debug/1up update --check -f human
    echo
    ONEUP_UPDATE_MANIFEST_URL="{{url}}" ./target/debug/1up update --status -f human

# Reap orphaned build/test 1up daemons (keeps the installed ~/.local/bin daemon).
reap-daemons:
    #!/usr/bin/env bash
    set -euo pipefail
    pids="$(pgrep -fl '1up __worker' 2>/dev/null | grep -v '/.local/bin/1up' | awk '{print $1}' || true)"
    if [ -z "$pids" ]; then echo "no orphaned 1up daemons"; exit 0; fi
    echo "reaping orphaned 1up daemon(s): $(echo "$pids" | tr '\n' ' ')"
    # shellcheck disable=SC2086
    kill -TERM $pids 2>/dev/null || true
    sleep 1
    left="$(pgrep -fl '1up __worker' 2>/dev/null | grep -v '/.local/bin/1up' | awk '{print $1}' || true)"
    if [ -n "$left" ]; then
        # shellcheck disable=SC2086
        kill -KILL $left 2>/dev/null || true
    fi
    echo "done"
