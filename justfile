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

impact-eval *flags:
    ./scripts/evaluate_impact_trust.sh {{flags}}

impact-bench *flags:
    ./scripts/benchmark_impact.sh {{flags}}

impact-rollout-approve *flags:
    ./scripts/approve_impact_rollout.sh {{flags}}

bench-parallel:
    ./scripts/benchmark_parallel_indexing.sh

security-check:
    ./scripts/security_check.sh

eval *flags:
    @cd evals && bun run eval; if echo "{{flags}}" | grep -q -- '--summary'; then just eval-summary; fi

# Run eval tests in parallel (separate promptfoo process per test)
eval-parallel *flags:
    @cd evals && ./run-parallel.sh; if echo "{{flags}}" | grep -q -- '--summary'; then just eval-summary; fi

eval-summary:
    @cd evals && ./summary.sh

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
