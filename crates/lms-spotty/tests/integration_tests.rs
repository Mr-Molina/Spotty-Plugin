//! End-to-end loopback JSON-RPC daemon and `lms-spotty` CLI binary tests.

#![allow(clippy::expect_used, clippy::unwrap_used)]

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

#[tokio::test]
async fn daemon_exercises_all_19_json_rpc_methods() {
    let temp = TempDir::new().expect("create tempdir");
    let config = DaemonConfig::new(temp.path()).with_sweep_interval(Duration::from_secs(60));
    let endpoint_path = config.endpoint_file.clone();

    let daemon = SpottyDaemon::bind(config)
        .await
        .expect("bind SpottyDaemon on loopback");
    let endpoint = read_endpoint_file(&endpoint_path).expect("read daemon.endpoint");
    let addr = endpoint.addr.clone();
    let token = endpoint.token.clone();
    let server_handle = tokio::spawn(async move { daemon.serve().await });

    // 1. system.ping
    let ping = rpc_call(
        &addr,
        &JsonRpcRequest::new(json!(1), &token, "system.ping", Value::Null),
    )
    .await;
    assert!(ping.error.is_none(), "system.ping failed: {ping:?}");

    // 2. plugin.init
    let init = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(2),
            &token,
            "plugin.init",
            json!({
                "disabled_formats": ["aac"],
                "arch": "x86_64-linux",
                "os": "linux",
                "helper_bin": "lms-spotty"
            }),
        ),
    )
    .await;
    assert!(init.error.is_none(), "plugin.init failed: {init:?}");

    // 3. opml.recent_searches (add, list, delete_term, clear)
    let rs_add = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(3),
            &token,
            "opml.recent_searches",
            json!({ "action": "add", "query": "daft punk" }),
        ),
    )
    .await;
    assert!(
        rs_add.error.is_none(),
        "opml.recent_searches add failed: {rs_add:?}"
    );
    assert_eq!(rs_add.result.as_ref().expect("rs_add")["count"], json!(1));

    let rs_del = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(4),
            &token,
            "opml.recent_searches",
            json!({ "action": "delete_term", "term": "daft punk" }),
        ),
    )
    .await;
    assert!(
        rs_del.error.is_none(),
        "opml.recent_searches delete_term failed: {rs_del:?}"
    );
    assert_eq!(rs_del.result.as_ref().expect("rs_del")["count"], json!(0));

    // 4. opml.feed (root/home route without network)
    let feed = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(5),
            &token,
            "opml.feed",
            json!({ "route": "search_history" }),
        ),
    )
    .await;
    assert_ne!(
        feed.error.as_ref().map(|e| e.code),
        Some(ERR_METHOD_NOT_FOUND),
        "opml.feed must be registered: {feed:?}"
    );

    // 5. protocol.track_gain (with track + album gain for MED-03)
    let gain = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(6),
            &token,
            "protocol.track_gain",
            json!({
                "server_mode": "album",
                "remote_replay_gain": -4.2,
                "remote_album_gain": -6.5
            }),
        ),
    )
    .await;
    assert!(gain.error.is_none(), "protocol.track_gain failed: {gain:?}");
    let gain_val = gain.result.as_ref().expect("gain result")["gain"]
        .as_f64()
        .expect("gain float");
    assert!(
        (gain_val - (-6.5)).abs() < 0.01,
        "expected album gain -6.5, got {gain_val}"
    );

    // 6. protocol.metadata (registered dispatch; no network auth expected)
    let meta = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(7),
            &token,
            "protocol.metadata",
            json!({ "url": "spotify:track:4uLU6hMCjMI75M1A2tKUQC" }),
        ),
    )
    .await;
    assert_ne!(
        meta.error.as_ref().map(|e| e.code),
        Some(ERR_METHOD_NOT_FOUND),
        "protocol.metadata must be registered: {meta:?}"
    );

    // 7. protocol.explode_playlist (registered dispatch)
    let explode = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(8),
            &token,
            "protocol.explode_playlist",
            json!({ "uri": "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M" }),
        ),
    )
    .await;
    assert_ne!(
        explode.error.as_ref().map(|e| e.code),
        Some(ERR_METHOD_NOT_FOUND),
        "protocol.explode_playlist must be registered: {explode:?}"
    );

    // 8. importer.needs_update
    let needs = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(9),
            &token,
            "importer.needs_update",
            json!({ "include_playlists": true }),
        ),
    )
    .await;
    assert!(
        needs.error.is_none(),
        "importer.needs_update failed: {needs:?}"
    );

    // 9. integrations.dstm (registered dispatch)
    let dstm = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(10),
            &token,
            "integrations.dstm",
            json!({ "seed_uri": "spotify:track:4uLU6hMCjMI75M1A2tKUQC" }),
        ),
    )
    .await;
    assert_ne!(
        dstm.error.as_ref().map(|e| e.code),
        Some(ERR_METHOD_NOT_FOUND),
        "integrations.dstm must be registered: {dstm:?}"
    );

    // 10. integrations.home_extras
    let extras = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(11),
            &token,
            "integrations.home_extras",
            json!({ "tag": "home" }),
        ),
    )
    .await;
    assert!(
        extras.error.is_none(),
        "integrations.home_extras failed: {extras:?}"
    );

    // 11. integrations.last_mix (registered dispatch)
    let last_mix = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(12),
            &token,
            "integrations.last_mix",
            json!({ "artist": "Daft Punk", "track": "Digital Love" }),
        ),
    )
    .await;
    assert_ne!(
        last_mix.error.as_ref().map(|e| e.code),
        Some(ERR_METHOD_NOT_FOUND),
        "integrations.last_mix must be registered: {last_mix:?}"
    );

    // 12. settings.basic
    let basic = rpc_call(
        &addr,
        &JsonRpcRequest::new(json!(13), &token, "settings.basic", json!({})),
    )
    .await;
    assert!(basic.error.is_none(), "settings.basic failed: {basic:?}");

    // 13. settings.auth_check
    let auth_check = rpc_call(
        &addr,
        &JsonRpcRequest::new(json!(14), &token, "settings.auth_check", json!({})),
    )
    .await;
    assert!(
        auth_check.error.is_none(),
        "settings.auth_check failed: {auth_check:?}"
    );

    // 14. settings.oauth_prepare (dispatches; may hit external relay 429)
    let oauth_prep = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(15),
            &token,
            "settings.oauth_prepare",
            json!({
                "lms_redirect_uri":
                    "http://127.0.0.1:9000/plugins/Spotty/settings/callback"
            }),
        ),
    )
    .await;
    assert_ne!(
        oauth_prep.error.as_ref().map(|e| e.code),
        Some(ERR_METHOD_NOT_FOUND),
        "settings.oauth_prepare must be registered: {oauth_prep:?}"
    );

    // 15. settings.oauth_callback (CRIT-05: fail closed on missing state)
    let oauth_cb = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(16),
            &token,
            "settings.oauth_callback",
            json!({ "code": "abc" }),
        ),
    )
    .await;
    assert!(
        oauth_cb.error.is_none(),
        "settings.oauth_callback failed: {oauth_cb:?}"
    );
    assert_eq!(
        oauth_cb.result.as_ref().expect("oauth_cb")["auth_error"],
        json!("Missing OAuth state")
    );

    // 16. settings.player (dispatches; errors if unauthenticated)
    let player = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(17),
            &token,
            "settings.player",
            json!({ "player_id": "aa:bb:cc:dd:ee:ff" }),
        ),
    )
    .await;
    assert_ne!(
        player.error.as_ref().map(|e| e.code),
        Some(ERR_METHOD_NOT_FOUND),
        "settings.player must be registered: {player:?}"
    );

    // 17. settings.player_auth_check
    let player_auth = rpc_call(
        &addr,
        &JsonRpcRequest::new(
            json!(18),
            &token,
            "settings.player_auth_check",
            json!({ "player_id": "aa:bb:cc:dd:ee:ff", "action": "poll" }),
        ),
    )
    .await;
    assert!(
        player_auth.error.is_none(),
        "settings.player_auth_check failed: {player_auth:?}"
    );

    // 18. settings.playlist_folders
    let folders = rpc_call(
        &addr,
        &JsonRpcRequest::new(json!(19), &token, "settings.playlist_folders", json!({})),
    )
    .await;
    assert!(
        folders.error.is_none(),
        "settings.playlist_folders failed: {folders:?}"
    );

    // 19. system.shutdown
    let shutdown = rpc_call(
        &addr,
        &JsonRpcRequest::new(json!(20), &token, "system.shutdown", Value::Null),
    )
    .await;
    assert!(
        shutdown.error.is_none(),
        "system.shutdown failed: {shutdown:?}"
    );
    assert_eq!(
        shutdown.result.as_ref().expect("shutdown")["status"],
        json!("shutting_down")
    );

    server_handle
        .await
        .expect("daemon join")
        .expect("daemon serve exit cleanly");
}
