# Build release binary and copy to ~/.local/bin
install:
    cargo build --release
    mkdir -p ~/.local/bin
    cp target/release/1up ~/.local/bin/1up
    codesign -f -s - ~/.local/bin/1up

bench-parallel repo='.':
    ./scripts/benchmark_parallel_indexing.sh {{repo}}

eval:
    cd evals && bun run eval
