#![allow(dead_code)]
use python_4_llms::{contract::RunPythonRequest, docker::Config};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};
use tempfile::TempDir;
use tokio::{
    io::{AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};

pub struct McpProcess {
    pub child: Child,
    pub stdin: Option<ChildStdin>,
    pub stdout: BufReader<ChildStdout>,
}

impl McpProcess {
    pub async fn start(config: &Config) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_python-mcp"))
            .env("PYTHON_MCP_DOCKER", &config.docker)
            .env("PYTHON_MCP_DOCKER_SOCKET", &config.socket)
            .env("PYTHON_MCP_IMAGE", &config.image)
            .env("HOST_SECRET_FOR_TEST", "must-not-reach-python")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut process = Self {
            child,
            stdin,
            stdout,
        };
        let initialized = process.request(json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
            "protocolVersion":"2025-11-25", "capabilities":{}, "clientInfo":{"name":"test","version":"1"}}})).await;
        assert_eq!(initialized["result"]["serverInfo"]["name"], "python-4-llms");
        process
            .send(json!({"jsonrpc":"2.0", "method":"notifications/initialized"}))
            .await;
        process
    }

    pub async fn send(&mut self, value: Value) {
        let mut encoded = serde_json::to_vec(&value).unwrap();
        encoded.push(b'\n');
        self.stdin
            .as_mut()
            .unwrap()
            .write_all(&encoded)
            .await
            .unwrap();
    }

    pub async fn read(&mut self) -> Value {
        let line = tokio::time::timeout(
            std::time::Duration::from_secs(90),
            python_4_llms::supervisor::read_frame(
                &mut self.stdout,
                python_4_llms::contract::MAX_RESPONSE_BYTES + 4096,
            ),
        )
        .await
        .expect("MCP response deadline")
        .expect("MCP response frame");
        serde_json::from_slice(&line).unwrap()
    }

    pub async fn request(&mut self, value: Value) -> Value {
        self.send(value).await;
        self.read().await
    }

    pub async fn call(&mut self, id: u64, arguments: Value) -> Value {
        self.request(json!({"jsonrpc":"2.0", "id":id, "method":"tools/call", "params":{"name":"run_python", "arguments":arguments}})).await
    }

    pub async fn close(mut self) {
        drop(self.stdin.take());
        let status = tokio::time::timeout(std::time::Duration::from_secs(30), self.child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
    }
}

pub async fn wait_for_path(path: &std::path::Path, exists: bool) {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while path.exists() != exists {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("lifecycle event deadline");
}

pub struct FakeDocker {
    pub dir: TempDir,
    pub config: Config,
}

impl FakeDocker {
    pub fn new(mode: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let python = Command::new("python3")
            .args(["-c", "import sys; print(sys.executable)"])
            .output()
            .unwrap();
        assert!(python.status.success());
        let python = String::from_utf8(python.stdout).unwrap();
        let path = dir.path().join("docker");
        fs::write(
            &path,
            format!(
                "#!{}\n{}",
                python.trim(),
                include_str!("../fixtures/docker.py")
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.path().join("mode"), mode).unwrap();
        let config = Config {
            docker: path,
            socket: dir.path().join("docker.sock"),
            image: format!("sha256:{}", "a".repeat(64)),
        };
        Self { dir, config }
    }

    pub fn events(&self) -> Vec<Vec<String>> {
        fs::read_to_string(self.dir.path().join("events"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    pub fn request(&self) -> RunPythonRequest {
        RunPythonRequest {
            code: "print(42)".into(),
            stdin: String::new(),
            timeout_ms: 3000,
        }
    }
}
