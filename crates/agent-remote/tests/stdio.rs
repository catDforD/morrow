use agent_protocol::{RemoteRequest, RemoteResponse, RemoteRole};
use agent_remote::client::RemoteClient;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::Command;

#[tokio::test]
async fn workspace_agent_serves_sessions_without_persisting_managed_settings() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "morrow-remote-stdio-{}-{stamp}",
        std::process::id()
    ));
    let home = root.join("home");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&workspace).expect("create workspace");

    let mut child = Command::new(env!("CARGO_BIN_EXE_morrow-remote"));
    child
        .arg("workspace-agent")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--stdio")
        .env("HOME", &home)
        .current_dir(&workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = child.spawn().expect("spawn workspace agent");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let client = RemoteClient::connect(stdout, stdin)
        .await
        .expect("handshake");
    assert_eq!(client.hello().role, RemoteRole::WorkspaceAgent);

    assert!(matches!(
        client
            .request(1, RemoteRequest::WorkspaceConfiguration)
            .await
            .expect("workspace configuration"),
        RemoteResponse::WorkspaceConfiguration(_)
    ));
    let sessions = client
        .request(
            1,
            RemoteRequest::Http {
                method: "GET".to_string(),
                path: "/api/sessions".to_string(),
                body: None,
            },
        )
        .await
        .expect("session listing");
    let RemoteResponse::Http(sessions) = sessions else {
        panic!("HTTP response expected");
    };
    assert_eq!(sessions.status, 200);

    client
        .request(
            1,
            RemoteRequest::Shutdown {
                cancel_running: true,
            },
        )
        .await
        .expect("shutdown");
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .expect("workspace agent exit timeout")
        .expect("workspace agent status");
    assert!(status.success());
    assert!(!home.join(".morrow/web-models.json").exists());
    assert!(!home.join(".morrow/web-mcp.json").exists());

    let _ = std::fs::remove_dir_all(root);
}
