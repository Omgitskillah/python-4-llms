//! The supervisor is a separate host process. Parent EOF cancels the job even
//! when the MCP process is killed; submitted Python cannot reach this process.
use crate::{
    contract::{
        MAX_JOB_MESSAGE_BYTES, MAX_RESPONSE_BYTES, Output, RunPythonRequest, RunPythonResponse,
        Status,
    },
    docker::{Config, check_container, create_args, strings, unavailable},
};
use serde::{Deserialize, Serialize};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub config: Config,
    pub request: RunPythonRequest,
}

pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: u64,
) -> Result<Vec<u8>, String> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf().await.map_err(|e| e.to_string())?;
        if buffer.is_empty() {
            return Err("Job pipe closed before a complete request".into());
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(buffer.len(), |n| n + 1);
        if line.len() as u64 + count as u64 > limit {
            return Err("Job request exceeds its byte limit".into());
        }
        line.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(line);
        }
    }
}

pub async fn supervise_stdio() -> Result<(), String> {
    let mut input = BufReader::new(tokio::io::stdin());
    let frame = tokio::time::timeout(
        Duration::from_secs(5),
        read_frame(&mut input, MAX_JOB_MESSAGE_BYTES),
    )
    .await
    .map_err(|_| "Timed out waiting for job request")??;
    let job: Job = serde_json::from_slice(&frame).map_err(|e| e.to_string())?;
    let stop = CancellationToken::new();
    let parent_stop = stop.clone();
    let parent_monitor = tokio::spawn(async move {
        let mut byte = [0];
        // EOF, pipe error, or an explicit byte all mean cancellation.
        let _ = input.read(&mut byte).await;
        parent_stop.cancel();
    });
    let response = execute(job, stop).await;
    let mut encoded = serde_json::to_vec(&response).map_err(|e| e.to_string())?;
    encoded.push(b'\n');
    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(&encoded)
        .await
        .map_err(|e| e.to_string())?;
    // Tokio stdout may acknowledge the final write while a blocking worker
    // still owns its bytes. Flush before the dedicated process exits.
    stdout.flush().await.map_err(|e| e.to_string())?;
    // Tokio stdin uses a blocking reader. Exit the dedicated process rather
    // than waiting for that thread while its parent intentionally holds the pipe.
    parent_monitor.abort();
    Ok(())
}

pub async fn execute(job: Job, stop: CancellationToken) -> RunPythonResponse {
    if let Err(response) = job.request.validate() {
        return response;
    }
    if let Err(error) = job.config.preflight().await {
        return unavailable(error);
    }
    if stop.is_cancelled() {
        return RunPythonResponse::error(Status::Cancelled, "Job cancelled before creation");
    }
    let name = format!(
        "python-4-llms-{}",
        ulid::Ulid::new().to_string().to_lowercase()
    );
    let created = job.config.control(&create_args(&job.config, &name)).await;
    let creation_uncertain = created.is_err();
    let mut response = match created {
        Err(error) => RunPythonResponse::error(Status::SandboxError, error),
        Ok(_) => match job
            .config
            .inspect(&name)
            .await
            .and_then(|value| check_container(&value, &job.config))
        {
            Err(error) => RunPythonResponse::error(Status::SandboxUnavailable, error),
            Ok(()) if stop.is_cancelled() => {
                RunPythonResponse::error(Status::Cancelled, "Job cancelled before execution")
            }
            Ok(()) => run_attached(&job, &name, stop).await,
        },
    };
    if let Err(error) = job.config.remove(&name).await {
        response.set_status(
            Status::CleanupFailed,
            format!("Could not verify container removal: {error}"),
        );
        response.exit_code = None;
    } else if creation_uncertain {
        // A timed-out create request may finish in the daemon after the CLI
        // exits. Never start it, and require recovery before admitting new work.
        response.set_status(Status::CleanupFailed, "Container creation failed; its outcome is unconfirmed. Run recovery and restart before retrying.");
    }
    response
}

async fn run_attached(job: &Job, name: &str, stop: CancellationToken) -> RunPythonResponse {
    let started = Instant::now();
    let mut command = job
        .config
        .command(&strings(&["start", "--attach", "--interactive", name]));
    command.stdin(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return RunPythonResponse::error(Status::SandboxError, error.to_string()),
    };
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let payload = serde_json::to_vec(
        &serde_json::json!({"code":job.request.code, "stdin":job.request.stdin}),
    )
    .expect("serialize strings");
    let write = async move {
        stdin.write_all(&payload).await?;
        stdin.shutdown().await
    };
    tokio::pin!(write);
    let deadline = tokio::time::sleep(Duration::from_millis(job.request.timeout_ms));
    tokio::pin!(deadline);
    let mut output = Output::default();
    let mut out_buffer = [0; 4096];
    let mut err_buffer = [0; 4096];
    let (mut out_done, mut err_done, mut input_done) = (false, false, false);
    let mut exit = None;
    let mut response = loop {
        if out_done && err_done && exit.is_some() {
            break RunPythonResponse::error(Status::SandboxError, "Inspecting Python exit status");
        }
        tokio::select! {
            biased;
            _ = stop.cancelled() => break RunPythonResponse::error(Status::Cancelled, "Job cancelled"),
            _ = &mut deadline => break RunPythonResponse::error(Status::Timeout, "Python exceeded its wall-clock limit"),
            result = &mut write, if !input_done => {
                input_done = true;
                if let Err(error) = result {
                    break RunPythonResponse::error(Status::SandboxError, format!("Could not deliver script: {error}"));
                }
            }
            result = stdout.read(&mut out_buffer), if !out_done => match result {
                Ok(0) => out_done = true,
                Ok(n) => if output.push(false, &out_buffer[..n]) {
                    break RunPythonResponse::error(Status::OutputLimit, "Combined stdout/stderr exceeded 65536 bytes");
                },
                Err(error) => break RunPythonResponse::error(Status::SandboxError, error.to_string()),
            },
            result = stderr.read(&mut err_buffer), if !err_done => match result {
                Ok(0) => err_done = true,
                Ok(n) => if output.push(true, &err_buffer[..n]) {
                    break RunPythonResponse::error(Status::OutputLimit, "Combined stdout/stderr exceeded 65536 bytes");
                },
                Err(error) => break RunPythonResponse::error(Status::SandboxError, error.to_string()),
            },
            result = child.wait(), if exit.is_none() => match result {
                Ok(status) => exit = Some(status),
                Err(error) => break RunPythonResponse::error(Status::SandboxError, error.to_string()),
            },
        }
    };
    response.duration_ms = started.elapsed().as_millis() as u64;
    output.apply(&mut response);
    if out_done && err_done && exit.is_some() {
        match job.config.inspect(name).await {
            Ok(container) if container["State"]["OOMKilled"] == true => {
                response.set_status(Status::MemoryLimit, "Python exceeded its memory limit");
            }
            Ok(container) if container["State"]["Status"] == "exited" => {
                if let Some(code) = container["State"]["ExitCode"]
                    .as_i64()
                    .and_then(|n| i32::try_from(n).ok())
                {
                    response.exit_code = Some(code);
                    response.ok = code == 0;
                    response.status = if code == 0 {
                        Status::Success
                    } else {
                        Status::ScriptError
                    };
                    response.message = if code == 0 {
                        None
                    } else {
                        Some("Python exited with an error".into())
                    };
                }
            }
            Ok(_) => response.set_status(
                Status::SandboxError,
                "Docker attach ended before Python exited",
            ),
            Err(error) => response.set_status(Status::SandboxError, error),
        }
    }
    // Killing the Docker CLI alone does not kill Python. execute() always removes
    // the named container and verifies absence, including all descendants.
    let _ = child.kill().await;
    response
}

/// Parent-side coordinator; dropping its liveness pipe triggers cleanup.
pub async fn launch(job: Job, stop: CancellationToken) -> RunPythonResponse {
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => return RunPythonResponse::error(Status::SandboxError, error.to_string()),
    };
    let mut child = match Command::new(executable)
        .arg("--supervise-job")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Do not kill the supervisor on cancellation: it must remove its container.
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return RunPythonResponse::error(Status::SandboxError, error.to_string()),
    };
    let mut liveness = child.stdin.take();
    let stdout = child.stdout.take().expect("piped stdout");
    let mut encoded = serde_json::to_vec(&job).expect("serialize job");
    encoded.push(b'\n');
    if let Err(error) = liveness
        .as_mut()
        .expect("piped stdin")
        .write_all(&encoded)
        .await
    {
        drop(liveness);
        let _ = child.wait().await;
        return RunPythonResponse::error(Status::SandboxError, error.to_string());
    }
    let collect = async move {
        let mut bytes = Vec::new();
        stdout
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut bytes)
            .await?;
        child.wait().await.map(|status| (status, bytes))
    };
    tokio::pin!(collect);
    let collected = tokio::select! {
        result = &mut collect => result,
        _ = stop.cancelled() => {
            drop(liveness.take());
            collect.await
        }
    };
    match collected {
        Ok((status, bytes)) if status.success() && bytes.len() as u64 <= MAX_RESPONSE_BYTES => {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                RunPythonResponse::error(
                    Status::CleanupFailed,
                    "Supervisor returned an invalid result; cleanup is unconfirmed",
                )
            })
        }
        _ => RunPythonResponse::error(
            Status::CleanupFailed,
            "Supervisor failed; cleanup is unconfirmed",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn job_frames_are_bounded_before_deserialization() {
        let mut reader = BufReader::new(&b"1234\n"[..]);
        assert_eq!(read_frame(&mut reader, 5).await.unwrap(), b"1234\n");
        let mut reader = BufReader::new(&b"12345\n"[..]);
        assert!(read_frame(&mut reader, 5).await.is_err());
        let mut reader = BufReader::new(&b"1234"[..]);
        assert!(read_frame(&mut reader, 5).await.is_err());
    }
}
