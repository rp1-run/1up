#!/usr/bin/env bash

impact_utc_timestamp() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

impact_default_baseline_ref() {
  local root_dir="$1"

  if git -C "$root_dir" rev-parse --verify origin/main >/dev/null 2>&1; then
    git -C "$root_dir" merge-base HEAD origin/main
    return
  fi

  if git -C "$root_dir" rev-parse --verify main >/dev/null 2>&1; then
    git -C "$root_dir" rev-parse main
    return
  fi

  git -C "$root_dir" rev-parse HEAD^
}

impact_oneup_data_dir() {
  local home_dir="$1"

  case "$(uname -s)" in
    Darwin)
      printf '%s/Library/Application Support/1up\n' "$home_dir"
      ;;
    *)
      printf '%s/.local/share/1up\n' "$home_dir"
      ;;
  esac
}

impact_prepare_fts_only_home() {
  local home_dir="$1"
  local data_dir

  mkdir -p "$home_dir"
  data_dir=$(impact_oneup_data_dir "$home_dir")
  mkdir -p "$data_dir/models/all-MiniLM-L6-v2"
  printf 'impact-trust-eval\n' > "$data_dir/models/all-MiniLM-L6-v2/.download_failed"
}

impact_run_oneup_json() {
  local bin_path="$1"
  local home_dir="$2"
  shift 2

  HOME="$home_dir" \
    XDG_DATA_HOME="$home_dir/.local/share" \
    "$bin_path" --format json "$@"
}

impact_build_binary() {
  local repo_dir="$1"

  cargo build --release --bin 1up --manifest-path "$repo_dir/Cargo.toml" >/dev/null
}

impact_sync_repo() {
  local source_dir="$1"
  local target_dir="$2"

  rm -rf "$target_dir"
  mkdir -p "$target_dir"
  cp -R "$source_dir"/. "$target_dir"/
}

impact_create_fixture() {
  local repo_dir="$1"

  rm -rf "$repo_dir"
  mkdir -p "$repo_dir/src/auth" "$repo_dir/src/cache" "$repo_dir/src/ui" "$repo_dir/tests"

  cat > "$repo_dir/src/auth/runtime.rs" <<'EOF'
pub fn load_auth_config() -> &'static str {
    "auth"
}

pub fn parse_auth_config(raw: &str) -> bool {
    !raw.trim().is_empty()
}
EOF

  cat > "$repo_dir/src/auth/bootstrap.rs" <<'EOF'
use crate::auth::runtime::load_auth_config;

pub fn boot_auth() -> &'static str {
    load_auth_config()
}
EOF

  cat > "$repo_dir/tests/auth_runtime_test.rs" <<'EOF'
use crate::auth::runtime::load_auth_config;

#[test]
fn loads_auth_runtime() {
    assert_eq!(load_auth_config(), "auth");
}
EOF

  cat > "$repo_dir/src/auth/config.rs" <<'EOF'
pub fn load_config() -> &'static str {
    "auth-scope"
}
EOF

  cat > "$repo_dir/src/auth/config_builder.rs" <<'EOF'
use crate::auth::config::load_config;

pub fn build_auth_config() -> &'static str {
    load_config()
}
EOF

  cat > "$repo_dir/src/cache/config.rs" <<'EOF'
pub fn load_config() -> &'static str {
    "cache"
}
EOF

  cat > "$repo_dir/src/ui/config.rs" <<'EOF'
pub fn load_config() -> &'static str {
    "ui"
}
EOF

  cat > "$repo_dir/tests/config_fixture.rs" <<'EOF'
pub fn load_config() -> &'static str {
    "tests"
}
EOF

  cat > "$repo_dir/src/cache/runtime.rs" <<'EOF'
pub fn warm_cache_key() -> &'static str {
    "cache"
}

pub fn normalize_cache_key(raw: &str) -> String {
    raw.trim().to_lowercase()
}
EOF

  cat > "$repo_dir/src/cache/priming.rs" <<'EOF'
use crate::cache::runtime::warm_cache_key;

pub fn prime_cache() -> &'static str {
    warm_cache_key()
}
EOF

  cat > "$repo_dir/tests/cache_runtime_test.rs" <<'EOF'
use crate::cache::runtime::warm_cache_key;

#[test]
fn warms_cache_runtime() {
    assert_eq!(warm_cache_key(), "cache");
}
EOF
}

impact_init_and_index_repo() {
  local bin_path="$1"
  local home_dir="$2"
  local repo_dir="$3"

  impact_run_oneup_json "$bin_path" "$home_dir" init "$repo_dir" >/dev/null
  impact_run_oneup_json "$bin_path" "$home_dir" index "$repo_dir" >/dev/null
}
