# Eval Record: Codex SDK / Luna Suite Prepared, Not Run

**Date:** 2026-07-13
**Status:** NOT RUN - configuration and local harness validation only
**Model:** `gpt-5.6-luna`
**Promptfoo:** `0.121.18`
**Codex SDK:** `0.144.1`
**Installed 1up:** `/Users/prem/.local/bin/1up`, version `0.1.15`

## Deliberate no-run status

The Luna search and impact suites invoke a credentialed model and may incur
cost. They were not executed during automated preparation. Recall evals were
also not run because they are model-enabled manual gates.

## Prepared surfaces

- Existing Claude configs and commands are preserved.
- Separate Luna configs use the Codex SDK and `gpt-5.6-luna`.
- Each 1up-agent test receives its disposable `WORKSPACE_DIR` as both the
  Codex working directory and the `1up mcp --path` target.
- The Codex `cli_config` contains the complete `mcp_servers.oneup` definition;
  the baseline provider does not receive the MCP server.
- Shared assertions normalize ordered Codex `mcp_tool_call` and
  `command_execution` items while retaining Claude `metadata.toolCalls`.
- The provider-selectable runner fails closed and retains per-test logs for the
  provider-selectable summary.

## Exact manual command

From an authenticated secure shell:

```sh
cd /path/to/1up/evals
bun install --frozen-lockfile
test "$(1up --version | awk '{print $3}')" = "0.1.15"
codex login status
PROMPTFOO_CACHE_ENABLED=false npm run eval:parallel:luna
npm run eval:summary:luna
```

The Codex SDK reuses the existing ChatGPT login when explicit Codex/OpenAI API
keys are unset. Outputs are retained under `evals/results/latest-luna/`.

## Dependency reproducibility validation

The committed Bun graph was accepted without mutation by:

```sh
bun install --frozen-lockfile --lockfile-only --offline
```

It contains 893 packages and resolves the direct pins to Promptfoo `0.121.18`
and Codex SDK `0.144.1`. An independent clean-directory npm install also
completed with zero audit findings and installed those exact direct versions.

A stronger empty-cache Bun installation was attempted twice, once with an
isolated cache and once with the normal cache. Bun `1.3.14` failed while
extracting many unrelated Artifactory tarballs with `ZlibError` / `Fail
extracting tarball`; it did not report a frozen-lockfile mismatch. A direct
request for the pinned Promptfoo tarball returned `application/x-gzip` with
Artifactory checksums, while direct public npm access is redirected by the
machine's dependency-confusion network policy. This bounds the remaining
uncertainty to Bun's tarball extraction on this workstation, not the lock graph
or requested direct dependency versions. Re-run the documented frozen Bun
install on the target secure shell before spending model credentials.
