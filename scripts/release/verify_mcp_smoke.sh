#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=./common.sh
source "$SCRIPT_DIR/common.sh"

BINARY_PATH=""
REPO_PATH=""
OUTPUT_PATH=""

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
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

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

BINARY_PATH=$(cd "$(dirname "$BINARY_PATH")" && pwd -P)/$(basename "$BINARY_PATH")
REPO_PATH=$(cd "$REPO_PATH" && pwd -P)
mkdir -p "$(dirname "$OUTPUT_PATH")"
OUTPUT_PATH=$(cd "$(dirname "$OUTPUT_PATH")" && pwd -P)/$(basename "$OUTPUT_PATH")

if "${PYTHON_CMD[@]}" - "$BINARY_PATH" "$REPO_PATH" "$OUTPUT_PATH" <<'PY'
import json
import os
import queue
import subprocess
import sys
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


binary_path = sys.argv[1]
repo_path = sys.argv[2]
output_path = sys.argv[3]
server_command = [binary_path, "mcp", "--path", repo_path]
artifact = {
    "schema": "mcp_smoke.v2",
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
        path = repo / relative_path
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.exists():
            existing = path.read_text(encoding="utf-8")
            if existing != content:
                raise SmokeFailure(
                    f"fixture file already exists with different content: {relative_path}"
                )
            continue
        path.write_text(content, encoding="utf-8")
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


def require_fixture_search_hit(results):
    for result in results:
        if not isinstance(result, dict):
            continue
        if (
            result.get("path") == "src/policy.rs"
            and result.get("symbol") == "PolicyRuleValidator"
            and isinstance(result.get("handle"), str)
            and result["handle"].strip()
            and int(result.get("line_start", 0)) <= 1
            and int(result.get("line_end", 0)) >= 1
        ):
            return result
    raise SmokeFailure("oneup_search did not return the fixture PolicyRuleValidator hit")


def require_fixture_segment(records):
    for record in records:
        if not isinstance(record, dict):
            continue
        segment = record.get("segment")
        if not isinstance(segment, dict):
            continue
        content = segment.get("content")
        if (
            segment.get("path") == "src/policy.rs"
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
        isinstance(record, dict) and record.get("path") == "src/policy.rs"
        for record in definitions
    ):
        raise SmokeFailure("oneup_symbol did not return the fixture definition path")
    if not any(
        isinstance(record, dict) and record.get("path") == "src/runner.rs"
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
            context.get("path") == "src/policy.rs"
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
            result.get("file_path") == "src/policy.rs"
            and result.get("language") == "rust"
            and result.get("content") == "PolicyRuleValidator"
        ):
            return
    raise SmokeFailure("oneup_structural did not return the fixture struct match")


try:
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
    if readiness_status not in DISCOVERY_READY_STATUSES:
        raise SmokeFailure(
            f"oneup_start did not make the fixture repository searchable: {readiness_status}"
        )

    search_result = call_tool(
        proc,
        stdout_queue,
        5,
        "oneup_search",
        {"query": "PolicyRuleValidator", "limit": 5},
    )
    search_envelope = require_tool_envelope(search_result, "search", "oneup_search")
    search_results = search_envelope["data"].get("results")
    if not isinstance(search_results, list) or not search_results:
        raise SmokeFailure("oneup_search did not return structured ranked results")
    hit = require_fixture_search_hit(search_results)
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
  log "MCP smoke passed and wrote $(relative_path "$OUTPUT_PATH")"
else
  exit 1
fi
