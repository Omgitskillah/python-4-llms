# Development log

## 2026-10-05 — Initial implementation, contract and Docker policy

- Started from the empty `Omgitskillah/python-4-llms` repository, cloned as an independent checkout in RemoteNotes.
- Chose a Rust stdio MCP server using the existing math server's pinned `rmcp` and `schemars` versions.
- Added strict, bounded inputs; versioned responses; a shared output budget; and a standard-library Python input adapter.
- Pinned the official multi-platform Python 3.14.8 slim-bookworm base image by the digest returned by Docker Hub's official tag endpoint.
- Added fixed Docker configuration, local Unix-socket access only, runtime capability checks, and inspection of applied container permissions before execution.
- Docker Desktop installation was requested by the user and is in progress. Verification is pending until the initial implementation is complete.

## 2026-10-05 — Supervisor and MCP lifecycle

- Added a separate host supervisor for each job, observing a parent liveness pipe and enforcing execution deadlines outside submitted Python.
- Kept a single execution permit through cancellation and verified container removal. Unconfirmed cleanup disables further execution until recovery/restart.
- Added bounded supervisor and MCP frames, streamed output capture, Docker exit/OOM inspection, and graceful signal shutdown.
- The initial Docker Desktop installation failed because Homebrew attempted administrator-only CLI links; retrying with `--no-binaries` to use the app's bundled CLI.
- Verification remains pending while the initial lifecycle implementation is assembled.

## 2026-10-05 — First compile and Docker installation checks

- `cargo test --offline --manifest-path python-4-llms/Cargo.toml` could not resolve uncached `rand_chacha`; retried with authorized network access.
- `cargo test --manifest-path python-4-llms/Cargo.toml` downloaded dependencies and identified Rust temporary-lifetime and collector-borrow errors; corrected stream bindings and moved result buffers into the collector.
- Bounded Docker diagnostic messages to 1024 bytes so infrastructure failures cannot inflate tool responses.
- Homebrew's `--no-binaries` retry still required an administrator-only kubectl link. Mounted the official cached Docker DMG and switched to direct app installation with its bundled CLI.

## 2026-10-05 — Deterministic lifecycle regression coverage

- Added a fake Docker control plane that never executes submitted source. It tests success, script errors, OOM classification, output overflow, deadlines, policy rejection, failed creation, and failed cleanup without timing-dependent container behavior.
- Added explicit checks that each critical applied permission/resource setting is verified before startup.
- Added expiry labels and recovery that preserves active jobs from other server instances; failed creation is conservatively classified as unconfirmed cleanup because daemon-side creation can outlive its CLI request.
- Added a fail-closed latch for an unexpected coordinator failure. Tests for these changes are pending the next test run.

## 2026-10-05 — Lifecycle verification and protocol/live-container suites

- `cargo test --locked --offline --manifest-path python-4-llms/Cargo.toml` passed six unit tests and five deterministic lifecycle tests (11 total).
- Docker Desktop was copied from the official DMG and launched. Computer Use access was unavailable, but a subsequent authorized Docker CLI check confirmed the engine is running with seccomp, memory/swap, CPU quota, and PID limits enabled.
- Added stdio MCP tests for discovery, schemas, result fallback, validation, error propagation, busy/cancellation behavior, cleanup failure latching, client EOF, and parent SIGKILL cleanup.
- Added an explicitly enabled real-Docker integration suite for standard input, host sentinel/secret isolation, privilege checks, networking, fresh state, script exceptions, time/output/memory limits, bounded scratch storage, and PID exhaustion.
- The pinned sandbox image is building; new protocol and live-container tests are pending.

## 2026-10-05 — Live verification identified a supervisor flush defect

- The pinned Docker image built successfully on the local Apple Silicon Docker engine.
- The first full test run passed unit/lifecycle checks and three of five protocol tests, but success/cancellation tests intermittently received an invalid supervisor result. The success test passed when run alone.
- The first real-Docker run passed standard input/execution, host sentinel and secret confinement, UID/capability/seccomp checks, blocked networking, fresh state, exceptions, and infinite-loop termination. It failed while receiving the large output-limit response.
- Root cause: Tokio stdout can retain its last queued write when `process::exit` runs. Added an explicit flush before supervisor exit and a full-output protocol regression test. Both suites will be rerun.

## 2026-10-05 — Successful sandbox checks and packaging

- After the flush fix, `cargo test --locked --offline --manifest-path python-4-llms/Cargo.toml` passed six unit, five lifecycle, and six stdio protocol tests (17 total); the real-Docker suite remained explicitly ignored in the ordinary run.
- `cargo clippy --locked --offline --manifest-path python-4-llms/Cargo.toml --all-targets -- -D warnings` passed.
- The explicitly enabled real-Docker suite passed all initial scenarios on Docker Engine 29.8.2 / Apple Silicon, including output, OOM, scratch quota, and PID quota.
- Added a real child-workload timeout/removal check, a host-path symlink probe, bounded MCP-frame regression coverage, and explicit CLI argument/help behavior.
- Added setup/client configuration, limits, failure/recovery documentation, and CI that runs both deterministic and real-Docker suites. CI is configured but has not been run remotely.
- These final additions require their own verification pass before delivery.

## 2026-10-05 — Bounded transport regression found an AsyncRead contract issue

- Final formatting and Clippy checks passed, but the new oversized-frame unit test found that returning an I/O error after filling the caller's `ReadBuf` violates Tokio's AsyncRead contract.
- Restored the previous filled length before returning the limit error and latched the over-limit state to reject subsequent reads.
- Clarified the tool description: the Python standard library is available, and package installation is not a tool capability; interpreter import filtering is not presented as the sandbox boundary.
- The release build is in progress; the corrected transport test and all final checks will be rerun.

## 2026-10-05 — Final verification and review

- Final cargo test --locked --offline --manifest-path python-4-llms/Cargo.toml passed 18 deterministic checks: six library unit tests, one bounded-transport binary test, five lifecycle tests, and six MCP protocol tests.
- With PYTHON_MCP_DOCKER pointing to the Docker Desktop bundled CLI and PYTHON_MCP_IMAGE=sha256:56357ac130fd71a8a09d8c9d72feedc215c8adbfc1de59680f9e256b8337ab14, cargo test --locked --offline --manifest-path python-4-llms/Cargo.toml --test docker_sandbox -- --ignored --nocapture passed the real-container suite, including symlink confinement and complete removal of a timed-out child workload.
- cargo fmt --manifest-path python-4-llms/Cargo.toml --all --check passed.
- cargo clippy --locked --offline --manifest-path python-4-llms/Cargo.toml --all-targets -- -D warnings passed.
- cargo build --locked --offline --release --manifest-path python-4-llms/Cargo.toml passed after the final source changes; target/release/python-mcp --help exited successfully.
- Reviewed the complete staged change set and git diff --cached --check passed. All 20 new files belong to this implementation, its contract/setup docs, tests, lockfile, or CI.
- Changes are staged in the independent python-4-llms checkout. No commit or push was made. GitHub Actions is configured but has not been run remotely; Linux-specific runtime behavior has not been verified locally.


## 2026-10-06: script source in error diagnostics

Unhandled errors now show the submitted `script.py` source line and exception,
without adapter frames. Source is registered in linecache without writing files.
Chained exceptions and exception groups retain script/library frames; syntax
carets, partial stdout, exit status and the MCP result schema remain intact.
Five focused adapter tests and a real Docker/MCP diagnostic test cover runtime,
syntax, chained/grouped errors and exit behavior. The system-python group test
handles Python before 3.11; the actual pinned Python 3.14 container exercises groups.
A rebuilt immutable sandbox image is required; old images keep old diagnostics.
