use crate::contract::{RunPythonResponse, Status};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::PathBuf,
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncReadExt, process::Command};

pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
pub const MEMORY_BYTES: i64 = 256 * 1024 * 1024;
pub const TMPFS_OPTIONS: &str = "rw,noexec,nosuid,nodev,size=67108864,mode=1777";
pub const LABEL: &str = "io.python-4-llms.sandbox=1";
pub const EXPIRY_LABEL: &str = "io.python-4-llms.expires";
const MAX_CONTROL_BYTES: u64 = 128 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub docker: PathBuf,
    pub socket: PathBuf,
    pub image: String,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let docker = match std::env::var_os("PYTHON_MCP_DOCKER") {
            Some(path) => PathBuf::from(path),
            None => std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|dir| dir.join("docker"))
                .find(|path| path.is_file())
                .ok_or("Docker CLI not found; install Docker and add it to PATH")?,
        };
        let socket = match std::env::var_os("PYTHON_MCP_DOCKER_SOCKET") {
            Some(path) => PathBuf::from(path),
            None if cfg!(target_os = "macos") => PathBuf::from(
                std::env::var_os("HOME").ok_or("HOME is required to locate Docker Desktop")?,
            )
            .join(".docker/run/docker.sock"),
            None => PathBuf::from("/var/run/docker.sock"),
        };
        let config = Self {
            docker,
            socket,
            image: std::env::var("PYTHON_MCP_IMAGE")
                .map_err(|_| "PYTHON_MCP_IMAGE must be the built sandbox's sha256 image ID")?,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.docker.is_absolute() || !self.socket.is_absolute() {
            return Err("Docker executable and local Unix socket must be absolute paths".into());
        }
        let digest = self.image.strip_prefix("sha256:").unwrap_or_default();
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("PYTHON_MCP_IMAGE must be a full sha256: image ID".into());
        }
        Ok(())
    }

    pub fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(&self.docker);
        command
            .arg("--host")
            .arg(format!("unix://{}", self.socket.display()))
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/local/bin")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    pub async fn control(&self, args: &[String]) -> Result<Vec<u8>, String> {
        let mut child = self.command(args).spawn().map_err(|e| e.to_string())?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let operation = async {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let mut stdout = stdout.take(MAX_CONTROL_BYTES + 1);
            let mut stderr = stderr.take(MAX_CONTROL_BYTES + 1);
            let (out_read, err_read, status) = tokio::join!(
                stdout.read_to_end(&mut out),
                stderr.read_to_end(&mut err),
                child.wait()
            );
            out_read.map_err(|e| e.to_string())?;
            err_read.map_err(|e| e.to_string())?;
            if out.len() as u64 > MAX_CONTROL_BYTES || err.len() as u64 > MAX_CONTROL_BYTES {
                return Err("Docker control response exceeded its byte limit".into());
            }
            if !status.map_err(|e| e.to_string())?.success() {
                return Err(format!(
                    "Docker command failed: {}",
                    String::from_utf8_lossy(&err[..err.len().min(1024)])
                ));
            }
            Ok(out)
        };
        tokio::time::timeout(CONTROL_TIMEOUT, operation)
            .await
            .map_err(|_| "Docker control command timed out".to_string())?
    }

    pub async fn preflight(&self) -> Result<(), String> {
        self.validate()?;
        let info = self
            .control(&strings(&["info", "--format", "{{json .}}"]))
            .await?;
        check_info(&serde_json::from_slice(&info).map_err(|e| e.to_string())?)?;
        let data = self
            .control(&strings(&["image", "inspect", &self.image]))
            .await?;
        let images: Vec<Value> = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
        let image = images.first().ok_or("Sandbox image not found")?;
        if image["Id"] != self.image
            || image["Os"] != "linux"
            || image["Config"]["User"] != "1000:1000"
            || image["Config"]["Entrypoint"]
                != serde_json::json!(["/usr/local/bin/python", "-I", "-B", "-u", "/opt/runner.py"])
            || !empty(&image["Config"]["Volumes"])
        {
            return Err("Sandbox image configuration does not match the required runner".into());
        }
        Ok(())
    }

    pub async fn inspect(&self, name: &str) -> Result<Value, String> {
        let data = self
            .control(&strings(&["container", "inspect", name]))
            .await?;
        let containers: Vec<Value> = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
        containers
            .into_iter()
            .next()
            .ok_or("Container not found".into())
    }

    pub async fn remove(&self, name: &str) -> Result<(), String> {
        // Check absence even if rm reports an error (e.g. create never succeeded).
        let removed = self.control(&strings(&["rm", "--force", name])).await;
        let remaining = self
            .control(&strings(&[
                "ps",
                "--all",
                "--quiet",
                "--filter",
                &format!("name=^/{name}$"),
            ]))
            .await?;
        if !remaining.iter().all(u8::is_ascii_whitespace) {
            return Err(removed
                .err()
                .unwrap_or("Container still exists after removal".into()));
        }
        Ok(())
    }

    /// Only expired jobs created by this tool are eligible; active jobs from
    /// another MCP server instance are preserved.
    pub async fn recover(&self) -> Result<usize, String> {
        let names = self
            .control(&strings(&[
                "ps",
                "--all",
                "--filter",
                &format!("label={LABEL}"),
                "--format",
                "{{.Names}}",
            ]))
            .await?;
        let names = String::from_utf8(names).map_err(|e| e.to_string())?;
        if names.lines().count() > 100 {
            return Err("More than 100 sandbox containers require manual inspection".into());
        }
        let now = epoch_seconds();
        let mut removed = 0;
        for name in names.lines() {
            if !name.starts_with("python-4-llms-")
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err("Unexpected container name in sandbox recovery".into());
            }
            let container = self.inspect(name).await?;
            let labels = &container["Config"]["Labels"];
            let expires = labels[EXPIRY_LABEL]
                .as_str()
                .and_then(|value| value.parse::<u64>().ok());
            if labels["io.python-4-llms.sandbox"] == "1"
                && expires.is_some_and(|expires| expires <= now)
            {
                self.remove(name).await?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after Unix epoch")
        .as_secs()
}

fn empty(value: &Value) -> bool {
    value.is_null()
        || value.as_array().is_some_and(Vec::is_empty)
        || value.as_object().is_some_and(serde_json::Map::is_empty)
        || value.as_str() == Some("")
}

pub fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_owned()).collect()
}

pub fn check_info(info: &Value) -> Result<(), String> {
    let seccomp = info["SecurityOptions"].as_array().is_some_and(|options| {
        options.iter().any(|value| {
            value
                .as_str()
                .is_some_and(|s| s.split(',').any(|part| part == "name=seccomp"))
        })
    });
    if info["OSType"] != "linux"
        || info["MemoryLimit"] != true
        || info["SwapLimit"] != true
        || info["CpuCfsQuota"] != true
        || info["PidsLimit"] != true
        || !seccomp
    {
        return Err("Docker must support Linux, seccomp, memory/swap, CPU, and PID limits".into());
    }
    Ok(())
}

pub fn create_args(config: &Config, name: &str) -> Vec<String> {
    strings(&[
        "create",
        "--pull",
        "never",
        "--name",
        name,
        "--label",
        LABEL,
        "--label",
        &format!("{EXPIRY_LABEL}={}", epoch_seconds() + 120),
        "--network",
        "none",
        "--user",
        "1000:1000",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--security-opt",
        "seccomp=builtin",
        "--read-only",
        "--tmpfs",
        &format!("/tmp:{TMPFS_OPTIONS}"),
        "--memory",
        "268435456",
        "--memory-swap",
        "268435456",
        "--cpus",
        "1",
        "--pids-limit",
        "16",
        "--shm-size",
        "65536",
        "--ipc",
        "private",
        "--ulimit",
        "nofile=64:64",
        "--ulimit",
        "core=0:0",
        "--log-driver",
        "none",
        "--restart",
        "no",
        "--init",
        "--workdir",
        "/tmp",
        "--entrypoint",
        "/usr/local/bin/python",
        "--interactive",
        &config.image,
        "-I",
        "-B",
        "-u",
        "/opt/runner.py",
    ])
}

/// Verify the actual daemon configuration before letting Python start.
pub fn check_container(container: &Value, config: &Config) -> Result<(), String> {
    let host = &container["HostConfig"];
    let runner = &container["Config"];
    let security = &host["SecurityOpt"];
    let safe = runner["Image"] == config.image
        && runner["User"] == "1000:1000"
        && runner["WorkingDir"] == "/tmp"
        && runner["Entrypoint"] == serde_json::json!(["/usr/local/bin/python"])
        && runner["Cmd"] == serde_json::json!(["-I", "-B", "-u", "/opt/runner.py"])
        && host["ReadonlyRootfs"] == true
        && host["Privileged"] == false
        && host["NetworkMode"] == "none"
        && host["CapDrop"] == serde_json::json!(["ALL"])
        && empty(&host["CapAdd"])
        && security.as_array().is_some_and(|s| {
            s.contains(&serde_json::json!("no-new-privileges"))
                && s.contains(&serde_json::json!("seccomp=builtin"))
                && s.len() == 2
        })
        && host["Memory"] == MEMORY_BYTES
        && host["MemorySwap"] == MEMORY_BYTES
        && host["NanoCpus"] == 1_000_000_000_i64
        && host["PidsLimit"] == 16
        && host["ShmSize"] == 65536
        && host["IpcMode"] == "private"
        && host["Init"] == true
        && host["LogConfig"]["Type"] == "none"
        && host["RestartPolicy"]["Name"] == "no"
        && host["Tmpfs"] == serde_json::json!({"/tmp":TMPFS_OPTIONS})
        && empty(&host["Binds"])
        && empty(&host["Mounts"])
        && empty(&host["Devices"])
        && empty(&host["DeviceRequests"])
        && empty(&host["PidMode"])
        && empty(&host["UTSMode"])
        && empty(&runner["Volumes"])
        && container["Mounts"].as_array().is_some_and(|mounts| {
            mounts
                .iter()
                .all(|m| m["Type"] == "tmpfs" && m["Destination"] == "/tmp")
        });
    if !safe {
        return Err("Docker did not apply the complete required sandbox policy".into());
    }
    Ok(())
}

pub fn unavailable(message: String) -> RunPythonResponse {
    RunPythonResponse::error(Status::SandboxUnavailable, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_runtime_protections_fail_closed() {
        let complete = serde_json::json!({"OSType":"linux", "MemoryLimit":true, "SwapLimit":true,
            "CpuCfsQuota":true, "PidsLimit":true, "SecurityOptions":["name=seccomp,profile=builtin"]});
        assert!(check_info(&complete).is_ok());
        for field in [
            "MemoryLimit",
            "SwapLimit",
            "CpuCfsQuota",
            "PidsLimit",
            "SecurityOptions",
            "OSType",
        ] {
            let mut incomplete = complete.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            assert!(check_info(&incomplete).is_err(), "{field}");
        }
    }

    #[test]
    fn only_local_socket_and_immutable_image_ids_are_accepted() {
        let mut config = Config {
            docker: "/bin/docker".into(),
            socket: "/tmp/docker.sock".into(),
            image: format!("sha256:{}", "a".repeat(64)),
        };
        assert!(config.validate().is_ok());
        for image in ["python:latest", "--privileged", "sha256:abc"] {
            config.image = image.into();
            assert!(config.validate().is_err());
        }
        config.image = format!("sha256:{}", "a".repeat(64));
        config.socket = "tcp://localhost:2375".into();
        assert!(config.validate().is_err());
    }
}
