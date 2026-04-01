#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
DEFAULT_REPO="$HOME/Development/cash-server/emdash"
REPO_INPUT="${1:-$DEFAULT_REPO}"

INDEX_RUNS="${INDEX_RUNS:-3}"
QUERY_RUNS="${QUERY_RUNS:-7}"
QUERY_WARMUP="${QUERY_WARMUP:-1}"
KEEP_SNAPSHOT="${KEEP_SNAPSHOT:-0}"
SNAPSHOT_DIR="${SNAPSHOT_DIR:-}"
AUTO_SNAPSHOT=0
SUPPORT_WARN_THRESHOLD_PCT="${SUPPORT_WARN_THRESHOLD_PCT:-20}"
ONEUP_BIN="${ONEUP_BIN:-1up}"
OSGREP_BIN="${OSGREP_BIN:-rg}"

if [[ ! -d "$REPO_INPUT" ]]; then
  printf 'repo not found: %s\n' "$REPO_INPUT" >&2
  exit 1
fi

REPO=$(cd "$REPO_INPUT" && pwd -P)
REPO_NAME=$(basename "$REPO")
TIMESTAMP=$(date +"%Y%m%d-%H%M%S")
OUT_DIR="${OUT_DIR:-$ROOT_DIR/target/benchmarks/${REPO_NAME}-${TIMESTAMP}}"

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    printf 'missing required command: %s\n' "$1" >&2
    exit 1
  fi
}

log() {
  printf '[bench] %s\n' "$*" >&2
}

human_bytes() {
  awk -v bytes="$1" '
    BEGIN {
      split("B KiB MiB GiB TiB", unit, " ")
      value = bytes + 0
      idx = 1
      while (value >= 1024 && idx < 5) {
        value /= 1024
        idx++
      }
      printf "%.1f %s", value, unit[idx]
    }
  '
}

cleanup() {
  if [[ -n "${SNAPSHOT_DIR:-}" && -d "${SNAPSHOT_DIR:-}" ]]; then
    "$ONEUP_BIN" stop "$SNAPSHOT_DIR" >/dev/null 2>&1 || true
  fi

  if [[ "$AUTO_SNAPSHOT" -eq 1 && "$KEEP_SNAPSHOT" != "1" && -d "${SNAPSHOT_DIR:-}" ]]; then
    rm -rf "$SNAPSHOT_DIR"
  fi
}

snapshot_repo() {
  mkdir -p "$OUT_DIR"

  if [[ -z "$SNAPSHOT_DIR" ]]; then
    SNAPSHOT_DIR=$(mktemp -d "${TMPDIR:-/tmp}/1up-${REPO_NAME}.XXXXXX")
    AUTO_SNAPSHOT=1
  else
    mkdir -p "$SNAPSHOT_DIR"
  fi

  log "snapshotting tracked files into $SNAPSHOT_DIR"
  git -C "$REPO" ls-files -z | rsync -a --delete --from0 --files-from=- "$REPO/" "$SNAPSHOT_DIR/"
}

collect_metadata() {
  FILE_COUNT=$(cd "$SNAPSHOT_DIR" && find . -type f | wc -l | tr -d ' ')
  TOTAL_BYTES=$(
    cd "$SNAPSHOT_DIR" &&
      find . -type f -print0 |
      xargs -0 stat -f '%z' |
      awk '{sum += $1} END {print sum + 0}'
  )

  SUPPORTED_FILE_COUNT=$(
    cd "$SNAPSHOT_DIR" &&
      find . -type f |
      while IFS= read -r path; do
        case "$path" in
          *.rs|*.py|*.js|*.jsx|*.mjs|*.cjs|*.ts|*.tsx|*.go|*.java|*.kt|*.kts|*.c|*.h|*.cc|*.cpp|*.cxx|*.hpp|*.hxx)
            printf '%s\n' "$path"
            ;;
        esac
      done |
      wc -l |
      tr -d ' '
  )

  SUPPORTED_RATIO_PCT=$(awk -v count="$SUPPORTED_FILE_COUNT" -v total="$FILE_COUNT" 'BEGIN {
    if (total == 0) {
      printf "0.0"
    } else {
      printf "%.1f", (count * 100.0) / total
    }
  }')

  EXTENSION_COUNTS=$(
    cd "$SNAPSHOT_DIR" &&
      find . -type f |
      awk '
        function ext_of(path,    file) {
          file = path
          sub(/^.*\//, "", file)
          if (file !~ /\./) {
            return "[no_ext]"
          }
          sub(/^.*\./, "", file)
          return "." file
        }
        {
          counts[ext_of($0)]++
        }
        END {
          for (ext in counts) {
            printf "%s\t%d\n", ext, counts[ext]
          }
        }
      ' |
      sort -k2,2nr -k1,1
  )

  printf '%s\n' "$EXTENSION_COUNTS" > "$OUT_DIR/extensions.tsv"

  jq -n \
    --arg repo "$REPO" \
    --arg snapshot "$SNAPSHOT_DIR" \
    --arg out_dir "$OUT_DIR" \
    --arg repo_name "$REPO_NAME" \
    --arg total_bytes_human "$(human_bytes "$TOTAL_BYTES")" \
    --arg supported_ratio_pct "$SUPPORTED_RATIO_PCT" \
    --arg timestamp "$TIMESTAMP" \
    --argjson file_count "$FILE_COUNT" \
    --argjson total_bytes "$TOTAL_BYTES" \
    --argjson supported_file_count "$SUPPORTED_FILE_COUNT" \
    '{
      repo: $repo,
      repo_name: $repo_name,
      snapshot: $snapshot,
      out_dir: $out_dir,
      captured_at: $timestamp,
      file_count: $file_count,
      total_bytes: $total_bytes,
      total_bytes_human: $total_bytes_human,
      supported_file_count: $supported_file_count,
      supported_ratio_pct: ($supported_ratio_pct | tonumber)
    }' > "$OUT_DIR/metadata.json"
}

oneup_search_cmd() {
  local query="$1"
  printf "%q -f plain search %q --path %q -n 5" "$ONEUP_BIN" "$query" "$SNAPSHOT_DIR"
}

oneup_symbol_cmd() {
  local query="$1"
  printf "%q -f plain symbol %q --path %q" "$ONEUP_BIN" "$query" "$SNAPSHOT_DIR"
}

rg_search_cmd() {
  local query="$1"
  printf "cd %q && %q search --plain --compact %q ." "$SNAPSHOT_DIR" "$OSGREP_BIN" "$query"
}

rg_symbol_cmd() {
  local query="$1"
  printf "cd %q && %q symbols %q" "$SNAPSHOT_DIR" "$OSGREP_BIN" "$query"
}

run_hyperfine() {
  local output_json="$1"
  local runs="$2"
  local warmup="$3"
  shift 3

  local args=(
    --export-json "$output_json"
    --runs "$runs"
  )

  if [[ "$warmup" -gt 0 ]]; then
    args+=(--warmup "$warmup")
  fi

  hyperfine "${args[@]}" "$@"
}

prepare_indexes() {
  log "building fresh indexes for query benchmarks"
  rm -rf "$SNAPSHOT_DIR/.1up" "$SNAPSHOT_DIR/.rg"

  "$ONEUP_BIN" init "$SNAPSHOT_DIR" >/dev/null 2>&1
  "$ONEUP_BIN" index "$SNAPSHOT_DIR" >/dev/null 2>&1

  (
    cd "$SNAPSHOT_DIR"
    "$OSGREP_BIN" index --path "$SNAPSHOT_DIR" --reset >/dev/null 2>&1
  )
}

capture_preview() {
  local name="$1"
  local oneup_cmd="$2"
  local rg_cmd="$3"

  mkdir -p "$OUT_DIR/previews"
  eval "$oneup_cmd" > "$OUT_DIR/previews/${name}.1up.txt" 2>&1 || true
  eval "$rg_cmd" > "$OUT_DIR/previews/${name}.rg.txt" 2>&1 || true
}

write_table() {
  local json_path="$1"
  jq -r '
    [
      "| Command | Mean (s) | Std Dev (s) | Median (s) | Min (s) | Max (s) |",
      "|---|---:|---:|---:|---:|---:|",
      (
        .results[] |
        "| \(.command) | \(.mean | tostring) | \(.stddev | tostring) | \(.median | tostring) | \(.min | tostring) | \(.max | tostring) |"
      )
    ] | .[]
  ' "$json_path"
}

write_ratio_line() {
  local json_path="$1"
  jq -r '
    if (.results | length) == 2 then
      (.results | sort_by(.mean)) as $sorted |
      "- Faster: " + $sorted[0].command + " (" + (($sorted[1].mean / $sorted[0].mean) | tostring) + "x vs " + $sorted[1].command + ")"
    else
      empty
    end
  ' "$json_path"
}

write_summary() {
  {
    printf '# %s Benchmark\n\n' "$REPO_NAME"
    printf -- '- Repo: `%s`\n' "$REPO"
    printf -- '- Snapshot: `%s`\n' "$SNAPSHOT_DIR"
    printf -- '- Output: `%s`\n' "$OUT_DIR"
    printf -- '- 1up binary: `%s`\n' "$ONEUP_BIN"
    printf -- '- rg binary: `%s`\n' "$OSGREP_BIN"
    printf -- '- Corpus: %s tracked files, %s\n' "$FILE_COUNT" "$(human_bytes "$TOTAL_BYTES")"
    printf -- '- 1up structural-language coverage: %s/%s files (%s%%)\n' "$SUPPORTED_FILE_COUNT" "$FILE_COUNT" "$SUPPORTED_RATIO_PCT"
    if awk -v ratio="$SUPPORTED_RATIO_PCT" -v threshold="$SUPPORT_WARN_THRESHOLD_PCT" 'BEGIN { exit !(ratio < threshold) }'; then
      printf -- '- Symbol benchmarks: skipped. This corpus is dominated by languages the current `1up` build indexes as text chunks rather than parsed symbols.\n'
    else
      printf -- '- Symbol benchmarks: exact and partial lookup included.\n'
    fi
    printf '\n## Index\n\n'
    write_table "$OUT_DIR/index.json"
    printf '\n'
    write_ratio_line "$OUT_DIR/index.json"
    printf '\n\n## Search: PolicyRuleValidator\n\n'
    write_table "$OUT_DIR/search-routingrulevalidator.json"
    printf '\n'
    write_ratio_line "$OUT_DIR/search-routingrulevalidator.json"
    printf '\n\n## Search: request signing secret\n\n'
    write_table "$OUT_DIR/search-webhook-signing-secret.json"
    printf '\n'
    write_ratio_line "$OUT_DIR/search-webhook-signing-secret.json"
    printf '\n\n## Search: WorkItemUpdateEvent\n\n'
    write_table "$OUT_DIR/search-taskupdateevent.json"
    printf '\n'
    write_ratio_line "$OUT_DIR/search-taskupdateevent.json"
    if [[ -f "$OUT_DIR/symbol-routingrulevalidator.json" ]]; then
      printf '\n\n## Symbol: PolicyRuleValidator\n\n'
      write_table "$OUT_DIR/symbol-routingrulevalidator.json"
      printf '\n'
      write_ratio_line "$OUT_DIR/symbol-routingrulevalidator.json"
    fi
    if [[ -f "$OUT_DIR/symbol-routingrule.json" ]]; then
      printf '\n\n## Symbol: RoutingRule\n\n'
      write_table "$OUT_DIR/symbol-routingrule.json"
      printf '\n'
      write_ratio_line "$OUT_DIR/symbol-routingrule.json"
    fi
    printf '\n\n## Top Extensions\n\n'
    printf '| Extension | Files |\n'
    printf '|---|---:|\n'
    head -n 12 "$OUT_DIR/extensions.tsv" | awk -F'\t' '{printf "| %s | %s |\n", $1, $2}'
    printf '\n'
  } > "$OUT_DIR/summary.md"
}

trap cleanup EXIT

require_cmd git
require_cmd jq
require_cmd hyperfine
require_cmd "$OSGREP_BIN"
require_cmd rsync
require_cmd stat
require_cmd "$ONEUP_BIN"

snapshot_repo
collect_metadata

log "benchmarking clean indexing"
run_hyperfine \
  "$OUT_DIR/index.json" \
  "$INDEX_RUNS" \
  0 \
  --command-name "1up:index" \
  "rm -rf '$SNAPSHOT_DIR/.1up' && '$ONEUP_BIN' init '$SNAPSHOT_DIR' >/dev/null 2>&1 && '$ONEUP_BIN' index '$SNAPSHOT_DIR' >/dev/null 2>&1" \
  --command-name "rg:index" \
  "rm -rf '$SNAPSHOT_DIR/.rg' && cd '$SNAPSHOT_DIR' && '$OSGREP_BIN' index --path '$SNAPSHOT_DIR' --reset >/dev/null 2>&1"

prepare_indexes

ROUTING_RULE_ONEUP=$(oneup_search_cmd "PolicyRuleValidator")
ROUTING_RULE_OSGREP=$(rg_search_cmd "PolicyRuleValidator")
WEBHOOK_ONEUP=$(oneup_search_cmd "request signing secret")
WEBHOOK_OSGREP=$(rg_search_cmd "request signing secret")
TASK_UPDATE_ONEUP=$(oneup_search_cmd "WorkItemUpdateEvent")
TASK_UPDATE_OSGREP=$(rg_search_cmd "WorkItemUpdateEvent")
SYMBOL_EXACT_ONEUP=$(oneup_symbol_cmd "PolicyRuleValidator")
SYMBOL_EXACT_OSGREP=$(rg_symbol_cmd "PolicyRuleValidator")
SYMBOL_PARTIAL_ONEUP=$(oneup_symbol_cmd "RoutingRule")
SYMBOL_PARTIAL_OSGREP=$(rg_symbol_cmd "RoutingRule")

capture_preview "search-routingrulevalidator" "$ROUTING_RULE_ONEUP" "$ROUTING_RULE_OSGREP"
capture_preview "search-webhook-signing-secret" "$WEBHOOK_ONEUP" "$WEBHOOK_OSGREP"
capture_preview "search-taskupdateevent" "$TASK_UPDATE_ONEUP" "$TASK_UPDATE_OSGREP"

log "benchmarking warm search queries"
run_hyperfine \
  "$OUT_DIR/search-routingrulevalidator.json" \
  "$QUERY_RUNS" \
  "$QUERY_WARMUP" \
  --command-name "1up:search:PolicyRuleValidator" \
  "$ROUTING_RULE_ONEUP >/dev/null 2>&1" \
  --command-name "rg:search:PolicyRuleValidator" \
  "$ROUTING_RULE_OSGREP >/dev/null 2>&1"

run_hyperfine \
  "$OUT_DIR/search-webhook-signing-secret.json" \
  "$QUERY_RUNS" \
  "$QUERY_WARMUP" \
  --command-name "1up:search:request signing secret" \
  "$WEBHOOK_ONEUP >/dev/null 2>&1" \
  --command-name "rg:search:request signing secret" \
  "$WEBHOOK_OSGREP >/dev/null 2>&1"

run_hyperfine \
  "$OUT_DIR/search-taskupdateevent.json" \
  "$QUERY_RUNS" \
  "$QUERY_WARMUP" \
  --command-name "1up:search:WorkItemUpdateEvent" \
  "$TASK_UPDATE_ONEUP >/dev/null 2>&1" \
  --command-name "rg:search:WorkItemUpdateEvent" \
  "$TASK_UPDATE_OSGREP >/dev/null 2>&1"

if ! awk -v ratio="$SUPPORTED_RATIO_PCT" -v threshold="$SUPPORT_WARN_THRESHOLD_PCT" 'BEGIN { exit !(ratio < threshold) }'; then
  capture_preview "symbol-routingrulevalidator" "$SYMBOL_EXACT_ONEUP" "$SYMBOL_EXACT_OSGREP"
  capture_preview "symbol-routingrule" "$SYMBOL_PARTIAL_ONEUP" "$SYMBOL_PARTIAL_OSGREP"

  log "benchmarking warm symbol queries"
  run_hyperfine \
    "$OUT_DIR/symbol-routingrulevalidator.json" \
    "$QUERY_RUNS" \
    "$QUERY_WARMUP" \
    --command-name "1up:symbol:PolicyRuleValidator" \
    "$SYMBOL_EXACT_ONEUP >/dev/null 2>&1" \
    --command-name "rg:symbol:PolicyRuleValidator" \
    "$SYMBOL_EXACT_OSGREP >/dev/null 2>&1"

  run_hyperfine \
    "$OUT_DIR/symbol-routingrule.json" \
    "$QUERY_RUNS" \
    "$QUERY_WARMUP" \
    --command-name "1up:symbol:RoutingRule" \
    "$SYMBOL_PARTIAL_ONEUP >/dev/null 2>&1" \
    --command-name "rg:symbol:RoutingRule" \
    "$SYMBOL_PARTIAL_OSGREP >/dev/null 2>&1"
fi

write_summary

printf 'summary: %s\n' "$OUT_DIR/summary.md"
printf 'results: %s\n' "$OUT_DIR"
if [[ "$AUTO_SNAPSHOT" -eq 1 && "$KEEP_SNAPSHOT" == "1" ]]; then
  printf 'snapshot: %s\n' "$SNAPSHOT_DIR"
fi
