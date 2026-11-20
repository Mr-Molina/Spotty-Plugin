//! End-to-end loopback JSON-RPC daemon and `lms-spotty` CLI binary tests.

use std::process::Command;
use std::time::Duration;

use lms_spotty::{
    read_endpoint_file, DaemonConfig, JsonRpcRequest, JsonRpcResponse, SpottyDaemon,
    ERR_METHOD_NOT_FOUND, ERR_UNAUTHORIZED, SPOTTY_VERSION,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;

async fn rpc_call(addr: &str, req: &JsonRpcRequest) -> JsonRpcResponse {
    let stream = TcpStream::connect(addr)
        .await
        .expect("connect to daemon loopback listener");
    let (read_half, mut write_half) = stream.into_split();
    let mut encoded = serde_json::to_vec(req).expect("serialize JSON-RPC request");
    encoded.push(b'\n');
    write_half
        .write_all(&encoded)
        .await
        .expect("write JSON-RPC request");
    write_half.flush().await.expect("flush JSON-RPC request");

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .await
        .expect("read JSON-RPC response");
    assert!(bytes > 0, "expected non-empty JSON-RPC response line");
    serde_json::from_str(line.trim()).expect("parse JSON-RPC response")
}

#[tokio::test]
async fn daemon_loopback_rpc_roundtrip_and_shutdown() {
    let temp = TempDir::new().expect("create tempdir");
    let config = DaemonConfig::new(temp.path()).with_sweep_interval(Duration::from_secs(60));
    let endpoint_path = config.endpoint_file.clone();

    let daemon = SpottyDaemon::bind(config)
        .await
        .expect("bind SpottyDaemon on loopback");
    let endpoint = read_endpoint_file(&endpoint_path).expect("read written daemon.endpoint");
    assert_eq!(endpoint.version, SPOTTY_VERSION);
    assert!(!endpoint.token.is_empty());

    let addr = endpoint.addr.clone();
    let token = endpoint.token.clone();
    let server_handle = tokio::spawn(async move { daemon.serve().await });

    // 1. system.ping with valid token
    let ping_resp = rpc_call(
        &addr,
        &JsonRpcRequest::new(json!(1), &token, "system.ping", Value::Null),
    )
    .await;
    assert_eq!(ping_resp.id, json!(1));
    assert!(
        ping_resp.error.is_none(),
        "unexpected ping error: {ping_resp:?}"
    );
    let ping_res = ping_resp.result.expect("ping result present");
    assert_eq!(ping_res["status"], json!("ok"));
    assert_eq!(ping_res["version"], json!(SPOTTY_VERSION));

    // 2. Unauthorized bearer token rejection
    let bad_resp = rpc_call(
        &addr,
        &JsonRpcRequest::new(json!(2), "wrong-token", "system.ping", Value::Null),
    )
    .await;
    assert!(bad_resp.result.is_none());
    let bad_err = bad_resp.error.expect("expected unauthorized error");
    assert_eq!(bad_err.code, ERR_UNAUTHORIZED);

    // 3. plugin.init runs migrations and builds transcode table
    let init_resp = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(3),
            &token,
            "plugin.init",
            json!({
                "disabled_formats": ["aac", "wma"],
                "arch": "x86_64-linux",
                "os": "linux",
                "helper_bin": "lms-spotty"
            }),
        ),
    )
    .await;
    assert!(
        init_resp.error.is_none(),
        "plugin.init error: {init_resp:?}"
    );
    let init_res = init_resp.result.expect("plugin.init result");
    assert_eq!(init_res["version"], json!(SPOTTY_VERSION));
    assert!(init_res["custom_convert_conf"]
        .as_str()
        .expect("custom_convert_conf string")
        .contains("[lms-spotty]"));

    // 4. integrations.home_extras returns registered MaterialSkin descriptors
    let extras_resp = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(4),
            &token,
            "integrations.home_extras",
            json!({ "tag": "home" }),
        ),
    )
    .await;
    assert!(
        extras_resp.error.is_none(),
        "integrations.home_extras error: {extras_resp:?}"
    );
    let extras_res = extras_resp.result.expect("home_extras result");
    assert_eq!(extras_res["resolved_menu"], json!("home_heroes_home"));
    assert_eq!(
        extras_res["extras"].as_array().expect("extras array").len(),
        5
    );

    // 5. Unknown method returns -32601
    let unknown_resp = rpc_call(
        &addr,
        &JsonRpcRequest::new(json!(5), &token, "nonexistent.method", Value::Null),
    )
    .await;
    let unknown_err = unknown_resp.error.expect("expected method not found");
    assert_eq!(unknown_err.code, ERR_METHOD_NOT_FOUND);

    // 6. system.shutdown cleanly stops the daemon and removes endpoint file
    let shutdown_resp = rpc_call(
        &addr,
        &JsonRpcRequest::new(json!(6), &token, "system.shutdown", Value::Null),
    )
    .await;
    assert!(shutdown_resp.error.is_none());
    assert_eq!(
        shutdown_resp.result.expect("shutdown result")["status"],
        json!("shutting_down")
    );

    server_handle
        .await
        .expect("daemon join")
        .expect("daemon serve exit cleanly");
    assert!(
        !endpoint_path.exists(),
        "daemon.endpoint should be removed after graceful shutdown"
    );
}

#[test]
fn binary_legacy_check_and_help_behave_as_drop_in_spotty_helper() {
    let bin = env!("CARGO_BIN_EXE_lms-spotty");

    let check_out = Command::new(bin)
        .args(["-n", "Spotty", "--check"])
        .output()
        .expect("run lms-spotty -n Spotty --check");
    assert!(
        check_out.status.success(),
        "--check failed: {}",
        String::from_utf8_lossy(&check_out.stderr)
    );
    let stdout = String::from_utf8_lossy(&check_out.stdout);
    assert!(
        stdout.starts_with(&format!("ok spotty v{SPOTTY_VERSION}\n")),
        "unexpected --check stdout: {stdout}"
    );
    assert!(
        stdout.contains("\"autoplay\": true"),
        "missing capability JSON: {stdout}"
    );

    let help_out = Command::new(bin)
        .arg("--help")
        .output()
        .expect("run lms-spotty --help");
    assert!(help_out.status.success(), "--help failed");
    let help_text = String::from_utf8_lossy(&help_out.stdout);
    assert!(
        help_text.contains("daemon"),
        "missing daemon subcommand: {help_text}"
    );
    assert!(
        help_text.contains("scan"),
        "missing scan subcommand: {help_text}"
    );
    assert!(
        help_text.contains("stream"),
        "missing stream subcommand: {help_text}"
    );
    assert!(
        help_text.contains("check"),
        "missing check subcommand: {help_text}"
    );
}
