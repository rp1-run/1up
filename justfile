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

# Run the deterministic recall@k harness against the current index.
# Builds the repo-local `1up` binary, indexes the repo with it, then runs the
# harness against that same binary so PATH-installed versions cannot mask
# regressions. Writes evals/suites/1up-search/recall-results.json.
eval-recall:
    cargo build --bin 1up
    ./target/debug/1up index .
    @cd evals && ONEUP_BENCH_BIN="$PWD/../target/debug/1up" bun run suites/1up-search/recall.ts

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
