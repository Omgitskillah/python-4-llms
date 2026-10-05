#![cfg(unix)]
mod common;
use common::FakeDocker;
use python_4_llms::{
    contract::Status,
    docker::{EXPIRY_LABEL, check_container, create_args},
    supervisor::{Job, execute},
};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn success_script_errors_and_oom_are_distinct_and_cleaned_up() {
    for (mode, status, code) in [
        ("success", Status::Success, Some(0)),
        ("error", Status::ScriptError, Some(1)),
        ("oom", Status::MemoryLimit, None),
    ] {
        let fake = FakeDocker::new(mode);
        let response = execute(
            Job {
                config: fake.config.clone(),
                request: fake.request(),
            },
            CancellationToken::new(),
        )
        .await;
        assert_eq!(response.status, status, "{response:?}");
        assert_eq!(response.exit_code, code);
        assert!(!fake.dir.path().join("container.json").exists());
        if mode == "success" {
            assert_eq!(response.stdout, "42\n");
        }
        if mode == "error" {
            assert!(response.stderr.contains("ValueError"));
        }
    }
}

#[tokio::test]
async fn applied_policy_is_verified_before_start_and_every_critical_setting_is_checked() {
    let fake = FakeDocker::new("policyfail");
    let response = execute(
        Job {
            config: fake.config.clone(),
            request: fake.request(),
        },
        CancellationToken::new(),
    )
    .await;
    assert_eq!(response.status, Status::SandboxUnavailable);
    assert!(!fake.events().iter().any(|args| args[0] == "start"));
    assert!(!fake.dir.path().join("container.json").exists());

    let fake = FakeDocker::new("success");
    fake.config
        .control(&create_args(&fake.config, "python-4-llms-policy-test"))
        .await
        .unwrap();
    let complete = fake
        .config
        .inspect("python-4-llms-policy-test")
        .await
        .unwrap();
    assert!(check_container(&complete, &fake.config).is_ok());
    for pointer in [
        "/HostConfig/ReadonlyRootfs",
        "/HostConfig/Privileged",
        "/HostConfig/NetworkMode",
        "/HostConfig/CapDrop",
        "/HostConfig/SecurityOpt",
        "/HostConfig/Memory",
        "/HostConfig/MemorySwap",
        "/HostConfig/NanoCpus",
        "/HostConfig/PidsLimit",
        "/HostConfig/ShmSize",
        "/HostConfig/IpcMode",
        "/HostConfig/Init",
        "/HostConfig/LogConfig/Type",
        "/HostConfig/RestartPolicy/Name",
        "/HostConfig/Tmpfs",
        "/Config/User",
        "/Config/Image",
        "/Config/Entrypoint",
        "/Config/Cmd",
        "/Config/WorkingDir",
        "/Mounts",
    ] {
        let mut changed = complete.clone();
        *changed.pointer_mut(pointer).unwrap() = serde_json::json!("unsafe");
        assert!(
            check_container(&changed, &fake.config).is_err(),
            "{pointer}"
        );
    }
    for field in [
        "Binds",
        "Mounts",
        "Devices",
        "DeviceRequests",
        "PidMode",
        "UTSMode",
        "CapAdd",
    ] {
        let mut changed = complete.clone();
        changed["HostConfig"][field] = serde_json::json!(["unsafe"]);
        assert!(check_container(&changed, &fake.config).is_err(), "{field}");
    }
}

#[tokio::test]
async fn deadlines_output_limits_and_unconfirmed_creation_cleanup_are_enforced() {
    for (mode, status) in [
        ("hang", Status::Timeout),
        ("output", Status::OutputLimit),
        ("cleanupfail", Status::CleanupFailed),
        ("createfail", Status::CleanupFailed),
    ] {
        let fake = FakeDocker::new(mode);
        let mut request = fake.request();
        if mode == "hang" {
            request.timeout_ms = 300;
        }
        let response = execute(
            Job {
                config: fake.config.clone(),
                request,
            },
            CancellationToken::new(),
        )
        .await;
        assert_eq!(response.status, status, "{response:?}");
        if mode != "cleanupfail" {
            assert!(!fake.dir.path().join("container.json").exists());
        }
        if mode == "output" {
            assert!(response.output_truncated);
            assert_eq!(response.stdout.len(), 65536);
        }
        if mode == "createfail" {
            assert!(!fake.events().iter().any(|args| args[0] == "start"));
        }
    }
}

#[tokio::test]
async fn cancellation_before_creation_starts_nothing() {
    let fake = FakeDocker::new("success");
    let stop = CancellationToken::new();
    stop.cancel();
    let response = execute(
        Job {
            config: fake.config.clone(),
            request: fake.request(),
        },
        stop,
    )
    .await;
    assert_eq!(response.status, Status::Cancelled);
    assert!(!fake.events().iter().any(|args| args[0] == "create"));
}

#[tokio::test]
async fn recovery_preserves_active_jobs_and_removes_only_expired_owned_jobs() {
    let fake = FakeDocker::new("success");
    let name = "python-4-llms-recovery-test";
    fake.config
        .control(&create_args(&fake.config, name))
        .await
        .unwrap();
    assert_eq!(fake.config.recover().await.unwrap(), 0);
    let path = fake.dir.path().join("container.json");
    let mut container: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    container["Config"]["Labels"][EXPIRY_LABEL] = serde_json::json!("1");
    std::fs::write(&path, serde_json::to_vec(&container).unwrap()).unwrap();
    assert_eq!(fake.config.recover().await.unwrap(), 1);
    assert!(!path.exists());
}
