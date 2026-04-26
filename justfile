# List available recipes
default:
    @just --list

# Build release binary and copy to ~/.local/bin
install:
    cargo build --release
    mkdir -p ~/.local/bin
    cp target/release/1up ~/.local/bin/1up
    codesign -f -s - ~/.local/bin/1up

bench:
    cargo build --release
    @cd evals && ONEUP_BENCH_BIN=../target/release/1up bun run bench

bench-parallel:
    ./scripts/benchmark_parallel_indexing.sh

# Size + throughput guard for schema v12 HNSW index (shrink-hnsw-vector-index).
# Fresh-reindexes the 1up repo and gates against REQ-001 (index.db <= 80 MiB)
# and REQ-003 (indexing_ms in [72900, 89100]). Pinned baseline for delta
# reporting lives at scripts/baselines/vector_index_size_baseline.json.
bench-vector-index-size *flags:
    ./scripts/benchmark_vector_index_size.sh {{flags}}

security-check:
    ./scripts/security_check.sh

# Local verification gate. Runs formatter, linter, the full test surface,
# and the install-script lint/smoke introduced by the update-script feature
# (T4). Uses `cargo test` (no per-suite filters) so any integration test
# crate added later -- including the security gate's existing targets -- is
# picked up automatically and verify cannot pass while CI fails.
verify:
    cargo fmt --all -- --check
    cargo clippy --all-targets -- -D warnings
    cargo test
    shellcheck --severity=style scripts/install/setup.sh
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
