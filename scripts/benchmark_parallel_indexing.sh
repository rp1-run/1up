#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    printf 'missing required command: %s\n' "$1" >&2
    exit 1
  fi
}

log() {
  printf '[parallel-index-bench] %s\n' "$*" >&2
}

to_ms() {
  awk -v value="$1" 'BEGIN { printf "%.2f", value * 1000 }'
}

metric_value() {
  local json_path="$1"
  local result_index="$2"
  jq -r --argjson idx "$result_index" '.results[$idx].median' "$json_path"
}

sync_repo() {
  local source_dir="$1"
  local target_dir="$2"

  rm -rf "$target_dir"
  mkdir -p "$target_dir"
  rsync -a --delete --exclude .git --exclude .1up --exclude target "$source_dir"/ "$target_dir"/
}

run_full_case() {
  local source_dir="$1"
  local run_dir="$2"
  local oneup_bin="$3"
  local jobs="$4"
  local embed_threads="$5"

  sync_repo "$source_dir" "$run_dir"

  local args=("$oneup_bin" reindex "$run_dir")
  if [[ -n "$jobs" ]]; then
    args+=(--jobs "$jobs")
  fi
  if [[ -n "$embed_threads" ]]; then
    args+=(--embed-threads "$embed_threads")
  fi

  "${args[@]}" >/dev/null 2>&1
}

run_incremental_case() {
  local source_dir="$1"
  local run_dir="$2"
  local oneup_bin="$3"
  local jobs="$4"
  local embed_threads="$5"

  sync_repo "$source_dir" "$run_dir"

  cat > "$run_dir/_1up_parallel_bench.rs" <<'EOF'
pub fn bench_marker() -> &'static str {
    "parallel-index-bench-initial"
}
EOF

  local args=("$oneup_bin" index "$run_dir")
  if [[ -n "$jobs" ]]; then
    args+=(--jobs "$jobs")
  fi
  if [[ -n "$embed_threads" ]]; then
    args+=(--embed-threads "$embed_threads")
  fi

  "${args[@]}" >/dev/null 2>&1

  cat > "$run_dir/_1up_parallel_bench.rs" <<'EOF'
pub fn bench_marker() -> &'static str {
    "parallel-index-bench-updated"
}

pub fn bench_extra() -> usize {
    42
}
EOF

  "${args[@]}" >/dev/null 2>&1
}

if [[ "${1:-}" == "__run_full_case" ]]; then
  shift
  run_full_case "$@"
  exit 0
fi

if [[ "${1:-}" == "__run_incremental_case" ]]; then
  shift
  run_incremental_case "$@"
  exit 0
fi

REPO_INPUT="${1:-$ROOT_DIR}"

if [[ ! -d "$REPO_INPUT" ]]; then
  printf 'repo not found: %s\n' "$REPO_INPUT" >&2
  exit 1
fi

require_cmd cargo
require_cmd hyperfine
require_cmd jq
require_cmd rsync

REPO=$(cd "$REPO_INPUT" && pwd -P)
REPO_NAME=$(basename "$REPO")
TIMESTAMP=$(date +"%Y%m%d-%H%M%S")

RUNS="${RUNS:-5}"
WARMUP="${WARMUP:-1}"
SERIAL_JOBS="${SERIAL_JOBS:-1}"
SERIAL_EMBED_THREADS="${SERIAL_EMBED_THREADS:-1}"
CONSTRAINED_JOBS="${CONSTRAINED_JOBS:-2}"
CONSTRAINED_EMBED_THREADS="${CONSTRAINED_EMBED_THREADS:-1}"
ONEUP_BIN="${ONEUP_BIN:-$ROOT_DIR/target/release/1up}"
OUT_DIR="${OUT_DIR:-$ROOT_DIR/target/parallel-index-bench/${REPO_NAME}-${TIMESTAMP}}"
PRISTINE_DIR="$OUT_DIR/pristine"
RUN_DIR_ROOT="$OUT_DIR/runs"
FULL_JSON="$OUT_DIR/full-index.json"
INCREMENTAL_JSON="$OUT_DIR/incremental-index.json"
SUMMARY_JSON="$OUT_DIR/summary.json"

mkdir -p "$OUT_DIR" "$RUN_DIR_ROOT"

log "building release binary"
cargo build --release --bin 1up --manifest-path "$ROOT_DIR/Cargo.toml" >/dev/null

log "capturing benchmark snapshot from $REPO"
sync_repo "$REPO" "$PRISTINE_DIR"

log "warming indexing environment"
"$ONEUP_BIN" index "$PRISTINE_DIR" >/dev/null 2>&1
rm -rf "$PRISTINE_DIR/.1up"

log "benchmarking full reindex runs"
hyperfine \
  --export-json "$FULL_JSON" \
  --runs "$RUNS" \
  --warmup "$WARMUP" \
  "bash \"$0\" __run_full_case \"$PRISTINE_DIR\" \"$RUN_DIR_ROOT/full-serial\" \"$ONEUP_BIN\" \"$SERIAL_JOBS\" \"$SERIAL_EMBED_THREADS\"" \
  "bash \"$0\" __run_full_case \"$PRISTINE_DIR\" \"$RUN_DIR_ROOT/full-auto\" \"$ONEUP_BIN\" \"\" \"\"" \
  "bash \"$0\" __run_full_case \"$PRISTINE_DIR\" \"$RUN_DIR_ROOT/full-constrained\" \"$ONEUP_BIN\" \"$CONSTRAINED_JOBS\" \"$CONSTRAINED_EMBED_THREADS\""

log "benchmarking incremental reindex runs"
hyperfine \
  --export-json "$INCREMENTAL_JSON" \
  --runs "$RUNS" \
  --warmup "$WARMUP" \
  "bash \"$0\" __run_incremental_case \"$PRISTINE_DIR\" \"$RUN_DIR_ROOT/incremental-serial\" \"$ONEUP_BIN\" \"$SERIAL_JOBS\" \"$SERIAL_EMBED_THREADS\"" \
  "bash \"$0\" __run_incremental_case \"$PRISTINE_DIR\" \"$RUN_DIR_ROOT/incremental-auto\" \"$ONEUP_BIN\" \"\" \"\"" \
  "bash \"$0\" __run_incremental_case \"$PRISTINE_DIR\" \"$RUN_DIR_ROOT/incremental-constrained\" \"$ONEUP_BIN\" \"$CONSTRAINED_JOBS\" \"$CONSTRAINED_EMBED_THREADS\""

SERIAL_FULL_MS=$(to_ms "$(metric_value "$FULL_JSON" 0)")
AUTO_FULL_MS=$(to_ms "$(metric_value "$FULL_JSON" 1)")
CONSTRAINED_FULL_MS=$(to_ms "$(metric_value "$FULL_JSON" 2)")
SERIAL_INCREMENTAL_MS=$(to_ms "$(metric_value "$INCREMENTAL_JSON" 0)")
AUTO_INCREMENTAL_MS=$(to_ms "$(metric_value "$INCREMENTAL_JSON" 1)")
CONSTRAINED_INCREMENTAL_MS=$(to_ms "$(metric_value "$INCREMENTAL_JSON" 2)")

jq -n \
  --arg repo "$REPO" \
  --arg out_dir "$OUT_DIR" \
  --arg serial_full_ms "$SERIAL_FULL_MS" \
  --arg auto_full_ms "$AUTO_FULL_MS" \
  --arg constrained_full_ms "$CONSTRAINED_FULL_MS" \
  --arg serial_incremental_ms "$SERIAL_INCREMENTAL_MS" \
  --arg auto_incremental_ms "$AUTO_INCREMENTAL_MS" \
  --arg constrained_incremental_ms "$CONSTRAINED_INCREMENTAL_MS" \
  --argjson runs "$RUNS" \
  --argjson warmup "$WARMUP" \
  --argjson serial_jobs "$SERIAL_JOBS" \
  --argjson serial_embed_threads "$SERIAL_EMBED_THREADS" \
  --argjson constrained_jobs "$CONSTRAINED_JOBS" \
  --argjson constrained_embed_threads "$CONSTRAINED_EMBED_THREADS" \
  '{
    repo: $repo,
    out_dir: $out_dir,
    runs: $runs,
    warmup: $warmup,
    full_index_median_ms: {
      serial: ($serial_full_ms | tonumber),
      auto: ($auto_full_ms | tonumber),
      constrained: ($constrained_full_ms | tonumber)
    },
    incremental_index_median_ms: {
      serial: ($serial_incremental_ms | tonumber),
      auto: ($auto_incremental_ms | tonumber),
      constrained: ($constrained_incremental_ms | tonumber)
    },
    configs: {
      serial: {
        jobs: $serial_jobs,
        embed_threads: $serial_embed_threads
      },
      auto: {
        jobs: null,
        embed_threads: null
      },
      constrained: {
        jobs: $constrained_jobs,
        embed_threads: $constrained_embed_threads
      }
    }
  }' > "$SUMMARY_JSON"

printf 'Parallel indexing benchmark complete.\n'
printf 'Repository: %s\n' "$REPO"
printf 'Output: %s\n' "$OUT_DIR"
printf 'Full reindex median ms: serial=%s auto=%s constrained=%s\n' \
  "$SERIAL_FULL_MS" "$AUTO_FULL_MS" "$CONSTRAINED_FULL_MS"
printf 'Incremental index median ms: serial=%s auto=%s constrained=%s\n' \
  "$SERIAL_INCREMENTAL_MS" "$AUTO_INCREMENTAL_MS" "$CONSTRAINED_INCREMENTAL_MS"
