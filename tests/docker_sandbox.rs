#![cfg(unix)]
mod common;
use common::McpProcess;
use python_4_llms::docker::{Config, LABEL, strings};
use serde_json::json;

async fn run(
    process: &mut McpProcess,
    source: &str,
    stdin: &str,
    timeout: u64,
) -> serde_json::Value {
    let response = process
        .call(2, json!({"code":source,"stdin":stdin,"timeout_ms":timeout}))
        .await;
    response["result"]["structuredContent"].clone()
}

#[tokio::test]
#[ignore = "requires a real local Docker engine and PYTHON_MCP_IMAGE; run explicitly or in CI"]
async fn real_containers_enforce_isolation_limits_and_fresh_state() {
    let config = Config::from_env().expect("Docker test configuration");
    config
        .preflight()
        .await
        .expect("required runtime protections");
    let mut process = McpProcess::start(&config).await;
    let result = run(
        &mut process,
        "import json, sys; print(json.dumps({'input': sys.stdin.read(), 'answer': 6 * 7}))",
        "hello\n",
        10000,
    )
    .await;
    assert_eq!(result["status"], "SUCCESS", "{result}");
    let payload: serde_json::Value =
        serde_json::from_str(result["stdout"].as_str().unwrap()).unwrap();
    assert_eq!(payload, json!({"input":"hello\n", "answer":42}));

    let sentinel = tempfile::tempdir().unwrap();
    let path = sentinel.path().join("host-only-secret.txt");
    std::fs::write(&path, "preserve me").unwrap();
    let source = format!(
        r#"
import errno, os, socket
assert os.getuid() == 1000
assert 'HOST_SECRET_FOR_TEST' not in os.environ
assert not os.path.exists({path:?})
os.symlink({path:?}, '/tmp/host-link')
assert not os.path.exists('/tmp/host-link')
assert not os.path.exists('/var/run/docker.sock')
status = open('/proc/self/status').read()
assert 'NoNewPrivs:\t1' in status
assert 'Seccomp:\t2' in status
assert 'CapEff:\t0000000000000000' in status
for name in ['/opt/runner.py', '/root/escape', '/etc/escape']:
    try:
        open(name, 'w').write('escape')
        raise AssertionError('root filesystem write succeeded')
    except OSError as error:
        assert error.errno in [errno.EROFS, errno.EACCES]
sock = socket.socket()
sock.settimeout(2)
try:
    sock.connect(('1.1.1.1', 443))
    raise AssertionError('external networking succeeded')
except OSError:
    pass
open('/tmp/state', 'w').write('temporary')
print('isolated')
"#,
        path = path.to_str().unwrap()
    );
    let isolated = run(&mut process, &source, "", 10000).await;
    assert_eq!(isolated["status"], "SUCCESS", "{isolated}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "preserve me");
    let fresh = run(
        &mut process,
        "import os; assert not os.path.exists('/tmp/state'); print('fresh')",
        "",
        10000,
    )
    .await;
    assert_eq!(fresh["status"], "SUCCESS", "{fresh}");

    let error = run(&mut process, "raise ValueError('expected')", "", 10000).await;
    assert_eq!(error["status"], "SCRIPT_ERROR", "{error}");
    assert!(
        error["stderr"]
            .as_str()
            .unwrap()
            .contains("ValueError: expected")
    );
    let timeout = run(&mut process, "while True: pass", "", 500).await;
    assert_eq!(timeout["status"], "TIMEOUT", "{timeout}");
    let containers = || {
        strings(&[
            "ps",
            "--all",
            "--filter",
            &format!("label={LABEL}"),
            "--format",
            "{{.Names}}",
        ])
    };
    let before = config.control(&containers()).await.unwrap();
    let child = run(&mut process, "import subprocess\nsubprocess.Popen(['/usr/local/bin/python', '-c', 'import time; time.sleep(60)'])\nwhile True: pass", "", 1000).await;
    assert_eq!(child["status"], "TIMEOUT", "{child}");
    assert_eq!(
        config.control(&containers()).await.unwrap(),
        before,
        "timeout must remove the entire container and its child workload"
    );
    let overflow = run(
        &mut process,
        "import sys; sys.stdout.write('x' * 1000000)",
        "",
        10000,
    )
    .await;
    assert_eq!(overflow["status"], "OUTPUT_LIMIT", "{overflow}");
    assert_eq!(overflow["output_truncated"], true);
    let memory = run(&mut process, "x = bytearray(512 * 1024 * 1024)", "", 10000).await;
    assert_eq!(memory["status"], "MEMORY_LIMIT", "{memory}");
    let disk = run(&mut process, "import errno\ntry:\n f = open('/tmp/large', 'wb')\n for _ in range(128): f.write(b'x' * 1024 * 1024)\n f.flush()\n raise AssertionError('scratch quota missing')\nexcept OSError as e:\n assert e.errno == errno.ENOSPC\n print('bounded')", "", 10000).await;
    assert_eq!(disk["status"], "SUCCESS", "{disk}");
    let pids = run(&mut process, "import errno, subprocess\nchildren=[]\ntry:\n for _ in range(100): children.append(subprocess.Popen(['/usr/local/bin/python', '-c', 'import time; time.sleep(60)']))\n raise AssertionError('PID quota missing')\nexcept OSError as e:\n assert e.errno == errno.EAGAIN\n print('bounded')\nfinally:\n for child in children: child.kill(); child.wait()", "", 10000).await;
    assert_eq!(pids["status"], "SUCCESS", "{pids}");
    process.close().await;
}
