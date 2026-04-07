# List available recipes
default:
    @just --list

# Build release binary and copy to ~/.local/bin
install:
    cargo build --release
    mkdir -p ~/.local/bin
    cp target/release/1up ~/.local/bin/1up
    codesign -f -s - ~/.local/bin/1up

bench-parallel repo='.':
    ./scripts/benchmark_parallel_indexing.sh {{repo}}

eval *flags:
    @cd evals && bun run eval; if echo "{{flags}}" | grep -q -- '--summary'; then just eval-summary; fi

# Run eval tests in parallel (separate promptfoo process per test)
eval-parallel *flags:
    @cd evals && ./run-parallel.sh; if echo "{{flags}}" | grep -q -- '--summary'; then just eval-summary; fi

eval-summary:
    @cd evals && ./summary.sh
