use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_CODE_BYTES: usize = 64 * 1024;
pub const MAX_STDIN_BYTES: usize = 256 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
pub const MAX_TIMEOUT_MS: u64 = 30_000;
// JSON escaping can expand each input byte to six bytes.
pub const MAX_JOB_MESSAGE_BYTES: u64 = 6 * (MAX_CODE_BYTES + MAX_STDIN_BYTES) as u64 + 4096;
pub const MAX_RESPONSE_BYTES: u64 = 6 * MAX_OUTPUT_BYTES as u64 + 4096;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunPythonRequest {
    /// Python source text, at most 64 KiB in UTF-8. Each call starts fresh.
    pub code: String,
    /// Text supplied through Python's sys.stdin, at most 256 KiB in UTF-8.
    #[serde(default)]
    pub stdin: String,
    /// Wall-clock execution limit in milliseconds (1..=30000); defaults to 10000.
    #[serde(default = "default_timeout")]
    #[schemars(range(min = 1, max = 30000))]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

impl RunPythonRequest {
    pub fn validate(&self) -> Result<(), RunPythonResponse> {
        if self.code.is_empty() || self.code.len() > MAX_CODE_BYTES {
            return Err(RunPythonResponse::error(
                Status::InvalidInput,
                "code must contain 1..=65536 UTF-8 bytes",
            ));
        }
        if self.stdin.len() > MAX_STDIN_BYTES {
            return Err(RunPythonResponse::error(
                Status::InvalidInput,
                "stdin must contain at most 262144 UTF-8 bytes",
            ));
        }
        if !(1..=MAX_TIMEOUT_MS).contains(&self.timeout_ms) {
            return Err(RunPythonResponse::error(
                Status::InvalidInput,
                "timeout_ms must be between 1 and 30000",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Success,
    ScriptError,
    Timeout,
    OutputLimit,
    MemoryLimit,
    Cancelled,
    Busy,
    InvalidInput,
    SandboxUnavailable,
    SandboxError,
    CleanupFailed,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct RunPythonResponse {
    pub schema_version: u8,
    pub ok: bool,
    pub status: Status,
    pub stdout: String,
    pub stderr: String,
    /// Null when Python did not finish normally or a limit interrupted execution.
    pub exit_code: Option<i32>,
    /// Execution duration, excluding sandbox preparation and cleanup.
    pub duration_ms: u64,
    /// True when the combined output limit discarded bytes.
    pub output_truncated: bool,
    pub message: Option<String>,
}

impl RunPythonResponse {
    pub fn error(status: Status, message: impl Into<String>) -> Self {
        Self {
            schema_version: 1,
            ok: false,
            status,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            duration_ms: 0,
            output_truncated: false,
            message: Some(message.into()),
        }
    }

    pub fn set_status(&mut self, status: Status, message: impl Into<String>) {
        self.ok = status == Status::Success;
        self.status = status;
        self.message = Some(message.into());
    }
}

/// Shared byte budget: neither stream can cause unbounded host buffering.
#[derive(Default)]
pub struct Output {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    pub truncated: bool,
}

impl Output {
    pub fn push(&mut self, stderr: bool, bytes: &[u8]) -> bool {
        let available = MAX_OUTPUT_BYTES - self.stdout.len() - self.stderr.len();
        let count = available.min(bytes.len());
        if stderr {
            &mut self.stderr
        } else {
            &mut self.stdout
        }
        .extend_from_slice(&bytes[..count]);
        self.truncated |= count < bytes.len();
        self.truncated
    }

    pub fn apply(self, response: &mut RunPythonResponse) {
        response.stdout = String::from_utf8_lossy(&self.stdout).into_owned();
        response.stderr = String::from_utf8_lossy(&self.stderr).into_owned();
        response.output_truncated = self.truncated;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_boundaries_are_enforced_in_bytes() {
        let mut request: RunPythonRequest = serde_json::from_str(r#"{"code":"pass"}"#).unwrap();
        assert_eq!(request.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(request.stdin, "");
        request.code = "é".repeat(MAX_CODE_BYTES / 2);
        request.stdin = "x".repeat(MAX_STDIN_BYTES);
        request.timeout_ms = MAX_TIMEOUT_MS;
        assert!(request.validate().is_ok());
        request.code.push('x');
        assert_eq!(request.validate().unwrap_err().status, Status::InvalidInput);
        request.code = "pass".into();
        request.stdin.push('x');
        assert!(request.validate().is_err());
        request.stdin.clear();
        for timeout in [0, MAX_TIMEOUT_MS + 1] {
            request.timeout_ms = timeout;
            assert!(request.validate().is_err());
        }
        request.code.clear();
        assert!(request.validate().is_err());
    }

    #[test]
    fn caller_cannot_override_container_policy() {
        for field in ["image", "network", "mounts", "env", "command", "memory"] {
            let input = serde_json::json!({"code":"pass", field:"override"});
            assert!(serde_json::from_value::<RunPythonRequest>(input).is_err());
        }
    }

    #[test]
    fn streams_share_a_hard_output_budget_and_decode_binary_output() {
        let mut output = Output::default();
        assert!(!output.push(false, &[0xff]));
        assert!(!output.push(true, &vec![b'x'; MAX_OUTPUT_BYTES - 1]));
        assert!(output.push(false, b"overflow"));
        let mut response = RunPythonResponse::error(Status::OutputLimit, "limit");
        output.apply(&mut response);
        assert_eq!(response.stdout, "\u{fffd}");
        assert_eq!(response.stderr.len(), MAX_OUTPUT_BYTES - 1);
        assert!(response.output_truncated);
    }
}
