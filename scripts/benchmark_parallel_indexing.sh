#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
EMDASH_REPO="https://github.com/emdash-cms/emdash.git"
EMDASH_COMMIT="5beb0ddc334deb862ba90cedbf03f052b58e4974"
EMDASH_CACHE_DIR="$ROOT_DIR/evals/.cache/emdash"

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    printf 'missing required command: %s\n' "$1" >&2
    exit 1
  fi
}

log() {
  printf '[parallel-index-bench] %s\n' "$*" >&2
}

fail() {
  log "$*"
  exit 1
}

write_skipped_summary() {
  local reason="$1"
  local out_dir="$2"
  local summary_json="$out_dir/summary.json"

  mkdir -p "$out_dir"
  jq -n \
    --arg reason "$reason" \
    '{
      status: "skipped",
      evidence_type: "retained_performance",
      skipped_reason: $reason,
      manual_readiness_note_required: true,
      retained_performance_outcomes: {
        semantic_indexing: {
          status: "skipped",
          command: "just bench-parallel"
        },
        incremental_indexing: {
          status: "skipped",
          command: "just bench-parallel"
        },
        daemon_refresh: {
          status: "skipped",
          command: "just bench-parallel"
        },
        search_latency: {
          status: "separate_evidence",
          command: "just bench-search-latency"
        }
      }
    }' > "$summary_json"

  cat "$summary_json"
  printf '\nParallel indexing benchmark skipped.\n'
  printf 'Output: %s\n' "$summary_json"
}

to_ms() {
  awk -v value="$1" 'BEGIN { printf "%.2f", value * 1000 }'
}

compute_median_ms() {
  local -a sorted
  IFS=$'\n' read -r -d '' -a sorted < <(printf '%s\n' "$@" | sort -n; printf '\0')
  local count=${#sorted[@]}
  if (( count == 0 )); then
    printf '0.00'
    return
  fi
  local mid=$(( count / 2 ))
  if (( count % 2 == 1 )); then
    printf '%s' "${sorted[$mid]}"
  else
    awk -v a="${sorted[$((mid - 1))]}" -v b="${sorted[$mid]}" 'BEGIN { printf "%.2f", (a + b) / 2 }'
  fi
}

metric_value() {
  local json_path="$1"
  local result_index="$2"
  jq -r --argjson idx "$result_index" '.results[$idx].median' "$json_path"
}

config_jobs() {
  case "$1" in
    serial) printf '%s' "$SERIAL_JOBS" ;;
    constrained) printf '%s' "$CONSTRAINED_JOBS" ;;
    auto) ;;
  esac
}

config_embed_threads() {
  case "$1" in
    serial) printf '%s' "$SERIAL_EMBED_THREADS" ;;
    constrained) printf '%s' "$CONSTRAINED_EMBED_THREADS" ;;
    auto) ;;
  esac
}

# Median for one full-reindex config as a JSON value: a number in ms when the
# config was selected via BENCH_FULL_CONFIGS, otherwise `null`.
full_metric_ms() {
  local config="$1"
  local idx=0
  local selected
  for selected in "${FULL_CONFIGS[@]}"; do
    if [[ "$selected" == "$config" ]]; then
      to_ms "$(metric_value "$FULL_JSON" "$idx")"
      return
    fi
    idx=$((idx + 1))
  done
  printf 'null'
}

require_index_work() {
  local label="$1"
  local output_json="$2"

  if jq -e '.progress.files_indexed > 0 and .progress.segments_stored > 0' >/dev/null <<<"$output_json"; then
    return 0
  fi

  local files_indexed
  local segments_stored
  files_indexed=$(jq -r '.progress.files_indexed // 0' <<<"$output_json")
  segments_stored=$(jq -r '.progress.segments_stored // 0' <<<"$output_json")
  printf 'benchmark run %s produced no indexed work (files_indexed=%s, segments_stored=%s)\n' \
    "$label" "$files_indexed" "$segments_stored" >&2
  return 1
}

sync_repo() {
  local source_dir="$1"
  local target_dir="$2"

  rm -rf "$target_dir"
  mkdir -p "$target_dir"
  rsync -a --delete --exclude .git --exclude .1up --exclude target "$source_dir"/ "$target_dir"/
  # project resolution refuses auto-init outside a git root or existing 1up
  # project, so benchmark copies need a git marker
  mkdir -p "$target_dir/.git"
}

ensure_emdash_fixture() {
  mkdir -p "$(dirname "$EMDASH_CACHE_DIR")"

  if [[ ! -d "$EMDASH_CACHE_DIR/.git" ]]; then
    log "cloning pinned emdash fixture"
    git clone --single-branch --branch main "$EMDASH_REPO" "$EMDASH_CACHE_DIR" >/dev/null 2>&1
  fi

  log "checking out pinned emdash commit"
  git -C "$EMDASH_CACHE_DIR" fetch --depth 1 origin "$EMDASH_COMMIT" >/dev/null 2>&1 || true
  git -C "$EMDASH_CACHE_DIR" checkout --force "$EMDASH_COMMIT" >/dev/null 2>&1
}

# Snapshot/restore keeps incremental benchmarks honest without paying for a
# full reindex in every hyperfine --prepare: the base index is built once per
# config, the whole run dir (including .1up state) is copied to a pristine
# snapshot, and each prepare restores that snapshot into the same absolute
# run dir so paths stored in the index stay valid.
snapshot_state() {
  local run_dir="$1"
  local snapshot_dir="$2"

  rm -rf "$snapshot_dir"
  mkdir -p "$snapshot_dir"
  rsync -a --delete "$run_dir"/ "$snapshot_dir"/
}

restore_state() {
  local snapshot_dir="$1"
  local run_dir="$2"

  mkdir -p "$run_dir"
  rsync -a --delete "$snapshot_dir"/ "$run_dir"/
}

prepare_full_case() {
  local source_dir="$1"
  local run_dir="$2"

  sync_repo "$source_dir" "$run_dir"
}

run_full_case() {
  local run_dir="$1"
  local oneup_bin="$2"
  local jobs="$3"
  local embed_threads="$4"

  local args=("$oneup_bin" reindex "$run_dir" --format json)
  if [[ -n "$jobs" ]]; then
    args+=(--jobs "$jobs")
  fi
  if [[ -n "$embed_threads" ]]; then
    args+=(--embed-threads "$embed_threads")
  fi

  local output
  output=$("${args[@]}")
  require_index_work "full:$run_dir" "$output"
  capture_telemetry "full:$run_dir" "$output"
}

prepare_incremental_base() {
  local source_dir="$1"
  local run_dir="$2"
  local snapshot_dir="$3"
  local oneup_bin="$4"
  local jobs="$5"
  local embed_threads="$6"

  sync_repo "$source_dir" "$run_dir"

  cat > "$run_dir/_1up_parallel_bench.rs" <<'EOF'
pub fn bench_marker() -> &'static str {
    "parallel-index-bench-initial"
}
EOF

  local args=("$oneup_bin" index "$run_dir" --format json)
  if [[ -n "$jobs" ]]; then
    args+=(--jobs "$jobs")
  fi
  if [[ -n "$embed_threads" ]]; then
    args+=(--embed-threads "$embed_threads")
  fi

  local output
  output=$("${args[@]}")
  require_index_work "prepare-incremental:$run_dir" "$output"

  snapshot_state "$run_dir" "$snapshot_dir"
}

restore_incremental_case() {
  local snapshot_dir="$1"
  local run_dir="$2"

  restore_state "$snapshot_dir" "$run_dir"

  cat > "$run_dir/_1up_parallel_bench.rs" <<'EOF'
pub fn bench_marker() -> &'static str {
    "parallel-index-bench-updated"
}

pub fn bench_extra() -> usize {
    42
}
EOF
}

run_incremental_case() {
  local run_dir="$1"
  local oneup_bin="$2"
  local jobs="$3"
  local embed_threads="$4"

  local args=("$oneup_bin" index "$run_dir" --format json)
  if [[ -n "$jobs" ]]; then
    args+=(--jobs "$jobs")
  fi
  if [[ -n "$embed_threads" ]]; then
    args+=(--embed-threads "$embed_threads")
  fi

  local output
  output=$("${args[@]}")
  require_index_work "incremental:$run_dir" "$output"
  capture_telemetry "incremental:$run_dir" "$output"
}

prepare_write_heavy_base() {
  local source_dir="$1"
  local run_dir="$2"
  local snapshot_dir="$3"
  local oneup_bin="$4"
  local jobs="$5"
  local embed_threads="$6"

  sync_repo "$source_dir" "$run_dir"
  mkdir -p "$run_dir/write_heavy"

  local idx
  for idx in $(seq 1 24); do
    cat > "$run_dir/write_heavy/file_${idx}.rs" <<EOF
pub fn write_heavy_marker_${idx}() -> &'static str {
    "write-heavy-initial-${idx}"
}
EOF
  done

  local args=("$oneup_bin" index "$run_dir" --format json)
  if [[ -n "$jobs" ]]; then
    args+=(--jobs "$jobs")
  fi
  if [[ -n "$embed_threads" ]]; then
    args+=(--embed-threads "$embed_threads")
  fi

  local output
  output=$("${args[@]}")
  require_index_work "prepare-write-heavy:$run_dir" "$output"

  snapshot_state "$run_dir" "$snapshot_dir"
}

restore_write_heavy_case() {
  local snapshot_dir="$1"
  local run_dir="$2"

  restore_state "$snapshot_dir" "$run_dir"

  local idx
  for idx in $(seq 1 24); do
    cat > "$run_dir/write_heavy/file_${idx}.rs" <<EOF
pub fn write_heavy_marker_${idx}() -> &'static str {
    "write-heavy-updated-${idx}"
}

pub fn write_heavy_counter_${idx}() -> usize {
    ${idx}
}
EOF
  done
}

run_write_heavy_case() {
  local run_dir="$1"
  local oneup_bin="$2"
  local jobs="$3"
  local embed_threads="$4"

  local args=("$oneup_bin" index "$run_dir" --format json)
  if [[ -n "$jobs" ]]; then
    args+=(--jobs "$jobs")
  fi
  if [[ -n "$embed_threads" ]]; then
    args+=(--embed-threads "$embed_threads")
  fi

  local output
  output=$("${args[@]}")
  require_index_work "write-heavy:$run_dir" "$output"
  capture_telemetry "write-heavy:$run_dir" "$output"
}

wait_for_daemon_ready() {
  local run_dir="$1"
  local oneup_bin="$2"
  local max_wait=60
  local elapsed=0

  while (( elapsed < max_wait )); do
    local status_json
    status_json=$("$oneup_bin" status "$run_dir" --format json 2>/dev/null || true)
    local daemon_running
    daemon_running=$(jq -r '.daemon_running // false' <<<"$status_json" 2>/dev/null || echo "false")
    local last_check
    last_check=$(jq -r '.last_file_check_at // "null"' <<<"$status_json" 2>/dev/null || echo "null")

    if [[ "$daemon_running" == "true" && "$last_check" != "null" ]]; then
      return 0
    fi

    sleep 1
    elapsed=$((elapsed + 1))
  done

  printf 'daemon did not become ready within %ds for %s\n' "$max_wait" "$run_dir" >&2
  return 1
}

wait_for_refresh_complete() {
  local run_dir="$1"
  local oneup_bin="$2"
  local baseline_updated_at="$3"
  local max_wait=120
  local elapsed=0

  while (( elapsed < max_wait )); do
    local status_json
    status_json=$("$oneup_bin" status "$run_dir" --format json 2>/dev/null || true)
    local state
    state=$(jq -r '.index_progress.state // "idle"' <<<"$status_json" 2>/dev/null || echo "idle")
    local updated_at
    updated_at=$(jq -r '.index_progress.updated_at // "null"' <<<"$status_json" 2>/dev/null || echo "null")

    if [[ "$state" == "complete" && "$updated_at" != "null" && "$updated_at" != "$baseline_updated_at" ]]; then
      printf '%s' "$status_json"
      return 0
    fi

    sleep 1
    elapsed=$((elapsed + 1))
  done

  printf 'daemon refresh did not complete within %ds for %s\n' "$max_wait" "$run_dir" >&2
  return 1
}

run_daemon_refresh_benchmark() {
  local source_dir="$1"
  local run_dir="$2"
  local oneup_bin="$3"
  local runs="$4"

  log "setting up daemon refresh benchmark"
  sync_repo "$source_dir" "$run_dir"

  "$oneup_bin" start "$run_dir" >/dev/null 2>&1
  wait_for_daemon_ready "$run_dir" "$oneup_bin"

  sleep 2

  local -a times_ms=()
  local iter

  for iter in $(seq 1 "$runs"); do
    local status_json
    status_json=$("$oneup_bin" status "$run_dir" --format json 2>/dev/null || true)
    local baseline_updated_at
    baseline_updated_at=$(jq -r '.index_progress.updated_at // "null"' <<<"$status_json" 2>/dev/null || echo "null")

    cat > "$run_dir/_1up_daemon_bench_${iter}.rs" <<EOF
pub fn daemon_bench_marker_${iter}() -> &'static str {
    "daemon-refresh-iteration-${iter}"
}
EOF

    local start_ns
    start_ns=$(perl -MTime::HiRes=time -e 'printf "%.0f\n", time * 1000')

    local refresh_json
    refresh_json=$(wait_for_refresh_complete "$run_dir" "$oneup_bin" "$baseline_updated_at")

    local end_ns
    end_ns=$(perl -MTime::HiRes=time -e 'printf "%.0f\n", time * 1000')

    local elapsed_ms
    elapsed_ms=$(awk -v s="$start_ns" -v e="$end_ns" 'BEGIN { printf "%.2f", e - s }')
    times_ms+=("$elapsed_ms")

    log "daemon refresh iteration ${iter}/${runs}: ${elapsed_ms}ms"

    if [[ -n "${TELEMETRY_DIR:-}" && -n "$refresh_json" ]]; then
      local safe_label
      safe_label=$(printf '%s' "daemon-refresh__iter-${iter}" | tr '/:' '__')
      local telemetry_file="$TELEMETRY_DIR/${safe_label}.json"
      jq -n \
        --arg label "daemon-refresh:iter-${iter}" \
        --argjson scope "$(jq '.index_progress.scope // null' <<<"$refresh_json")" \
        --argjson prefilter "$(jq '.index_progress.prefilter // null' <<<"$refresh_json")" \
        --argjson timings "$(jq '.index_progress.timings // null' <<<"$refresh_json")" \
        '{label: $label, scope: $scope, prefilter: $prefilter, timings: $timings}' \
        > "$telemetry_file"
    fi

    sleep 1
  done

  log "stopping daemon for benchmark cleanup"
  "$oneup_bin" stop "$run_dir" >/dev/null 2>&1 || true
  sleep 1

  local median
  median=$(compute_median_ms "${times_ms[@]}")
  printf '%s' "$median"
}

TELEMETRY_DIR=""

capture_telemetry() {
  local label="$1"
  local output_json="$2"

  if [[ -z "$TELEMETRY_DIR" ]]; then
    return 0
  fi

  local safe_label
  safe_label=$(printf '%s' "$label" | tr '/:' '__')
  local ts
  ts=$(date +%s%N 2>/dev/null || perl -MTime::HiRes=time -e 'printf "%.0f\n", time * 1e9')
  local telemetry_file="$TELEMETRY_DIR/${safe_label}_${ts}.json"

  jq -n \
    --arg label "$label" \
    --argjson scope "$(jq '.progress.scope // null' <<<"$output_json")" \
    --argjson prefilter "$(jq '.progress.prefilter // null' <<<"$output_json")" \
    --argjson timings "$(jq '.progress.timings // null' <<<"$output_json")" \
    '{label: $label, scope: $scope, prefilter: $prefilter, timings: $timings}' \
    > "$telemetry_file"
}

if [[ "${1:-}" == "__prepare_full_case" ]]; then
  shift
  prepare_full_case "$@"
  exit 0
fi

if [[ "${1:-}" == "__run_full_case" ]]; then
  shift
  run_full_case "$@"
  exit 0
fi

if [[ "${1:-}" == "__restore_incremental_case" ]]; then
  shift
  restore_incremental_case "$@"
  exit 0
fi

if [[ "${1:-}" == "__run_incremental_case" ]]; then
  shift
  run_incremental_case "$@"
  exit 0
fi

if [[ "${1:-}" == "__restore_write_heavy_case" ]]; then
  shift
  restore_write_heavy_case "$@"
  exit 0
fi

if [[ "${1:-}" == "__run_write_heavy_case" ]]; then
  shift
  run_write_heavy_case "$@"
  exit 0
fi

REPO_INPUT="${1:-$EMDASH_CACHE_DIR}"
if [[ -d "$REPO_INPUT" ]]; then
  REPO_NAME=$(basename "$(cd "$REPO_INPUT" && pwd -P)")
else
  REPO_NAME=$(basename "$REPO_INPUT")
fi
TIMESTAMP=$(date +"%Y%m%d-%H%M%S")

RUNS="${RUNS:-5}"
WARMUP="${WARMUP:-1}"
SERIAL_JOBS="${SERIAL_JOBS:-1}"
SERIAL_EMBED_THREADS="${SERIAL_EMBED_THREADS:-1}"
CONSTRAINED_JOBS="${CONSTRAINED_JOBS:-2}"
CONSTRAINED_EMBED_THREADS="${CONSTRAINED_EMBED_THREADS:-1}"
# Comma-separated subset of serial,auto,constrained for the full-reindex
# matrix (the most expensive case). CI narrows this to `auto`; local
# `just bench-parallel` keeps all three by default.
BENCH_FULL_CONFIGS="${BENCH_FULL_CONFIGS:-serial,auto,constrained}"
# When ONEUP_BIN is provided (for example a verified release binary in CI),
# the script benchmarks it directly instead of building from source.
ONEUP_BIN_PROVIDED="${ONEUP_BIN:-}"
ONEUP_BIN="${ONEUP_BIN:-$ROOT_DIR/target/release/1up}"
OUT_DIR="${OUT_DIR:-$ROOT_DIR/target/parallel-index-bench/${REPO_NAME}-${TIMESTAMP}}"
PRISTINE_DIR="$OUT_DIR/pristine"
RUN_DIR_ROOT="$OUT_DIR/runs"
SNAPSHOT_DIR_ROOT="$OUT_DIR/snapshots"
FULL_JSON="$OUT_DIR/full-index.json"
INCREMENTAL_JSON="$OUT_DIR/incremental-index.json"
WRITE_HEAVY_JSON="$OUT_DIR/write-heavy-index.json"
SUMMARY_JSON="$OUT_DIR/summary.json"

BENCHMARK_SKIPPED_REASON="${BENCHMARK_SKIPPED_REASON:-${SKIP_REASON:-}}"
if [[ -n "${BENCHMARK_SKIPPED_REASON//[[:space:]]/}" ]]; then
  require_cmd jq
  write_skipped_summary "$BENCHMARK_SKIPPED_REASON" "$OUT_DIR"
  exit 0
fi

require_cmd git
require_cmd hyperfine
require_cmd jq
require_cmd perl
require_cmd rsync

FULL_CONFIGS=()
IFS=',' read -r -a requested_full_configs <<<"$BENCH_FULL_CONFIGS"
for config in "${requested_full_configs[@]}"; do
  config="${config//[[:space:]]/}"
  case "$config" in
    serial|auto|constrained) FULL_CONFIGS+=("$config") ;;
    '') ;;
    *) fail "invalid BENCH_FULL_CONFIGS entry '$config' (expected serial, auto, constrained)" ;;
  esac
done
if (( ${#FULL_CONFIGS[@]} == 0 )); then
  fail "BENCH_FULL_CONFIGS selected no configs: $BENCH_FULL_CONFIGS"
fi

if [[ "$REPO_INPUT" == "$EMDASH_CACHE_DIR" ]]; then
  ensure_emdash_fixture
fi

if [[ ! -d "$REPO_INPUT" ]]; then
  printf 'repo not found: %s\n' "$REPO_INPUT" >&2
  exit 1
fi

REPO=$(cd "$REPO_INPUT" && pwd -P)
REPO_NAME=$(basename "$REPO")

TELEMETRY_DIR="$OUT_DIR/telemetry"
mkdir -p "$OUT_DIR" "$RUN_DIR_ROOT" "$SNAPSHOT_DIR_ROOT" "$TELEMETRY_DIR"

if [[ -n "$ONEUP_BIN_PROVIDED" ]]; then
  if [[ ! -x "$ONEUP_BIN" ]]; then
    fail "ONEUP_BIN is not an executable file: $ONEUP_BIN"
  fi
  log "benchmarking provided binary: $ONEUP_BIN"
else
  require_cmd cargo
  log "building release binary"
  cargo build --release --bin 1up --manifest-path "$ROOT_DIR/Cargo.toml" >/dev/null
fi

log "capturing benchmark snapshot from $REPO"
sync_repo "$REPO" "$PRISTINE_DIR"

log "warming indexing environment"
"$ONEUP_BIN" index "$PRISTINE_DIR" >/dev/null
rm -rf "$PRISTINE_DIR/.1up"

log "benchmarking full reindex runs (configs: ${FULL_CONFIGS[*]})"
full_bench_args=(--export-json "$FULL_JSON" --runs "$RUNS" --warmup "$WARMUP")
for config in "${FULL_CONFIGS[@]}"; do
  full_run_dir="$RUN_DIR_ROOT/full-$config"
  full_bench_args+=(
    --prepare "bash \"$0\" __prepare_full_case \"$PRISTINE_DIR\" \"$full_run_dir\""
    "bash \"$0\" __run_full_case \"$full_run_dir\" \"$ONEUP_BIN\" \"$(config_jobs "$config")\" \"$(config_embed_threads "$config")\""
  )
done
hyperfine "${full_bench_args[@]}"

log "preparing incremental benchmark base indexes"
for config in serial auto constrained; do
  prepare_incremental_base \
    "$PRISTINE_DIR" \
    "$RUN_DIR_ROOT/incremental-$config" \
    "$SNAPSHOT_DIR_ROOT/incremental-$config" \
    "$ONEUP_BIN" \
    "$(config_jobs "$config")" \
    "$(config_embed_threads "$config")"
done

log "benchmarking incremental reindex runs"
hyperfine \
  --export-json "$INCREMENTAL_JSON" \
  --runs "$RUNS" \
  --warmup "$WARMUP" \
  --prepare "bash \"$0\" __restore_incremental_case \"$SNAPSHOT_DIR_ROOT/incremental-serial\" \"$RUN_DIR_ROOT/incremental-serial\"" \
  "bash \"$0\" __run_incremental_case \"$RUN_DIR_ROOT/incremental-serial\" \"$ONEUP_BIN\" \"$SERIAL_JOBS\" \"$SERIAL_EMBED_THREADS\"" \
  --prepare "bash \"$0\" __restore_incremental_case \"$SNAPSHOT_DIR_ROOT/incremental-auto\" \"$RUN_DIR_ROOT/incremental-auto\"" \
  "bash \"$0\" __run_incremental_case \"$RUN_DIR_ROOT/incremental-auto\" \"$ONEUP_BIN\" \"\" \"\"" \
  --prepare "bash \"$0\" __restore_incremental_case \"$SNAPSHOT_DIR_ROOT/incremental-constrained\" \"$RUN_DIR_ROOT/incremental-constrained\"" \
  "bash \"$0\" __run_incremental_case \"$RUN_DIR_ROOT/incremental-constrained\" \"$ONEUP_BIN\" \"$CONSTRAINED_JOBS\" \"$CONSTRAINED_EMBED_THREADS\""

log "preparing write-heavy benchmark base indexes"
for config in serial auto constrained; do
  prepare_write_heavy_base \
    "$PRISTINE_DIR" \
    "$RUN_DIR_ROOT/write-heavy-$config" \
    "$SNAPSHOT_DIR_ROOT/write-heavy-$config" \
    "$ONEUP_BIN" \
    "$(config_jobs "$config")" \
    "$(config_embed_threads "$config")"
done

log "benchmarking write-heavy incremental runs"
hyperfine \
  --export-json "$WRITE_HEAVY_JSON" \
  --runs "$RUNS" \
  --warmup "$WARMUP" \
  --prepare "bash \"$0\" __restore_write_heavy_case \"$SNAPSHOT_DIR_ROOT/write-heavy-serial\" \"$RUN_DIR_ROOT/write-heavy-serial\"" \
  "bash \"$0\" __run_write_heavy_case \"$RUN_DIR_ROOT/write-heavy-serial\" \"$ONEUP_BIN\" \"$SERIAL_JOBS\" \"$SERIAL_EMBED_THREADS\"" \
  --prepare "bash \"$0\" __restore_write_heavy_case \"$SNAPSHOT_DIR_ROOT/write-heavy-auto\" \"$RUN_DIR_ROOT/write-heavy-auto\"" \
  "bash \"$0\" __run_write_heavy_case \"$RUN_DIR_ROOT/write-heavy-auto\" \"$ONEUP_BIN\" \"\" \"\"" \
  --prepare "bash \"$0\" __restore_write_heavy_case \"$SNAPSHOT_DIR_ROOT/write-heavy-constrained\" \"$RUN_DIR_ROOT/write-heavy-constrained\"" \
  "bash \"$0\" __run_write_heavy_case \"$RUN_DIR_ROOT/write-heavy-constrained\" \"$ONEUP_BIN\" \"$CONSTRAINED_JOBS\" \"$CONSTRAINED_EMBED_THREADS\""

log "benchmarking daemon refresh cycles"
DAEMON_REFRESH_MEDIAN_MS=$(run_daemon_refresh_benchmark "$PRISTINE_DIR" "$RUN_DIR_ROOT/daemon-refresh" "$ONEUP_BIN" "$RUNS")

SERIAL_FULL_MS=$(full_metric_ms serial)
AUTO_FULL_MS=$(full_metric_ms auto)
CONSTRAINED_FULL_MS=$(full_metric_ms constrained)
SERIAL_INCREMENTAL_MS=$(to_ms "$(metric_value "$INCREMENTAL_JSON" 0)")
AUTO_INCREMENTAL_MS=$(to_ms "$(metric_value "$INCREMENTAL_JSON" 1)")
CONSTRAINED_INCREMENTAL_MS=$(to_ms "$(metric_value "$INCREMENTAL_JSON" 2)")
SERIAL_WRITE_HEAVY_MS=$(to_ms "$(metric_value "$WRITE_HEAVY_JSON" 0)")
AUTO_WRITE_HEAVY_MS=$(to_ms "$(metric_value "$WRITE_HEAVY_JSON" 1)")
CONSTRAINED_WRITE_HEAVY_MS=$(to_ms "$(metric_value "$WRITE_HEAVY_JSON" 2)")

# Collect telemetry from captured run outputs
TELEMETRY_JSON="[]"
if [[ -d "$TELEMETRY_DIR" ]] && compgen -G "$TELEMETRY_DIR/*.json" >/dev/null 2>&1; then
  TELEMETRY_JSON=$(jq -s '.' "$TELEMETRY_DIR"/*.json 2>/dev/null || echo '[]')
fi

# Count scope fallbacks and executed-scope distribution from telemetry
FALLBACK_COUNT=$(printf '%s' "$TELEMETRY_JSON" | jq '[.[] | select(.scope.fallback_reason != null)] | length')
SCOPED_COUNT=$(printf '%s' "$TELEMETRY_JSON" | jq '[.[] | select(.scope.executed | startswith("scoped"))] | length')
FULL_COUNT=$(printf '%s' "$TELEMETRY_JSON" | jq '[.[] | select(.scope.executed == "full")] | length')

# Count prefilter work distribution. Full-run metadata skips are expected on
# unchanged refreshes and indicate files avoided content reads before parsing.
PREFILTER_DISCOVERED_TOTAL=$(printf '%s' "$TELEMETRY_JSON" | jq '[.[] | (.prefilter.discovered // 0)] | add // 0')
PREFILTER_METADATA_SKIPPED_TOTAL=$(printf '%s' "$TELEMETRY_JSON" | jq '[.[] | (.prefilter.metadata_skipped // 0)] | add // 0')
PREFILTER_CONTENT_READ_TOTAL=$(printf '%s' "$TELEMETRY_JSON" | jq '[.[] | (.prefilter.content_read // 0)] | add // 0')
PREFILTER_DELETED_TOTAL=$(printf '%s' "$TELEMETRY_JSON" | jq '[.[] | (.prefilter.deleted // 0)] | add // 0')
FULL_METADATA_SKIPPED_TOTAL=$(printf '%s' "$TELEMETRY_JSON" | jq '[.[] | select(.label | startswith("full:")) | (.prefilter.metadata_skipped // 0)] | add // 0')
FULL_CONTENT_READ_TOTAL=$(printf '%s' "$TELEMETRY_JSON" | jq '[.[] | select(.label | startswith("full:")) | (.prefilter.content_read // 0)] | add // 0')

jq -n \
  --arg repo "$REPO" \
  --arg out_dir "$OUT_DIR" \
  --arg search_latency_command "just bench-search-latency" \
  --argjson serial_full_ms "$SERIAL_FULL_MS" \
  --argjson auto_full_ms "$AUTO_FULL_MS" \
  --argjson constrained_full_ms "$CONSTRAINED_FULL_MS" \
  --arg full_configs_csv "$(IFS=','; printf '%s' "${FULL_CONFIGS[*]}")" \
  --arg serial_incremental_ms "$SERIAL_INCREMENTAL_MS" \
  --arg auto_incremental_ms "$AUTO_INCREMENTAL_MS" \
  --arg constrained_incremental_ms "$CONSTRAINED_INCREMENTAL_MS" \
  --arg serial_write_heavy_ms "$SERIAL_WRITE_HEAVY_MS" \
  --arg auto_write_heavy_ms "$AUTO_WRITE_HEAVY_MS" \
  --arg constrained_write_heavy_ms "$CONSTRAINED_WRITE_HEAVY_MS" \
  --arg daemon_refresh_ms "$DAEMON_REFRESH_MEDIAN_MS" \
  --argjson runs "$RUNS" \
  --argjson warmup "$WARMUP" \
  --argjson serial_jobs "$SERIAL_JOBS" \
  --argjson serial_embed_threads "$SERIAL_EMBED_THREADS" \
  --argjson constrained_jobs "$CONSTRAINED_JOBS" \
  --argjson constrained_embed_threads "$CONSTRAINED_EMBED_THREADS" \
  --argjson fallback_count "$FALLBACK_COUNT" \
  --argjson scoped_count "$SCOPED_COUNT" \
  --argjson full_count "$FULL_COUNT" \
  --argjson prefilter_discovered_total "$PREFILTER_DISCOVERED_TOTAL" \
  --argjson prefilter_metadata_skipped_total "$PREFILTER_METADATA_SKIPPED_TOTAL" \
  --argjson prefilter_content_read_total "$PREFILTER_CONTENT_READ_TOTAL" \
  --argjson prefilter_deleted_total "$PREFILTER_DELETED_TOTAL" \
  --argjson full_metadata_skipped_total "$FULL_METADATA_SKIPPED_TOTAL" \
  --argjson full_content_read_total "$FULL_CONTENT_READ_TOTAL" \
  --argjson telemetry "$TELEMETRY_JSON" \
  '{
    status: "recorded",
    evidence_type: "retained_performance",
    repo: $repo,
    out_dir: $out_dir,
    runs: $runs,
    warmup: $warmup,
    retained_performance_outcomes: {
      semantic_indexing: {
        status: "recorded",
        metric: "full_index_median_ms",
        command: "just bench-parallel"
      },
      incremental_indexing: {
        status: "recorded",
        metric: "incremental_index_median_ms",
        command: "just bench-parallel"
      },
      daemon_refresh: {
        status: "recorded",
        metric: "daemon_refresh_median_ms",
        command: "just bench-parallel"
      },
      search_latency: {
        status: "separate_evidence",
        command: $search_latency_command
      }
    },
    full_configs: ($full_configs_csv | split(",")),
    full_index_median_ms: {
      serial: $serial_full_ms,
      auto: $auto_full_ms,
      constrained: $constrained_full_ms
    },
    incremental_index_median_ms: {
      serial: ($serial_incremental_ms | tonumber),
      auto: ($auto_incremental_ms | tonumber),
      constrained: ($constrained_incremental_ms | tonumber)
    },
    scoped_follow_up_median_ms: {
      serial: ($serial_incremental_ms | tonumber),
      auto: ($auto_incremental_ms | tonumber),
      constrained: ($constrained_incremental_ms | tonumber)
    },
    write_heavy_index_median_ms: {
      serial: ($serial_write_heavy_ms | tonumber),
      auto: ($auto_write_heavy_ms | tonumber),
      constrained: ($constrained_write_heavy_ms | tonumber)
    },
    daemon_refresh_median_ms: ($daemon_refresh_ms | tonumber),
    scope_evidence: {
      fallback_count: $fallback_count,
      scoped_count: $scoped_count,
      full_count: $full_count
    },
    prefilter_evidence: {
      discovered_total: $prefilter_discovered_total,
      metadata_skipped_total: $prefilter_metadata_skipped_total,
      content_read_total: $prefilter_content_read_total,
      deleted_total: $prefilter_deleted_total,
      full_metadata_skipped_total: $full_metadata_skipped_total,
      full_content_read_total: $full_content_read_total
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
    },
    telemetry: $telemetry
  }' > "$SUMMARY_JSON"

printf 'Parallel indexing benchmark complete.\n'
printf 'Repository: %s\n' "$REPO"
printf 'Output: %s\n' "$OUT_DIR"
printf 'Retained outcomes: semantic_indexing incremental_indexing daemon_refresh; search_latency via just bench-search-latency\n'
printf 'Full reindex median ms: serial=%s auto=%s constrained=%s\n' \
  "$SERIAL_FULL_MS" "$AUTO_FULL_MS" "$CONSTRAINED_FULL_MS"
printf 'Incremental index median ms: serial=%s auto=%s constrained=%s\n' \
  "$SERIAL_INCREMENTAL_MS" "$AUTO_INCREMENTAL_MS" "$CONSTRAINED_INCREMENTAL_MS"
printf 'Write-heavy median ms: serial=%s auto=%s constrained=%s\n' \
  "$SERIAL_WRITE_HEAVY_MS" "$AUTO_WRITE_HEAVY_MS" "$CONSTRAINED_WRITE_HEAVY_MS"
printf 'Daemon refresh median ms: %s\n' "$DAEMON_REFRESH_MEDIAN_MS"
printf 'Scope evidence: fallback=%s scoped=%s full=%s\n' \
  "$FALLBACK_COUNT" "$SCOPED_COUNT" "$FULL_COUNT"
printf 'Prefilter evidence: discovered=%s metadata_skipped=%s content_read=%s deleted=%s full_metadata_skipped=%s full_content_read=%s\n' \
  "$PREFILTER_DISCOVERED_TOTAL" "$PREFILTER_METADATA_SKIPPED_TOTAL" \
  "$PREFILTER_CONTENT_READ_TOTAL" "$PREFILTER_DELETED_TOTAL" \
  "$FULL_METADATA_SKIPPED_TOTAL" "$FULL_CONTENT_READ_TOTAL"
