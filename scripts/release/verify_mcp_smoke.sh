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
    "oneup_prepare",
    "oneup_search",
    "oneup_read",
    "oneup_symbol",
    "oneup_impact",
]
READINESS_STATUSES = {"missing", "indexing", "stale", "ready", "degraded"}


class SmokeFailure(Exception):
    def __init__(self, message, protocol_clean=None):
        super().__init__(message)
        self.protocol_clean = protocol_clean


binary_path = sys.argv[1]
repo_path = sys.argv[2]
output_path = sys.argv[3]
server_command = [binary_path, "mcp", "--path", repo_path]
artifact = {
    "status": "failed",
    "binary": binary_path,
    "version": "",
    "server_command": server_command,
    "tools": [],
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


try:
    version = subprocess.run(
        [binary_path, "--version"],
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
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

    write_json(
        proc,
        {
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "oneup_prepare",
                "arguments": {},
            },
        },
    )
    prepare_result = require_success_response(
        read_response(proc, stdout_queue, 3),
        "oneup_prepare",
    )
    structured = prepare_result.get("structuredContent")
    if not isinstance(structured, dict):
        raise SmokeFailure("oneup_prepare result is missing structuredContent")

    readiness_status = structured.get("status")
    if readiness_status not in READINESS_STATUSES:
        raise SmokeFailure(f"oneup_prepare returned unsupported readiness status: {readiness_status}")
    artifact["readiness_status"] = readiness_status

    summary = structured.get("summary")
    data = structured.get("data")
    next_actions = structured.get("next_actions")
    if not isinstance(summary, str) or not summary.strip():
        raise SmokeFailure("oneup_prepare structuredContent is missing a non-empty summary")
    if not isinstance(data, dict):
        raise SmokeFailure("oneup_prepare structuredContent is missing data object")
    if not isinstance(next_actions, list):
        raise SmokeFailure("oneup_prepare structuredContent is missing next_actions array")
    if readiness_status in {"missing", "indexing", "stale", "degraded"} and not next_actions:
        raise SmokeFailure(
            f"oneup_prepare readiness status {readiness_status} did not include actionable next steps"
        )

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
