#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
BASELINE_REF="${BASELINE_REF:-45b5117}"
INDEX_RUNS="${INDEX_RUNS:-3}"
QUERY_RUNS="${QUERY_RUNS:-7}"
QUERY_WARMUP="${QUERY_WARMUP:-1}"
CHURN_CYCLES="${CHURN_CYCLES:-3}"
KEEP_BASELINE_WORKTREE="${KEEP_BASELINE_WORKTREE:-0}"
TIMESTAMP=$(date +"%Y%m%d-%H%M%S")
OUT_DIR="${OUT_DIR:-$ROOT_DIR/target/rewrite-sql-bench/$TIMESTAMP}"
MODEL_DIR="${HOME}/.local/share/1up/models/all-MiniLM-L6-v2"

BASELINE_WORKTREE=""

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    printf 'missing required command: %s\n' "$1" >&2
    exit 1
  fi
}

log() {
  printf '[rewrite-sql-bench] %s\n' "$*" >&2
}

cleanup() {
  if [[ -n "$BASELINE_WORKTREE" && -d "$BASELINE_WORKTREE" && "$KEEP_BASELINE_WORKTREE" != "1" ]]; then
    git -C "$ROOT_DIR" worktree remove --force "$BASELINE_WORKTREE" >/dev/null 2>&1 || true
  fi
}

trap cleanup EXIT

create_fixture() {
  local dir="$1"
  mkdir -p "$dir/src" "$dir/tools" "$dir/web"

  cat > "$dir/src/config.rs" <<'EOF'
pub fn load_config() -> &'static str {
    let host = "config loading host port settings";
    let cache = "config cache path";
    if host.is_empty() {
        return cache;
    }
    host
}
EOF

  cat > "$dir/src/auth.rs" <<'EOF'
pub fn validate_token(token: &str) -> bool {
    let middleware = "request auth token validation middleware";
    let session = "auth token session gate";
    !token.is_empty() && !middleware.is_empty() && !session.is_empty()
}
EOF

  cat > "$dir/src/server.rs" <<'EOF'
pub fn request_pipeline() -> &'static str {
    let flow = "request pipeline json response rendering";
    let guard = "auth middleware before response";
    if flow.is_empty() {
        return guard;
    }
    flow
}
EOF

  cat > "$dir/tools/output.py" <<'EOF'
import json

def serialize_response(payload: dict) -> str:
    text = "serialize json response payload"
    return json.dumps({"text": text, "payload": payload}, indent=2, sort_keys=True)
EOF

  cat > "$dir/web/billing.js" <<'EOF'
function invoiceTotal(subtotal, tax) {
    const label = "billing invoice total tax calculation";
    if (!label) {
        return subtotal;
    }
    return subtotal + tax;
}

module.exports = { invoiceTotal };
EOF

  for idx in $(seq 1 18); do
    cat > "$dir/src/worker_${idx}.rs" <<EOF
pub fn worker_${idx}_flow() -> &'static str {
    let request = "request worker ${idx} orchestration";
    let config = "config host ${idx}";
    let auth = "auth token ${idx}";
    if request.is_empty() {
        return auth;
    }
    config
}
EOF
  done

  for idx in $(seq 1 10); do
    cat > "$dir/tools/task_${idx}.py" <<EOF
def task_${idx}_summary() -> str:
    text = "serialize worker ${idx} result"
    return text
EOF
  done
}

prepare_snapshot() {
  local template_dir="$1"
  local snapshot_dir="$2"
  rm -rf "$snapshot_dir"
  cp -R "$template_dir" "$snapshot_dir"
}

build_binary() {
  local workdir="$1"
  log "building $(basename "$workdir")"
  cargo build --release --bin 1up --manifest-path "$workdir/Cargo.toml" >/dev/null
}

metric_value() {
  local json_path="$1"
  local result_index="$2"
  local metric="$3"

  if [[ "$metric" == "median" ]]; then
    jq -r --argjson idx "$result_index" '.results[$idx].median' "$json_path"
  else
    jq -r --argjson idx "$result_index" '
      .results[$idx].times
      | sort
      | .[((length * 0.95 | ceil) - 1)]
    ' "$json_path"
  fi
}

to_ms() {
  awk -v value="$1" 'BEGIN { printf "%.2f", value * 1000 }'
}

top_hits() {
  local bin_path="$1"
  local repo_path="$2"
  local query="$3"
  "$bin_path" --format json search "$query" --path "$repo_path" -n 3 2>/dev/null |
    jq -r '.[].file_path' |
    paste -sd ',' -
}

search_hits_expected() {
  local bin_path="$1"
  local repo_path="$2"
  local query="$3"
  local expected="$4"
  local stderr_path="$5"
  local output

  if ! output=$("$bin_path" --format json search "$query" --path "$repo_path" -n 5 2>"$stderr_path"); then
    return 1
  fi

  jq -e --arg expected "$expected" --arg query "$query" '
    .[]
    | select(.file_path == $expected and (.content | contains($query)))
  ' <<<"$output" >/dev/null
}

search_misses_expected() {
  local bin_path="$1"
  local repo_path="$2"
  local query="$3"
  local expected="$4"
  local stderr_path="$5"
  local output

  if ! output=$("$bin_path" --format json search "$query" --path "$repo_path" -n 5 2>"$stderr_path"); then
    return 1
  fi

  jq -e --arg expected "$expected" --arg query "$query" '
    map(select(.file_path == $expected and (.content | contains($query)))) | length == 0
  ' <<<"$output" >/dev/null
}

stderr_mentions_reindex() {
  local stderr_path="$1"
  [[ -f "$stderr_path" ]] && grep -q "1up reindex" "$stderr_path"
}

run_operational_stability() {
  local label="$1"
  local bin_path="$2"
  local repo_path="$3"
  local label_key
  local command_failures=0
  local reindex_prompts=0
  local freshness_failures=0
  local checks_total=$((CHURN_CYCLES * 4))

  prepare_snapshot "$TEMPLATE_DIR" "$repo_path"
  label_key=$(printf '%s' "$label" | tr '[:upper:]' '[:lower:]')

  local stderr_path="$OUT_DIR/${label}_initial_index.stderr"
  if ! "$bin_path" --format json index "$repo_path" >/dev/null 2>"$stderr_path"; then
    command_failures=$((command_failures + 1))
  fi
  if stderr_mentions_reindex "$stderr_path"; then
    reindex_prompts=$((reindex_prompts + 1))
  fi

  for cycle in $(seq 1 "$CHURN_CYCLES"); do
    local rel_path="src/churn_${cycle}.rs"
    local file_path="$repo_path/$rel_path"
    local add_query="${label_key}churnaddcycle${cycle}marker"
    local edit_query="${label_key}churneditcycle${cycle}marker"

    cat > "$file_path" <<EOF
pub fn churn_${cycle}_marker() -> &'static str {
    "${add_query}"
}
EOF

    stderr_path="$OUT_DIR/${label}_add_${cycle}.stderr"
    if ! "$bin_path" --format json index "$repo_path" >/dev/null 2>"$stderr_path"; then
      command_failures=$((command_failures + 1))
    fi
    if stderr_mentions_reindex "$stderr_path"; then
      reindex_prompts=$((reindex_prompts + 1))
    fi
    stderr_path="$OUT_DIR/${label}_add_search_${cycle}.stderr"
    if ! search_hits_expected "$bin_path" "$repo_path" "$add_query" "$rel_path" "$stderr_path"; then
      freshness_failures=$((freshness_failures + 1))
    fi
    if stderr_mentions_reindex "$stderr_path"; then
      reindex_prompts=$((reindex_prompts + 1))
    fi

    cat > "$file_path" <<EOF
pub fn churn_${cycle}_marker_updated() -> &'static str {
    "${edit_query}"
}
EOF

    stderr_path="$OUT_DIR/${label}_edit_${cycle}.stderr"
    if ! "$bin_path" --format json index "$repo_path" >/dev/null 2>"$stderr_path"; then
      command_failures=$((command_failures + 1))
    fi
    if stderr_mentions_reindex "$stderr_path"; then
      reindex_prompts=$((reindex_prompts + 1))
    fi
    stderr_path="$OUT_DIR/${label}_edit_search_${cycle}.stderr"
    if ! search_hits_expected "$bin_path" "$repo_path" "$edit_query" "$rel_path" "$stderr_path"; then
      freshness_failures=$((freshness_failures + 1))
    fi
    if stderr_mentions_reindex "$stderr_path"; then
      reindex_prompts=$((reindex_prompts + 1))
    fi
    stderr_path="$OUT_DIR/${label}_stale_search_${cycle}.stderr"
    if ! search_misses_expected "$bin_path" "$repo_path" "$add_query" "$rel_path" "$stderr_path"; then
      freshness_failures=$((freshness_failures + 1))
    fi
    if stderr_mentions_reindex "$stderr_path"; then
      reindex_prompts=$((reindex_prompts + 1))
    fi

    rm -f "$file_path"

    stderr_path="$OUT_DIR/${label}_delete_${cycle}.stderr"
    if ! "$bin_path" --format json index "$repo_path" >/dev/null 2>"$stderr_path"; then
      command_failures=$((command_failures + 1))
    fi
    if stderr_mentions_reindex "$stderr_path"; then
      reindex_prompts=$((reindex_prompts + 1))
    fi
    stderr_path="$OUT_DIR/${label}_delete_search_${cycle}.stderr"
    if ! search_misses_expected "$bin_path" "$repo_path" "$edit_query" "$rel_path" "$stderr_path"; then
      freshness_failures=$((freshness_failures + 1))
    fi
    if stderr_mentions_reindex "$stderr_path"; then
      reindex_prompts=$((reindex_prompts + 1))
    fi
  done

  local checks_passed=$((checks_total - freshness_failures))
  local manual_interventions=$((command_failures + reindex_prompts))
  local notes="Routine add/edit/delete cycles completed without extra operator action"
  if [[ "$manual_interventions" -ne 0 || "$freshness_failures" -ne 0 ]]; then
    notes="See command, prompt, or freshness failure counts"
  fi

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$label" \
    "$CHURN_CYCLES" \
    "$checks_passed" \
    "$checks_total" \
    "$command_failures" \
    "$reindex_prompts" \
    "$manual_interventions" \
    "$notes" >> "$OPERATIONAL_TSV"
}

require_cmd cargo git hyperfine jq

if [[ ! -f "$MODEL_DIR/model.onnx" || ! -f "$MODEL_DIR/tokenizer.json" ]]; then
  printf 'embedding model not available at %s\n' "$MODEL_DIR" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"

BASELINE_WORKTREE=$(mktemp -d "${TMPDIR:-/tmp}/1up-rewrite-sql-baseline.XXXXXX")
rm -rf "$BASELINE_WORKTREE"
git -C "$ROOT_DIR" worktree add --detach "$BASELINE_WORKTREE" "$BASELINE_REF" >/dev/null

CANDIDATE_REF=$(git -C "$ROOT_DIR" rev-parse --short HEAD)
BASELINE_SHA=$(git -C "$ROOT_DIR" rev-parse --short "$BASELINE_REF")
BASELINE_BIN="$BASELINE_WORKTREE/target/release/1up"
CANDIDATE_BIN="$ROOT_DIR/target/release/1up"
TEMPLATE_DIR="$OUT_DIR/template_repo"
BASELINE_SNAPSHOT="$OUT_DIR/baseline_repo"
CANDIDATE_SNAPSHOT="$OUT_DIR/candidate_repo"
INDEX_JSON="$OUT_DIR/index.json"
QUALITY_TSV="$OUT_DIR/quality.tsv"
OPERATIONAL_TSV="$OUT_DIR/operational.tsv"
SUMMARY_MD="$OUT_DIR/summary.md"

create_fixture "$TEMPLATE_DIR"
build_binary "$BASELINE_WORKTREE"
build_binary "$ROOT_DIR"

log "benchmarking clean index rebuild"
BASELINE_INDEX_CMD=$(printf 'rm -rf %q && cp -R %q %q && %q --format json index %q >/dev/null' "$BASELINE_SNAPSHOT" "$TEMPLATE_DIR" "$BASELINE_SNAPSHOT" "$BASELINE_BIN" "$BASELINE_SNAPSHOT")
CANDIDATE_INDEX_CMD=$(printf 'rm -rf %q && cp -R %q %q && %q --format json index %q >/dev/null' "$CANDIDATE_SNAPSHOT" "$TEMPLATE_DIR" "$CANDIDATE_SNAPSHOT" "$CANDIDATE_BIN" "$CANDIDATE_SNAPSHOT")
hyperfine --runs "$INDEX_RUNS" --export-json "$INDEX_JSON" "$BASELINE_INDEX_CMD" "$CANDIDATE_INDEX_CMD" >/dev/null

prepare_snapshot "$TEMPLATE_DIR" "$BASELINE_SNAPSHOT"
prepare_snapshot "$TEMPLATE_DIR" "$CANDIDATE_SNAPSHOT"
"$BASELINE_BIN" --format json index "$BASELINE_SNAPSHOT" >/dev/null
"$CANDIDATE_BIN" --format json index "$CANDIDATE_SNAPSHOT" >/dev/null

queries=(
  "config loading host port|src/config.rs"
  "request auth token validation middleware|src/auth.rs"
  "request pipeline json response rendering|src/server.rs"
  "serialize json response payload|tools/output.py"
  "billing invoice total tax calculation|web/billing.js"
)

printf 'query\texpected\tbaseline_top3\tbaseline_hit\tcandidate_top3\tcandidate_hit\n' > "$QUALITY_TSV"
printf 'variant\tcycles\tchecks_passed\tchecks_total\tcommand_failures\treindex_prompts\tmanual_interventions\tnotes\n' > "$OPERATIONAL_TSV"

for entry in "${queries[@]}"; do
  query=${entry%%|*}
  expected=${entry##*|}
  slug=$(printf '%s' "$query" | tr ' ' '_' | tr -cd '[:alnum:]_')
  json_path="$OUT_DIR/${slug}.json"
  baseline_cmd=$(printf '%q --format json search %q --path %q -n 5 >/dev/null' "$BASELINE_BIN" "$query" "$BASELINE_SNAPSHOT")
  candidate_cmd=$(printf '%q --format json search %q --path %q -n 5 >/dev/null' "$CANDIDATE_BIN" "$query" "$CANDIDATE_SNAPSHOT")

  log "benchmarking query: $query"
  hyperfine --runs "$QUERY_RUNS" --warmup "$QUERY_WARMUP" --export-json "$json_path" "$baseline_cmd" "$candidate_cmd" >/dev/null

  baseline_top3=$(top_hits "$BASELINE_BIN" "$BASELINE_SNAPSHOT" "$query")
  candidate_top3=$(top_hits "$CANDIDATE_BIN" "$CANDIDATE_SNAPSHOT" "$query")
  baseline_hit=0
  candidate_hit=0
  [[ ",$baseline_top3," == *",$expected,"* ]] && baseline_hit=1
  [[ ",$candidate_top3," == *",$expected,"* ]] && candidate_hit=1
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$query" \
    "$expected" \
    "$baseline_top3" \
    "$baseline_hit" \
    "$candidate_top3" \
    "$candidate_hit" >> "$QUALITY_TSV"
done

log "running operational stability comparison"
run_operational_stability "Baseline" "$BASELINE_BIN" "$BASELINE_SNAPSHOT"
run_operational_stability "Candidate" "$CANDIDATE_BIN" "$CANDIDATE_SNAPSHOT"

{
  printf '# rewrite-sql benchmark evidence\n\n'
  printf -- '- Baseline ref: `%s` (%s)\n' "$BASELINE_REF" "$BASELINE_SHA"
  printf -- '- Candidate ref: `HEAD` (%s)\n' "$CANDIDATE_REF"
  printf -- '- Output dir: `%s`\n' "$OUT_DIR"
  printf -- '- Index runs: %s\n' "$INDEX_RUNS"
  printf -- '- Query runs: %s (warmup %s)\n' "$QUERY_RUNS" "$QUERY_WARMUP"
  printf -- '- Fixture size: %s files\n\n' "$(find "$TEMPLATE_DIR" -type f | wc -l | tr -d ' ')"

  printf '## Clean rebuild latency\n\n'
  printf '| Variant | Median (ms) | p95 (ms) |\n'
  printf '|---|---:|---:|\n'
  printf '| Baseline | %s | %s |\n' \
    "$(to_ms "$(metric_value "$INDEX_JSON" 0 median)")" \
    "$(to_ms "$(metric_value "$INDEX_JSON" 0 p95)")"
  printf '| Candidate | %s | %s |\n\n' \
    "$(to_ms "$(metric_value "$INDEX_JSON" 1 median)")" \
    "$(to_ms "$(metric_value "$INDEX_JSON" 1 p95)")"

  printf '## Search latency\n\n'
  printf '| Query | Expected file | Baseline median (ms) | Baseline p95 (ms) | Candidate median (ms) | Candidate p95 (ms) |\n'
  printf '|---|---|---:|---:|---:|---:|\n'

  for entry in "${queries[@]}"; do
    query=${entry%%|*}
    expected=${entry##*|}
    slug=$(printf '%s' "$query" | tr ' ' '_' | tr -cd '[:alnum:]_')
    json_path="$OUT_DIR/${slug}.json"
  printf '| %s | `%s` | %s | %s | %s | %s |\n' \
      "$query" \
      "$expected" \
      "$(to_ms "$(metric_value "$json_path" 0 median)")" \
      "$(to_ms "$(metric_value "$json_path" 0 p95)")" \
      "$(to_ms "$(metric_value "$json_path" 1 median)")" \
      "$(to_ms "$(metric_value "$json_path" 1 p95)")"
  done

  printf '\n## Operational stability\n\n'
  printf '| Variant | Churn cycles | Freshness checks passed | Command failures | Reindex prompts | Manual interventions | Notes |\n'
  printf '|---|---:|---:|---:|---:|---:|---|\n'
  tail -n +2 "$OPERATIONAL_TSV" |
    while IFS=$'\t' read -r variant cycles checks_passed checks_total command_failures reindex_prompts manual_interventions notes; do
      printf '| %s | %s | %s/%s | %s | %s | %s | %s |\n' \
        "$variant" \
        "$cycles" \
        "$checks_passed" \
        "$checks_total" \
        "$command_failures" \
        "$reindex_prompts" \
        "$manual_interventions" \
        "$notes"
    done

  printf '\n## Quality corpus\n\n'
  printf '| Query | Expected file | Baseline top-3 | Baseline hit | Candidate top-3 | Candidate hit |\n'
  printf '|---|---|---|---:|---|---:|\n'
  tail -n +2 "$QUALITY_TSV" |
    while IFS=$'\t' read -r query expected baseline_top3 baseline_hit candidate_top3 candidate_hit; do
      printf '| %s | `%s` | `%s` | %s | `%s` | %s |\n' \
        "$query" \
        "$expected" \
        "$baseline_top3" \
        "$baseline_hit" \
        "$candidate_top3" \
        "$candidate_hit"
    done
} > "$SUMMARY_MD"

log "wrote summary to $SUMMARY_MD"
printf '%s\n' "$SUMMARY_MD"
