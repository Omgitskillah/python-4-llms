#![cfg(unix)]
mod common;
use common::{FakeDocker, McpProcess, wait_for_path};
use serde_json::json;

#[tokio::test]
async fn stdio_discovery_structured_results_and_validation() {
    let fake = FakeDocker::new("success");
    let mut process = McpProcess::start(&fake.config).await;
    let listing = process
        .request(json!({"jsonrpc":"2.0", "id":2, "method":"tools/list", "params":{}}))
        .await;
    let tools = listing["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "run_python");
    assert_eq!(tools[0]["inputSchema"]["required"], json!(["code"]));
    assert_eq!(tools[0]["inputSchema"]["additionalProperties"], false);
    assert_eq!(tools[0]["outputSchema"]["type"], "object");

    let success = process.call(3, json!({"code":"print(42)"})).await;
    assert_ne!(success["result"]["isError"], true, "{success}");
    assert_eq!(success["result"]["structuredContent"]["status"], "SUCCESS");
    assert_eq!(success["result"]["structuredContent"]["stdout"], "42\n");
    let fallback: serde_json::Value =
        serde_json::from_str(success["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(fallback, success["result"]["structuredContent"]);
    let before = fake.events().len();
    for input in [
        json!({}),
        json!({"code":"pass", "network":true}),
        json!({"code":"pass", "timeout_ms":0}),
        json!({"code":"x".repeat(65537)}),
        json!({"code":"pass", "stdin":"x".repeat(262145)}),
    ] {
        let invalid = process.call(4, input).await;
        assert_eq!(invalid["result"]["isError"], true, "{invalid}");
    }
    assert_eq!(
        fake.events().len(),
        before,
        "invalid requests never invoke Docker"
    );
    process.close().await;
}

#[tokio::test]
async fn tool_errors_are_structured_and_unconfirmed_cleanup_latches_closed() {
    let fake = FakeDocker::new("error");
    let mut process = McpProcess::start(&fake.config).await;
    let error = process.call(2, json!({"code":"raise ValueError()"})).await;
    assert_eq!(error["result"]["isError"], true);
    assert_eq!(
        error["result"]["structuredContent"]["status"],
        "SCRIPT_ERROR"
    );
    process.close().await;

    let fake = FakeDocker::new("cleanupfail");
    let mut process = McpProcess::start(&fake.config).await;
    let error = process.call(2, json!({"code":"pass"})).await;
    assert_eq!(
        error["result"]["structuredContent"]["status"],
        "CLEANUP_FAILED"
    );
    let before = fake.events().len();
    std::fs::write(fake.dir.path().join("mode"), "success").unwrap();
    let retry = process.call(3, json!({"code":"pass"})).await;
    assert_eq!(
        retry["result"]["structuredContent"]["status"],
        "CLEANUP_FAILED"
    );
    assert_eq!(
        fake.events().len(),
        before,
        "unsafe retry must not invoke Docker"
    );
    process.close().await;
}

#[tokio::test]
async fn busy_and_cancellation_hold_the_permit_through_cleanup() {
    let fake = FakeDocker::new("hang");
    let mut process = McpProcess::start(&fake.config).await;
    process.send(json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":"run_python", "arguments":{"code":"pass", "timeout_ms":30000}}})).await;
    wait_for_path(&fake.dir.path().join("ready"), true).await;
    let busy = process.call(3, json!({"code":"pass"})).await;
    assert_eq!(busy["id"], 3);
    assert_eq!(busy["result"]["structuredContent"]["status"], "BUSY");
    process.send(json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":2,"reason":"test"}})).await;
    wait_for_path(&fake.dir.path().join("container.json"), false).await;
    // Wait for the coordinator to finish verification, rather than guessing when
    // rm's filesystem effect has become a completed cleanup response.
    let next = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            std::fs::write(fake.dir.path().join("mode"), "success").unwrap();
            let reply = process.call(4, json!({"code":"print(42)"})).await;
            if reply["result"]["structuredContent"]["status"] != "BUSY" {
                break reply;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        next["result"]["structuredContent"]["status"], "SUCCESS",
        "{next}"
    );
    process.close().await;
}

#[tokio::test]
async fn parent_sigkill_leaves_supervisor_alive_to_remove_the_container() {
    let fake = FakeDocker::new("hang");
    let mut process = McpProcess::start(&fake.config).await;
    process.send(json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":"run_python", "arguments":{"code":"pass", "timeout_ms":30000}}})).await;
    wait_for_path(&fake.dir.path().join("ready"), true).await;
    process.child.kill().await.unwrap();
    drop(process.stdin.take());
    wait_for_path(&fake.dir.path().join("container.json"), false).await;
    assert!(fake.events().iter().any(|args| args[0] == "rm"));
}

#[tokio::test]
async fn client_eof_shuts_down_an_active_job() {
    let fake = FakeDocker::new("hang");
    let mut process = McpProcess::start(&fake.config).await;
    process.send(json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":"run_python", "arguments":{"code":"pass", "timeout_ms":30000}}})).await;
    wait_for_path(&fake.dir.path().join("ready"), true).await;
    process.close().await;
    assert!(!fake.dir.path().join("container.json").exists());
}

#[tokio::test]
async fn supervisor_flushes_full_sized_results_before_process_exit() {
    let fake = FakeDocker::new("output");
    let mut process = McpProcess::start(&fake.config).await;
    let result = process
        .call(2, json!({"code":"print('large output')"}))
        .await;
    assert_eq!(
        result["result"]["structuredContent"]["status"], "OUTPUT_LIMIT",
        "{result}"
    );
    assert_eq!(
        result["result"]["structuredContent"]["stdout"]
            .as_str()
            .unwrap()
            .len(),
        65536
    );
    assert_eq!(
        result["result"]["structuredContent"]["output_truncated"],
        true
    );
    process.close().await;
}
