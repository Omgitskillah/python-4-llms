# python-4-llms

A local stdio MCP server exposing **`run_python`**. Each call runs a Python script
in a fresh, restricted Docker container and returns a versioned structured
result. Python's standard library is available; no additional analytics packages
are installed and the tool does not expose package installation.

The Rust server and a separate host supervisor enforce the sandbox policy,
deadlines, output limits, and cleanup. The Python input adapter is not a security
boundary. There is no HTTP endpoint or vault integration.

## Setup on macOS

Install stable Rust and Docker Desktop, complete Docker's first-run setup, and
start the engine. Run these commands from this repository:

```sh
export PYTHON_MCP_DOCKER=/Applications/Docker.app/Contents/Resources/bin/docker
export PYTHON_MCP_DOCKER_SOCKET="$HOME/.docker/run/docker.sock"
"$PYTHON_MCP_DOCKER" --host "unix://$PYTHON_MCP_DOCKER_SOCKET" build \
  --file sandbox/Dockerfile --tag python-4-llms-sandbox:0.1.0 .
export PYTHON_MCP_IMAGE="$("$PYTHON_MCP_DOCKER" \
  --host "unix://$PYTHON_MCP_DOCKER_SOCKET" image inspect \
  --format '{{.Id}}' python-4-llms-sandbox:0.1.0)"
cargo build --locked --release
```

The base image is pinned by digest. `PYTHON_MCP_IMAGE` must be the full immutable
`sha256:` ID of the built image, not its tag. Rebuilding the image requires
updating this value in the MCP client configuration.

On Linux, Docker is discovered on `PATH`; the default socket is
`/var/run/docker.sock`. For rootless Docker, set `PYTHON_MCP_DOCKER_SOCKET` to its
absolute local socket path. The engine must report support for seccomp, memory,
swap, CPU quota, and PID limits; unsupported runtimes fail closed. A recent Docker
engine supporting `seccomp=builtin` is required (verified locally on 29.8.2).
Remote Docker contexts and `DOCKER_HOST` are deliberately ignored.

## MCP client configuration

Replace the paths and image ID with your own values:

```json
{
  "mcpServers": {
    "python": {
      "command": "/absolute/path/python-4-llms/target/release/python-mcp",
      "args": [],
      "env": {
        "PYTHON_MCP_DOCKER": "/Applications/Docker.app/Contents/Resources/bin/docker",
        "PYTHON_MCP_DOCKER_SOCKET": "/Users/YOU/.docker/run/docker.sock",
        "PYTHON_MCP_IMAGE": "sha256:REPLACE_WITH_64_HEX_DIGITS"
      }
    }
  }
}
```

The server speaks MCP on stdin/stdout. Startup diagnostics go to stderr, and
Python output is captured inside tool results. Launch it through your MCP client
using its normal tool permission controls.

## Tool contract

```json
{
  "code": "import json, sys\nvalues = json.load(sys.stdin)\nprint(sum(values))",
  "stdin": "[1, 2, 3]",
  "timeout_ms": 10000
}
```

Only `code` is required. `stdin` defaults to an empty string and is exposed as
text through `sys.stdin`. The script executes as `__main__` with `__file__` and
`sys.argv[0]` set to `script.py`. Working directory is `/tmp`. Each call starts
with fresh state; return results by printing text or JSON to stdout.

Example structured result (duration varies):

```json
{
  "schema_version": 1,
  "ok": true,
  "status": "SUCCESS",
  "stdout": "6\n",
  "stderr": "",
  "exit_code": 0,
  "duration_ms": 120,
  "output_truncated": false,
  "message": null
}
```

The MCP response includes both `structuredContent` and an equivalent JSON text
fallback. Failures set `isError: true`. Malformed parameter shapes are rejected
by the MCP SDK; valid parameter shapes that exceed limits return `INVALID_INPUT`.
Read `ok` and `status` before using output, since failures can include partial
stdout/stderr. Unhandled script errors include the failing `script.py` source
line and exception type. Input-adapter frames are omitted, including in chained
exceptions and exception groups; script and library frames remain. Syntax errors
retain their source line and caret. These diagnostics do not change exit codes
or the `SCRIPT_ERROR` result contract.

| Limit | Value |
| --- | --- |
| Python source | 1–65,536 UTF-8 bytes |
| Standard input | 262,144 UTF-8 bytes |
| Execution wall time | 10,000 ms default; 1–30,000 ms accepted |
| Combined stdout/stderr | 65,536 raw bytes |
| Memory | 256 MiB; swap disabled |
| CPU | 1 CPU |
| Processes/threads | 16, including init and Python |
| Scratch storage | 64 MiB `/tmp`; 64 KiB `/dev/shm` |
| Open files | 64 per process |
| Concurrent jobs | 1 per server process; additional calls return `BUSY` |

Time and `duration_ms` include Docker attach/start and script execution, but not
preparation or cleanup. Docker control commands have separate five-second
deadlines. Output beyond its combined limit terminates the job; binary output is
decoded with UTF-8 replacement characters, so its returned UTF-8 size can exceed
the raw-byte budget. No output files or persistent sessions are exported.

Statuses are `SUCCESS`, `SCRIPT_ERROR`, `TIMEOUT`, `OUTPUT_LIMIT`, `MEMORY_LIMIT`,
`CANCELLED`, `BUSY`, `INVALID_INPUT`, `SANDBOX_UNAVAILABLE`, `SANDBOX_ERROR`, and
`CLEANUP_FAILED`. `exit_code` is null for interrupted execution or infrastructure
failures. Kernel OOM kills return `MEMORY_LIMIT`; Python-raised `MemoryError`, PID
exhaustion, and filesystem errors are ordinary `SCRIPT_ERROR` unless the script
handles them itself. MCP cancellation normally suppresses the cancelled response;
cleanup still runs.

## Security and recovery

Every container uses a non-root UID, no Linux capabilities, no privilege
escalation, Docker's built-in seccomp profile, disabled external networking, and a
read-only root filesystem. Scratch mounts are bounded and use `noexec`, `nosuid`,
and `nodev`. There are no host directory, secret, or Docker-socket mounts. Script
text is sent as JSON over a pipe, never interpolated into a host shell command.
Caller input cannot select an image, mount, environment, or Docker option.

The server verifies engine support and inspects the actual container settings
before starting Python. It uses only the configured local Unix socket and image
ID, clears host environment variables from supervisor/Docker subprocesses, and
disables Docker output logging. The tool does not intentionally persist source or
results to host logs; MCP clients may retain their conversation/tool history.

Cancellation and normal shutdown hold the execution permit until container
removal is verified. A separate supervisor watches the MCP process's pipe, so
killing the MCP process also triggers cleanup. If Docker is unavailable or the
supervisor itself fails, cleanup may remain unconfirmed; subsequent calls are
blocked. Docker isolation depends on the engine/VM and kernel and is intended for
local use with a trusted MCP client, not a public multi-tenant execution service.

Each job carries a 120-second expiry label for recovery. Server startup removes
expired tool-owned containers while preserving active jobs from other instances.
After an unconfirmed cleanup, restore Docker connectivity, wait until the job's
expiry, run recovery with the same environment, then restart the MCP server:

```sh
./target/release/python-mcp --recover
```

Recovery only touches containers with this tool's ownership/expiry labels and
name prefix. An unavailable or invalid image, missing runtime protection, or
unverifiable cleanup produces a diagnostic rather than falling back to host
Python execution.

## Verification

```sh
cargo test --locked
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
```

Ordinary tests use a deterministic Docker fixture that never executes submitted
code. Protocol tests launch the actual MCP binary. They cover cancellation,
disconnect, parent SIGKILL, failed cleanup, recovery, schemas, and bounded data.
These tests require `python3` on PATH and run on Unix systems.

With Docker running and the setup environment exported, explicitly run the real
container tests:

```sh
cargo test --locked --test docker_sandbox -- --ignored --nocapture
```

This suite exercises real confinement, privileges, blocked networking, fresh
state, exceptions, and time/output/memory/storage/PID limits. GitHub Actions runs
both suites, formatting, and Clippy on Linux. Verification records are in
[docs/development-log.md](docs/development-log.md).
