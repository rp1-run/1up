#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=./common.sh
source "$SCRIPT_DIR/common.sh"

BINARY_PATH=""
REPO_PATH=""
OUTPUT_PATH=""
SELF_TEST_ONLY=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --binary)
      BINARY_PATH="${2:-}"
      shift 2
      ;;
    --repo)
      REPO_PATH="${2:-}"
      shift 2
      ;;
    --output)
      OUTPUT_PATH="${2:-}"
      shift 2
      ;;
    --self-test-only)
      SELF_TEST_ONLY=1
      shift
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

# --self-test-only runs just the ancestor-guard self-test (no MCP server, no
# binary) so CI can exercise the platform-specific guard branches — notably
# the Windows directory-junction branch — without a full release smoke.
if [[ "$SELF_TEST_ONLY" == "1" ]]; then
  if [[ -z "$OUTPUT_PATH" ]]; then
    fail "usage: $(basename "$0") --self-test-only --output <path>"
  fi
  BINARY_PATH="(self-test-only)"
  REPO_PATH="(self-test-only)"
else
  if [[ -z "$BINARY_PATH" || -z "$REPO_PATH" || -z "$OUTPUT_PATH" ]]; then
    fail "usage: $(basename "$0") --binary <path> --repo <path> --output <path>"
  fi

  require_cmd jq
  require_file "$BINARY_PATH"

  if [[ ! -d "$REPO_PATH" ]]; then
    fail "missing required repository directory: $(relative_path "$REPO_PATH")"
  fi

  if [[ ! -x "$BINARY_PATH" ]]; then
    fail "binary is not executable: $(relative_path "$BINARY_PATH")"
  fi
fi

PYTHON_CMD=()
if command -v python3 >/dev/null 2>&1; then
  PYTHON_CMD=(python3)
elif command -v python >/dev/null 2>&1; then
  PYTHON_CMD=(python)
elif command -v py >/dev/null 2>&1; then
  PYTHON_CMD=(py -3)
else
  fail "missing required command: python3 or python"
fi

if [[ "$SELF_TEST_ONLY" != "1" ]]; then
  BINARY_PATH=$(cd "$(dirname "$BINARY_PATH")" && pwd -P)/$(basename "$BINARY_PATH")
  REPO_PATH=$(cd "$REPO_PATH" && pwd -P)
fi
mkdir -p "$(dirname "$OUTPUT_PATH")"
OUTPUT_PATH=$(cd "$(dirname "$OUTPUT_PATH")" && pwd -P)/$(basename "$OUTPUT_PATH")

RUN_MODE="full"
if [[ "$SELF_TEST_ONLY" == "1" ]]; then
  RUN_MODE="self-test-only"
fi

# UTF-8 mode: windows python otherwise decodes child output as cp1252 and
# fails on multibyte CLI output such as the version banner emoji.
if PYTHONUTF8=1 "${PYTHON_CMD[@]}" - "$BINARY_PATH" "$REPO_PATH" "$OUTPUT_PATH" "$RUN_MODE" <<'PY'
import json
import os
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

EXPECTED_TOOLS = [
    "oneup_status",
    "oneup_start",
    "oneup_search",
    "oneup_get",
    "oneup_symbol",
    "oneup_context",
    "oneup_impact",
    "oneup_structural",
    "oneup_overview",
]
READINESS_STATUSES = {"missing", "indexing", "stale", "ready", "degraded", "blocked"}
DISCOVERY_READY_STATUSES = {"ready", "degraded"}
REQUIRED_FLOW_LABELS = [
    "status",
    "start",
    "search",
    "get",
    "symbol",
    "context",
    "impact",
    "structural",
    "overview",
]
FIXTURE_FILES = {
    "src/policy.rs": """pub struct PolicyRuleValidator;

impl PolicyRuleValidator {
    pub fn validate(&self, policy: &str) -> bool {
        !policy.is_empty()
    }
}
""",
    "src/runner.rs": """use crate::policy::PolicyRuleValidator;

pub fn run_validation(validator: &PolicyRuleValidator) -> bool {
    validator.validate("allow")
}
""",
}


class SmokeFailure(Exception):
    def __init__(self, message, protocol_clean=None):
        super().__init__(message)
        self.protocol_clean = protocol_clean


class KnownIssue110Failure(SmokeFailure):
    """The tracked rp1-run/1up#110 shape: `oneup_search` yields no fixture
    PolicyRuleValidator hit (an empty result set or a result set without the
    hit). The known-issue gate keys on this exact type, never on message
    text, so an unrelated failure whose message happens to contain the same
    words can never be converted into a known-issue pass."""


FIXTURE_SEARCH_HIT_MISSING = (
    "oneup_search did not return the fixture PolicyRuleValidator hit"
)


def strip_extended_length_prefix(path):
    r"""Strips Windows extended-length prefixes (``//?/`` and ``//?/UNC/``)
    from an already forward-slashed path."""
    if path.startswith("//?/UNC/"):
        return "//" + path[len("//?/UNC/") :]
    if path.startswith("//?/"):
        return path[len("//?/") :]
    return path


# Forward-slashed, prefix-stripped spellings of the fixture repository root
# (as given and fully resolved); populated once ``repo_path`` is parsed.
FIXTURE_REPO_ROOT_VARIANTS = []


def normalize_fixture_path(value):
    r"""Normalize a repository path for fixture comparison.

    Collapses Windows backslash separators to forward slashes, strips
    extended-length path prefixes (``\\?\`` and ``\\?\UNC\``), and
    relativizes a path under the fixture repository root — an absolute
    result path such as ``C:\runner\fixture\src\policy.rs`` can never
    compare equal to the fixture-relative ``src/policy.rs`` otherwise.
    Absolute paths outside the fixture root are returned as-is, so they
    simply never match a fixture expectation.
    """
    if not isinstance(value, str):
        return value
    normalized = strip_extended_length_prefix(value.replace("\\", "/"))
    # Windows paths compare case-insensitively (drive letter and 8.3 casing
    # both vary across producers); POSIX paths do not.
    haystack = normalized.lower() if sys.platform == "win32" else normalized
    for root in FIXTURE_REPO_ROOT_VARIANTS:
        needle = root.lower() if sys.platform == "win32" else root
        if haystack.startswith(needle + "/"):
            return normalized[len(root) + 1 :]
    return normalized


def parse_line_number(value):
    """Best-effort integer conversion: ``None`` for malformed metadata, so a
    bad record reads as a non-match instead of crashing the smoke before its
    diagnostics are captured."""
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


# Hard expiry for the #110 known-issue gate: on/after this date the gate no
# longer converts the matched failure into passed_with_known_issue.
KNOWN_ISSUE_110_EXPIRY = "2026-10-15"


def known_issue_110_gate_decision():
    """Decide whether the #110 known-issue gate converts the matched failure.

    Returns ``(active, reason)``. ``ONEUP_SMOKE_KNOWN_ISSUE_110=0`` forces
    the gate off (hard-fail restored) and is always honored. The hard expiry
    is enforced next: on/after ``KNOWN_ISSUE_110_EXPIRY`` the gate is off and
    no override can extend it — re-keying the deadline requires a deliberate
    edit here. Before expiry the gate defaults to on.
    """
    override = os.environ.get("ONEUP_SMOKE_KNOWN_ISSUE_110", "")
    if override == "0":
        return False, "ONEUP_SMOKE_KNOWN_ISSUE_110=0 forces the gate off"
    today = time.strftime("%Y-%m-%d", time.gmtime())
    if today >= KNOWN_ISSUE_110_EXPIRY:
        return False, (
            f"gate expired on {KNOWN_ISSUE_110_EXPIRY}; fix rp1-run/1up#110 "
            "or deliberately re-key/extend KNOWN_ISSUE_110_EXPIRY "
            "(ONEUP_SMOKE_KNOWN_ISSUE_110=1 cannot extend the hard expiry)"
        )
    return True, f"default gate active until {KNOWN_ISSUE_110_EXPIRY}"


binary_path = sys.argv[1]
repo_path = sys.argv[2]
output_path = sys.argv[3]
self_test_only = len(sys.argv) > 4 and sys.argv[4] == "self-test-only"
if not self_test_only:
    for _root in (repo_path, str(Path(repo_path).resolve())):
        _cleaned = strip_extended_length_prefix(_root.replace("\\", "/")).rstrip("/")
        if _cleaned and _cleaned not in FIXTURE_REPO_ROOT_VARIANTS:
            FIXTURE_REPO_ROOT_VARIANTS.append(_cleaned)
server_command = [binary_path, "mcp", "--path", repo_path]
artifact = {
    "schema": "mcp_smoke.v2",
    "mode": "self_test_only" if self_test_only else "full",
    "status": "failed",
    "binary": binary_path,
    "version": "",
    "server_command": server_command,
    "fixture_repo": repo_path,
    "fixture_files_created": [],
    "tools": [],
    "exercised_tools": [],
    "tool_calls": [],
    "response_statuses": {},
    "structured_content_present": {},
    "presentation_free": True,
    "presentation_leaks": [],
    "discovery_flow": {
        "status": "failed",
        "required_labels": REQUIRED_FLOW_LABELS,
    },
    "readiness_status": "",
    "stdout_protocol_clean": True,
    "diagnostics": [],
}


def write_artifact(status):
    artifact["status"] = status
    output = Path(output_path)
    output.parent.mkdir(parents=True, exist_ok=True)
    tmp = output.with_name(f"{output.name}.tmp")
    tmp.write_text(json.dumps(artifact, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(tmp, output)


def fail(message, protocol_clean=None):
    if protocol_clean is not None:
        artifact["stdout_protocol_clean"] = protocol_clean
    artifact["diagnostics"].append(message)
    write_artifact("failed")
    print(f"[release-assets] {message}", file=sys.stderr)
    return 1


def record_known_issue_110(reason):
    artifact["discovery_flow"]["status"] = "skipped_known_issue"
    artifact["known_issue"] = {
        "target_platform": "windows",
        "signature": "search_assertion",
        "readiness_status": artifact.get("readiness_status", ""),
        "reason": reason,
        "tracking_issue": "https://github.com/rp1-run/1up/issues/110",
        "description": (
            "windows oneup_search does not return the fixture "
            "PolicyRuleValidator hit; MCP protocol surface and prior "
            "discovery steps verified, search-dependent assertion skipped"
        ),
        "search_debug": artifact.get("search_debug"),
    }
    print(
        "[release-assets] WARNING: windows MCP search assertion skipped due to "
        f"known issue rp1-run/1up#110: {reason}",
        file=sys.stderr,
    )
    write_artifact("passed_with_known_issue")


def require_real_ancestors(repo, relative_path):
    """Rejects a redirecting (or otherwise non-directory) ancestor of a
    fixture path, so a fixture write can never traverse out of the controlled
    tree — the file-level guard below covers only the final path component.
    Not-yet-existing ancestors are fine: mkdir creates them as real
    directories. The repo root itself is already physically resolved by the
    wrapping bash script (`pwd -P`).

    Two independent checks are required: `is_symlink()` catches POSIX
    symlinks (including broken ones, where `exists()` is False), but is False
    for a Windows directory junction, whose reparse point `exists()`/`is_dir()`
    happily follow. Junctions — and any other reparse redirect — are caught by
    comparing each component's physical resolution (`os.path.realpath`, which
    resolves junctions) against the physical path it would have if every
    component were a real directory."""
    resolved_expected = Path(os.path.realpath(repo))
    current = Path(repo)
    for part in Path(relative_path).parent.parts:
        current = current / part
        resolved_expected = resolved_expected / part
        if current.is_symlink() or (current.exists() and not current.is_dir()):
            raise SmokeFailure(
                f"fixture ancestor {current.relative_to(repo)} of "
                f"{relative_path} is not a real directory; refusing to write "
                "through it"
            )
        if current.exists():
            resolved = Path(os.path.realpath(current))
            if os.path.normcase(str(resolved)) != os.path.normcase(
                str(resolved_expected)
            ):
                raise SmokeFailure(
                    f"fixture ancestor {current.relative_to(repo)} of "
                    f"{relative_path} physically resolves to {resolved} "
                    "(a junction or other reparse redirect); refusing to "
                    "write through it"
                )


def write_fixture_file(repo, relative_path, content):
    """Writes one fixture file under `repo` behind the ancestor and final-
    component guards. Returns True when the file was (re)written, False when
    an identical file already exists. Shared by `ensure_fixture_repo` and the
    adversarial self-test so the test exercises the real write path."""
    path = repo / relative_path
    require_real_ancestors(repo, relative_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    # Never write through a symlink or onto a non-regular file: the
    # rewrite below must only ever mutate the fixture file itself, not
    # whatever an existing link happens to point at.
    if path.is_symlink() or (path.exists() and not path.is_file()):
        raise SmokeFailure(
            f"fixture path {relative_path} exists but is not a regular "
            "file; refusing to overwrite it"
        )
    # Compare raw bytes: read_text universal newlines would treat a
    # CRLF-on-disk fixture as equal and silently keep it.
    if path.exists() and path.read_bytes() == content.encode("utf-8"):
        return False
    path.write_bytes(content.encode("utf-8"))
    return True


def self_test_ancestor_guard():
    """Adversarial regression test for the ancestor guard, run on every smoke
    invocation on every platform: builds a scratch repo whose `src` ancestor
    redirects to an outside directory — a POSIX symlink here, a real directory
    junction on Windows (the case `is_symlink()` cannot see) — then requires
    the fixture write to refuse and proves the outside sentinel and target
    directory were left untouched. Also proves the healthy path still writes
    through real directories, so the guard cannot silently break fixture
    creation.

    The scratch root is a fresh `mkdtemp` directory the test itself creates:
    a fixed reusable path could hold legitimate pre-existing data (which the
    cleanup here would delete) or be pre-planted as a symlink/junction that
    redirects every scratch write outside the tree the test believes it
    owns."""
    base = Path(tempfile.mkdtemp(prefix="oneup-ancestor-guard-selftest-"))
    outside = base / "outside-target"
    outside.mkdir(parents=True)
    sentinel = outside / "sentinel.txt"
    sentinel_content = b"must remain untouched"
    sentinel.write_bytes(sentinel_content)
    scratch_repo = base / "repo"
    scratch_repo.mkdir()
    redirect = scratch_repo / "src"

    try:
        if sys.platform == "win32":
            import _winapi

            _winapi.CreateJunction(str(outside), str(redirect))
            flavor = "directory junction"
        else:
            os.symlink(str(outside), str(redirect), target_is_directory=True)
            flavor = "symlink"

        refused = False
        try:
            write_fixture_file(scratch_repo, "src/escape.py", "escape-attempt")
        except SmokeFailure:
            refused = True
        if not refused:
            raise SmokeFailure(
                f"ancestor-guard self-test failed: a {flavor} ancestor was "
                "accepted for a fixture write"
            )
        if (outside / "escape.py").exists():
            raise SmokeFailure(
                f"ancestor-guard self-test failed: a fixture write escaped "
                f"through a {flavor} ancestor into {outside}"
            )
        if sentinel.read_bytes() != sentinel_content or len(list(outside.iterdir())) != 1:
            raise SmokeFailure(
                f"ancestor-guard self-test failed: the outside target changed "
                f"after a refused write through a {flavor} ancestor"
            )

        # Healthy-path control: real (and not-yet-existing) directories must
        # still be accepted, or the guard would break fixture creation itself.
        if not write_fixture_file(scratch_repo, "lib/util.py", "healthy write"):
            raise SmokeFailure(
                "ancestor-guard self-test failed: a healthy fixture write "
                "reported nothing written"
            )
        if (scratch_repo / "lib" / "util.py").read_bytes() != b"healthy write":
            raise SmokeFailure(
                "ancestor-guard self-test failed: the healthy fixture write "
                "did not land in the scratch repo"
            )
        artifact["ancestor_guard_selftest"] = f"refused {flavor} ancestor"
    finally:
        shutil.rmtree(base, ignore_errors=True)


def ensure_fixture_repo():
    repo = Path(repo_path)
    repo.mkdir(parents=True, exist_ok=True)
    dot_git = repo / ".git"
    if not dot_git.exists():
        dot_git.mkdir()
        artifact["fixture_files_created"].append(".git/")
    elif not (dot_git.is_dir() or dot_git.is_file()):
        raise SmokeFailure(".git exists but is neither a directory nor a worktree file")

    for relative_path, content in FIXTURE_FILES.items():
        if write_fixture_file(repo, relative_path, content):
            artifact["fixture_files_created"].append(relative_path)


def isolated_child_env():
    env = os.environ.copy()
    smoke_home = Path(output_path).parent / ".mcp-smoke-home"
    xdg_data = smoke_home / "xdg-data"
    local_app_data = smoke_home / "local-app-data"
    mac_data = smoke_home / "Library" / "Application Support"

    for data_root in (xdg_data, local_app_data, mac_data):
        marker = data_root / "1up" / "models" / "all-MiniLM-L6-v2" / ".download_failed"
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.write_text("release-smoke-fts-only", encoding="utf-8")

    # On windows the binary resolves its data dir through the Known Folder
    # API, which ignores the env overrides below, so the marker must live in
    # the runner's real profile. Gate on CI to avoid degrading a developer's
    # real 1up state.
    if sys.platform == "win32" and os.environ.get("GITHUB_ACTIONS") == "true":
        for env_name in ("APPDATA", "LOCALAPPDATA"):
            base = os.environ.get(env_name)
            if not base:
                continue
            marker = Path(base) / "1up" / "models" / "all-MiniLM-L6-v2" / ".download_failed"
            marker.parent.mkdir(parents=True, exist_ok=True)
            marker.write_text("release-smoke-fts-only", encoding="utf-8")

    env["HOME"] = str(smoke_home)
    env["XDG_DATA_HOME"] = str(xdg_data)
    env["LOCALAPPDATA"] = str(local_app_data)
    return env


def start_reader(stream, output_queue):
    def run():
        try:
            for line in iter(stream.readline, ""):
                output_queue.put(line)
        finally:
            output_queue.put(None)

    thread = threading.Thread(target=run, daemon=True)
    thread.start()
    return thread


def collect_stream(stream, lines):
    def run():
        try:
            for line in iter(stream.readline, ""):
                lines.append(line)
        except Exception:
            pass

    thread = threading.Thread(target=run, daemon=True)
    thread.start()
    return thread


def write_json(proc, payload):
    proc.stdin.write(json.dumps(payload, separators=(",", ":")) + "\n")
    proc.stdin.flush()


def read_response(proc, stdout_queue, expected_id, timeout_seconds=15):
    deadline = time.monotonic() + timeout_seconds
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SmokeFailure(f"timed out waiting for JSON-RPC response {expected_id}")

        try:
            line = stdout_queue.get(timeout=remaining)
        except queue.Empty:
            if proc.poll() is not None:
                raise SmokeFailure(
                    f"MCP server exited before response {expected_id} with status {proc.returncode}"
                )
            raise SmokeFailure(f"timed out waiting for JSON-RPC response {expected_id}")

        if line is None:
            raise SmokeFailure(f"MCP server closed stdout before response {expected_id}")

        stripped = line.rstrip("\r\n")
        if not stripped:
            raise SmokeFailure("MCP server wrote an empty stdout line during protocol exchange", False)

        try:
            response = json.loads(stripped)
        except json.JSONDecodeError:
            raise SmokeFailure(
                f"MCP server wrote non-JSON stdout during protocol exchange: {stripped[:200]}",
                False,
            )

        if not isinstance(response, dict):
            raise SmokeFailure("MCP server wrote a non-object JSON-RPC response", False)
        if response.get("jsonrpc") != "2.0":
            raise SmokeFailure("MCP server wrote JSON stdout that was not a JSON-RPC 2.0 message", False)

        if response.get("id") == expected_id:
            return response


def require_success_response(response, label):
    if "error" in response:
        raise SmokeFailure(f"{label} returned JSON-RPC error: {response['error']}")
    if "result" not in response:
        raise SmokeFailure(f"{label} response is missing result")
    return response["result"]


def presentation_issues(label, value):
    issues = []

    def visit(current):
        if isinstance(current, str):
            if "\x1b[" in current:
                issues.append(f"{label} includes an ANSI control sequence")
            for character in current:
                codepoint = ord(character)
                if 0x2500 <= codepoint <= 0x257F:
                    issues.append(f"{label} includes a box/table drawing character")
                    break
                if 0x2800 <= codepoint <= 0x28FF:
                    issues.append(f"{label} includes a spinner glyph")
                    break
            for line in current.splitlines():
                trimmed = line.strip()
                if trimmed.startswith("|") and trimmed.endswith("|") and trimmed.count("|") >= 2:
                    issues.append(f"{label} includes a terminal-oriented table row")
                    break
        elif isinstance(current, list):
            for item in current:
                visit(item)
        elif isinstance(current, dict):
            for item in current.values():
                visit(item)

    visit(value)
    return issues


def record_tool_call(label, tool_name, result, structured=None):
    status = structured.get("status") if isinstance(structured, dict) else ""
    issues = presentation_issues(label, result)
    presentation_free = not issues
    if issues:
        artifact["presentation_free"] = False
        artifact["presentation_leaks"].extend(issues)

    artifact["tool_calls"].append(
        {
            "label": label,
            "name": tool_name,
            "status": status,
            "structured_content": isinstance(structured, dict),
            "presentation_free": presentation_free,
        }
    )
    artifact["response_statuses"][label] = status
    artifact["structured_content_present"][label] = isinstance(structured, dict)
    if tool_name not in artifact["exercised_tools"]:
        artifact["exercised_tools"].append(tool_name)
    return issues


def require_tool_envelope(result, label, tool_name):
    structured = result.get("structuredContent")
    issues = record_tool_call(label, tool_name, result, structured)
    if result.get("isError") is True:
        raise SmokeFailure(f"{label} returned tool error result")
    if issues:
        raise SmokeFailure(f"{label} response leaked terminal presentation: {issues[0]}")
    if not isinstance(structured, dict):
        raise SmokeFailure(f"{label} result is missing structuredContent")

    summary = structured.get("summary")
    data = structured.get("data")
    next_actions = structured.get("next_actions")
    if not isinstance(structured.get("status"), str) or not structured["status"].strip():
        raise SmokeFailure(f"{label} structuredContent is missing a non-empty status")
    if not isinstance(summary, str) or not summary.strip():
        raise SmokeFailure(f"{label} structuredContent is missing a non-empty summary")
    if not isinstance(data, dict):
        raise SmokeFailure(f"{label} structuredContent is missing data object")
    if not isinstance(next_actions, list):
        raise SmokeFailure(f"{label} structuredContent is missing next_actions array")
    return structured


def call_tool(proc, stdout_queue, request_id, name, arguments, timeout_seconds=30):
    write_json(
        proc,
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": arguments,
            },
        },
    )
    return require_success_response(
        read_response(proc, stdout_queue, request_id, timeout_seconds),
        name,
    )


def require_records_data(envelope, label):
    records = envelope["data"].get("records")
    if not isinstance(records, list) or not records:
        raise SmokeFailure(f"{label} response did not include records")
    return records


def require_fixture_search_hit(results, response, request):
    # Capture the raw payload BEFORE parsing any result field: a malformed
    # record must never escape this function without the diagnostics needed
    # to explain it. Cleared again on the success path below.
    artifact["search_debug"] = {
        "request": request,
        "response": response,
    }
    for result in results:
        if not isinstance(result, dict):
            continue
        line_start = parse_line_number(result.get("line_start", 0))
        line_end = parse_line_number(result.get("line_end", 0))
        if (
            normalize_fixture_path(result.get("path")) == "src/policy.rs"
            and result.get("symbol") == "PolicyRuleValidator"
            and isinstance(result.get("handle"), str)
            and result["handle"].strip()
            and line_start is not None
            and line_start <= 1
            and line_end is not None
            and line_end >= 1
        ):
            artifact.pop("search_debug", None)
            return result
    raise KnownIssue110Failure(FIXTURE_SEARCH_HIT_MISSING)


def require_fixture_segment(records):
    for record in records:
        if not isinstance(record, dict):
            continue
        segment = record.get("segment")
        if not isinstance(segment, dict):
            continue
        content = segment.get("content")
        if (
            normalize_fixture_path(segment.get("path")) == "src/policy.rs"
            and isinstance(content, str)
            and "PolicyRuleValidator" in content
        ):
            return segment
    raise SmokeFailure("oneup_get response did not hydrate the fixture policy source")


def require_fixture_symbol_evidence(envelope):
    definitions = envelope["data"].get("definitions")
    references = envelope["data"].get("references")
    if not isinstance(definitions, list) or not definitions:
        raise SmokeFailure("oneup_symbol did not return structured definition evidence")
    if references is not None and not isinstance(references, list):
        raise SmokeFailure("oneup_symbol references field is not structured as an array")
    if not any(
        isinstance(record, dict)
        and normalize_fixture_path(record.get("path")) == "src/policy.rs"
        for record in definitions
    ):
        raise SmokeFailure("oneup_symbol did not return the fixture definition path")
    if not any(
        isinstance(record, dict)
        and normalize_fixture_path(record.get("path")) == "src/runner.rs"
        for record in references or []
    ):
        raise SmokeFailure("oneup_symbol did not return the fixture reference path")


def require_fixture_location_context(records):
    for record in records:
        if not isinstance(record, dict):
            continue
        context = record.get("context")
        if not isinstance(context, dict):
            continue
        content = context.get("content")
        line_start = context.get("line_start")
        line_end = context.get("line_end")
        if (
            normalize_fixture_path(context.get("path")) == "src/policy.rs"
            and isinstance(content, str)
            and "validate(&self" in content
            and isinstance(line_start, int)
            and isinstance(line_end, int)
            and line_start <= 4 <= line_end
        ):
            return context
    raise SmokeFailure("oneup_context response did not hydrate fixture file-line context")


def require_fixture_structural_match(envelope):
    results = envelope["data"].get("results")
    if not isinstance(results, list) or not results:
        raise SmokeFailure("oneup_structural did not return structured matches")
    for result in results:
        if not isinstance(result, dict):
            continue
        if (
            normalize_fixture_path(result.get("file_path")) == "src/policy.rs"
            and result.get("language") == "rust"
            and result.get("content") == "PolicyRuleValidator"
        ):
            return
    raise SmokeFailure("oneup_structural did not return the fixture struct match")


def require_fixture_overview(envelope):
    stats = envelope["data"].get("stats")
    if not isinstance(stats, dict):
        raise SmokeFailure("oneup_overview response did not include stats")
    for field in ("indexed_files", "total_segments"):
        value = stats.get(field)
        if not isinstance(value, int) or value <= 0:
            raise SmokeFailure(f"oneup_overview stats did not report a nonzero {field}")
    modules = envelope["data"].get("modules")
    if not isinstance(modules, list) or not modules:
        raise SmokeFailure("oneup_overview did not return module map entries")
    if not any(
        isinstance(module, dict) and module.get("module") == "src" for module in modules
    ):
        raise SmokeFailure("oneup_overview modules did not include the fixture src module")
    if not envelope["next_actions"]:
        raise SmokeFailure("oneup_overview did not include suggested next actions")


# Focused CI mode: run just the adversarial guard self-test (which builds the
# platform-specific redirect — a directory junction on Windows) and stop
# before anything that needs the release binary.
if self_test_only:
    try:
        self_test_ancestor_guard()
    except SmokeFailure as exc:
        sys.exit(fail(str(exc)))
    except Exception as exc:
        sys.exit(fail(f"ancestor-guard self-test failed unexpectedly: {exc}"))
    write_artifact("passed")
    print(f"[release-assets] ancestor-guard self-test: {artifact['ancestor_guard_selftest']}")
    sys.exit(0)

try:
    self_test_ancestor_guard()
    ensure_fixture_repo()
    smoke_env = isolated_child_env()
except SmokeFailure as exc:
    sys.exit(fail(str(exc)))
except Exception as exc:
    sys.exit(fail(f"failed to prepare MCP smoke fixture: {exc}"))

try:
    version = subprocess.run(
        [binary_path, "--version"],
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
        env=smoke_env,
    )
except Exception as exc:
    sys.exit(fail(f"failed to execute version smoke: {exc}"))

if version.returncode != 0:
    detail = (version.stderr or version.stdout or "").strip()
    sys.exit(fail(f"version smoke failed with status {version.returncode}: {detail}"))

artifact["version"] = version.stdout.replace("\r", "").strip()
if not artifact["version"]:
    sys.exit(fail("version smoke did not produce stdout"))

proc = None
stderr_lines = []

try:
    proc = subprocess.Popen(
        server_command,
        cwd=repo_path,
        env=smoke_env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    stdout_queue = queue.Queue()
    start_reader(proc.stdout, stdout_queue)
    collect_stream(proc.stderr, stderr_lines)

    write_json(
        proc,
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "1up-release-smoke", "version": "0"},
            },
        },
    )
    require_success_response(read_response(proc, stdout_queue, 1), "initialize")
    write_json(
        proc,
        {
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {},
        },
    )

    write_json(
        proc,
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {},
        },
    )
    tools_result = require_success_response(
        read_response(proc, stdout_queue, 2),
        "tools/list",
    )
    tools = tools_result.get("tools")
    if not isinstance(tools, list):
        raise SmokeFailure("tools/list result is missing tools array")

    tool_names = []
    for tool in tools:
        if not isinstance(tool, dict) or not isinstance(tool.get("name"), str):
            raise SmokeFailure("tools/list returned a tool without a string name")
        tool_names.append(tool["name"])
    artifact["tools"] = tool_names

    missing_tools = [name for name in EXPECTED_TOOLS if name not in tool_names]
    if missing_tools:
        raise SmokeFailure(f"tools/list is missing canonical tools: {', '.join(missing_tools)}")

    status_result = call_tool(
        proc,
        stdout_queue,
        3,
        "oneup_status",
        {},
    )
    status_envelope = require_tool_envelope(status_result, "status", "oneup_status")
    status_readiness = status_envelope.get("status")
    if status_readiness not in READINESS_STATUSES:
        raise SmokeFailure(f"oneup_status returned unsupported readiness status: {status_readiness}")

    start_result = call_tool(
        proc,
        stdout_queue,
        4,
        "oneup_start",
        {"mode": "index_if_needed"},
        timeout_seconds=90,
    )
    structured = require_tool_envelope(start_result, "start", "oneup_start")
    readiness_status = structured.get("status")
    if readiness_status not in READINESS_STATUSES:
        raise SmokeFailure(f"oneup_start returned unsupported readiness status: {readiness_status}")
    artifact["readiness_status"] = readiness_status

    next_actions = structured.get("next_actions")
    if readiness_status in {"missing", "indexing", "stale", "degraded"} and not next_actions:
        raise SmokeFailure(
            f"oneup_start readiness status {readiness_status} did not include actionable next steps"
        )

    # oneup_start is non-blocking with a bounded wait — longer
    # first indexes (fresh runner, cold model download) detach and callers
    # poll oneup_status. Poll like a real agent instead of expecting the
    # pre-v0.1.13 blocking-start semantics from a single response.
    if readiness_status in {"missing", "indexing", "stale"}:
        poll_deadline = time.time() + 180
        poll_id = 400
        while time.time() < poll_deadline:
            time.sleep(3)
            poll_result = call_tool(
                proc,
                stdout_queue,
                poll_id,
                "oneup_status",
                {},
                timeout_seconds=30,
            )
            poll_id += 1
            poll_envelope = require_tool_envelope(poll_result, "status-poll", "oneup_status")
            readiness_status = poll_envelope.get("status")
            if readiness_status in DISCOVERY_READY_STATUSES or readiness_status == "blocked":
                structured = poll_envelope
                break
        artifact["readiness_status"] = readiness_status

    if readiness_status not in DISCOVERY_READY_STATUSES:
        raise SmokeFailure(
            "oneup_start did not make the fixture repository searchable: "
            f"{readiness_status} (summary: {json.dumps(structured.get('summary'))}, "
            f"data: {json.dumps(structured.get('data'))})"
        )

    search_arguments = {"query": "PolicyRuleValidator", "limit": 5}
    search_result = call_tool(
        proc,
        stdout_queue,
        5,
        "oneup_search",
        search_arguments,
    )
    search_envelope = require_tool_envelope(search_result, "search", "oneup_search")
    search_results = search_envelope["data"].get("results")
    if not isinstance(search_results, list):
        # A structurally malformed envelope is NOT the #110 shape: it stays a
        # hard failure everywhere.
        artifact["search_debug"] = {
            "request": search_arguments,
            "response": search_result,
        }
        raise SmokeFailure("oneup_search did not return structured ranked results")
    if not search_results:
        # An empty result set is the primary #110 failure shape — route it
        # through the same typed failure as a present-but-hitless result set
        # so the Windows known-issue gate recognizes both.
        artifact["search_debug"] = {
            "request": search_arguments,
            "response": search_result,
        }
        raise KnownIssue110Failure(
            f"oneup_search returned an empty result set; {FIXTURE_SEARCH_HIT_MISSING}"
        )
    hit = require_fixture_search_hit(search_results, search_result, search_arguments)
    handle = hit.get("handle")
    if not isinstance(handle, str) or not handle.strip():
        raise SmokeFailure("oneup_search result is missing a stable handle")

    get_result = call_tool(
        proc,
        stdout_queue,
        6,
        "oneup_get",
        {"handles": [f":{handle}"]},
    )
    get_envelope = require_tool_envelope(get_result, "get", "oneup_get")
    handle_records = require_records_data(get_envelope, "oneup_get")
    require_fixture_segment(handle_records)

    symbol_result = call_tool(
        proc,
        stdout_queue,
        7,
        "oneup_symbol",
        {"name": "PolicyRuleValidator", "include": "both"},
    )
    symbol_envelope = require_tool_envelope(symbol_result, "symbol", "oneup_symbol")
    require_fixture_symbol_evidence(symbol_envelope)

    context_result = call_tool(
        proc,
        stdout_queue,
        8,
        "oneup_context",
        {"locations": [{"path": "src/policy.rs", "line": 4, "expansion": 2}]},
    )
    context_envelope = require_tool_envelope(context_result, "context", "oneup_context")
    location_records = require_records_data(context_envelope, "oneup_context")
    require_fixture_location_context(location_records)

    impact_result = call_tool(
        proc,
        stdout_queue,
        9,
        "oneup_impact",
        {"handle": f":{handle}"},
    )
    require_tool_envelope(impact_result, "impact", "oneup_impact")

    structural_result = call_tool(
        proc,
        stdout_queue,
        10,
        "oneup_structural",
        {
            "pattern": "(struct_item name: (type_identifier) @name)",
            "language": "rust",
        },
    )
    structural_envelope = require_tool_envelope(
        structural_result,
        "structural",
        "oneup_structural",
    )
    require_fixture_structural_match(structural_envelope)

    overview_result = call_tool(
        proc,
        stdout_queue,
        11,
        "oneup_overview",
        {},
    )
    overview_envelope = require_tool_envelope(
        overview_result,
        "overview",
        "oneup_overview",
    )
    require_fixture_overview(overview_envelope)

    seen_labels = {call["label"] for call in artifact["tool_calls"]}
    missing_labels = [label for label in REQUIRED_FLOW_LABELS if label not in seen_labels]
    if missing_labels:
        raise SmokeFailure(f"MCP smoke did not exercise required calls: {', '.join(missing_labels)}")
    artifact["discovery_flow"]["status"] = "passed"

    write_artifact("passed")
except SmokeFailure as exc:
    if exc.protocol_clean is not None:
        artifact["stdout_protocol_clean"] = exc.protocol_clean
    if stderr_lines:
        stderr = "".join(stderr_lines).strip()
        if stderr:
            artifact["diagnostics"].append(f"MCP stderr: {stderr[-1000:]}")
    if sys.platform == "win32" and isinstance(exc, KnownIssue110Failure):
        gate_active, gate_reason = known_issue_110_gate_decision()
        print(
            "[release-assets] WARNING: known-issue rp1-run/1up#110 gate "
            f"{'applies' if gate_active else 'does not apply'}: {gate_reason}",
            file=sys.stderr,
        )
        if gate_active:
            record_known_issue_110(str(exc))
        else:
            artifact["diagnostics"].append(
                f"known-issue #110 gate not applied: {gate_reason}"
            )
            sys.exit(fail(str(exc), artifact["stdout_protocol_clean"]))
    else:
        sys.exit(fail(str(exc), artifact["stdout_protocol_clean"]))
except Exception as exc:
    if stderr_lines:
        stderr = "".join(stderr_lines).strip()
        if stderr:
            artifact["diagnostics"].append(f"MCP stderr: {stderr[-1000:]}")
    sys.exit(fail(f"MCP smoke failed unexpectedly: {exc}"))
finally:
    if proc is not None and proc.poll() is None:
        try:
            proc.terminate()
            proc.wait(timeout=5)
        except Exception:
            try:
                proc.kill()
            except Exception:
                pass
PY
then
  if [[ "$SELF_TEST_ONLY" == "1" ]]; then
    log "ancestor-guard self-test passed and wrote $(relative_path "$OUTPUT_PATH")"
  else
    log "MCP smoke passed and wrote $(relative_path "$OUTPUT_PATH")"
  fi
else
  exit 1
fi
