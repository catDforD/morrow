use super::*;
use agent_config::{AgentConfig, ModelContextLimits, ServerAppConfig, ServerConfig};
use agent_model::{OpenAiCompatClient, OpenAiCompatConfig};
use agent_protocol::{
    ModelInvocation, PermissionMode, ReasoningLevel, ReasoningProfile, ShellPolicy,
};
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tower::ServiceExt;

static ENV_LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();

fn router(options: ServerOptions) -> Result<Router, ModelRegistryError> {
    build_router(options, ServerAccessPolicy::browser(None)).map(|(router, _)| router)
}

struct HomeGuard {
    previous: Option<std::ffi::OsString>,
}

impl HomeGuard {
    fn set(path: &std::path::Path) -> Self {
        let previous = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", path);
        }
        Self { previous }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe {
                std::env::set_var("HOME", value);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }
    }
}

fn test_options() -> ServerOptions {
    let root = unique_test_dir("options");
    let client = OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
        base_url: "http://127.0.0.1:1/v1".to_string(),
        model: "test-model".to_string(),
        api_key: "secret-test-key".to_string(),
        timeout: Duration::from_secs(1),
        max_retries: 1,
    })
    .expect("client");
    ServerOptions {
        host: "127.0.0.1".parse().expect("host"),
        port: 0,
        fallback_model: Some(FallbackModel {
            provider_name: "Current config".to_string(),
            model_id: "test-model".to_string(),
            model_name: "test-model".to_string(),
            client: Some(client),
            limits: ModelContextLimits {
                context_window_tokens: 65_536,
                reserved_output_tokens: 8_192,
            },
            reasoning_profile: ReasoningProfile::None,
        }),
        model_store_path: root.join("web-models.json"),
        mcp_store_path: root.join("web-mcp.json"),
        command_store_path: root.join("commands"),
        subagent_store_path: root.join("subagents.json"),
        hook_home_dir: root.clone(),
        system_prompt: "system".to_string(),
        workspace_instructions: Arc::new(WorkspaceInstructionsCache::new(&root)),
        context_config: ContextConfig {
            auto_compact: false,
            auto_compact_threshold: 0.835,
            retain_recent_turns: 2,
            summary_target_tokens: 256,
            compact_max_retries: 2,
            max_context_tokens: Some(300_000),
        },
        workspace_root: root.clone(),
        workspace_location: WorkspaceLocation::Local { path: root.clone() },
        config_path: Some(root.join("morrow.toml")),
        config_diagnostics: Vec::new(),
        permissions: PermissionProfile::for_mode(DEFAULT_WEB_PERMISSION_MODE),
        auto_approve_workspace_writes: true,
        permission_ceiling: PermissionMode::DangerFullAccess,
        mcp_servers: Vec::new(),
        tools: ToolsConfig::default(),
        default_session_name: "default".to_string(),
    }
}

fn test_state() -> AppState {
    test_state_with_options(test_options())
}

fn test_state_with_options(options: ServerOptions) -> AppState {
    let model_registry = ModelRegistry::load(
        options.model_store_path.clone(),
        &options.workspace_root,
        options.fallback_model.clone(),
    )
    .expect("model registry");
    let mcp_registry =
        McpRegistry::load(options.mcp_store_path.clone(), options.mcp_servers.clone())
            .expect("MCP registry");
    let command_registry = CommandRegistry::new(options.command_store_path.clone());
    let hook_manager = HookManager::new(
        options.hook_home_dir.clone(),
        options.workspace_root.clone(),
    );
    let subagent_registry =
        SubagentRegistry::load(options.subagent_store_path.clone()).expect("subagent registry");
    AppState {
        inner: Arc::new(ServerState {
            options,
            model_registry,
            mcp_registry,
            command_registry,
            hook_manager,
            subagent_registry,
            sessions: Mutex::new(HashMap::new()),
            mcp_cache: RwLock::new(Arc::new(McpToolCache::new())),
            access_policy: ServerAccessPolicy::Browser { token: None },
            shutting_down: AtomicBool::new(false),
        }),
    }
}

fn unique_test_dir(name: &str) -> PathBuf {
    let stamp = agent_runtime::timestamp_ms();
    let path = std::env::temp_dir().join(format!(
        "morrow-server-{name}-{stamp}-{}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

#[test]
fn server_options_load_workspace_instructions_and_preserve_diagnostics() {
    let root = unique_test_dir("agents-options");
    fs::write(
        root.join("AGENTS.md"),
        "Use the workspace release checklist.",
    )
    .expect("write AGENTS.md");
    let loaded_config = || LoadedServerConfig {
        config: ServerAppConfig {
            agent: AgentConfig {
                system_prompt: "base system prompt".to_string(),
            },
            context: ContextConfig {
                auto_compact: false,
                auto_compact_threshold: 0.835,
                retain_recent_turns: 2,
                summary_target_tokens: 256,
                compact_max_retries: 2,
                max_context_tokens: Some(300_000),
            },
            permissions: PermissionProfile::for_mode(DEFAULT_WEB_PERMISSION_MODE),
            workspace_write_require_approval: false,
            mcp_servers: Vec::new(),
            server: ServerConfig::default(),
            tools: ToolsConfig::default(),
        },
        path: None,
        model: None,
        diagnostics: vec!["existing diagnostic".to_string()],
    };

    let options = server_options_from_loaded_config(
        "127.0.0.1".parse().expect("host"),
        0,
        root.clone(),
        &root,
        loaded_config(),
        "default".to_string(),
    )
    .expect("server options");
    // options.system_prompt 只保留配置层 base；AGENTS.md 由缓存每轮拼装。
    assert_eq!(options.system_prompt, "base system prompt");
    assert!(
        options
            .workspace_instructions
            .apply(&options.system_prompt)
            .contains("Use the workspace release checklist.")
    );
    assert_eq!(options.config_diagnostics, ["existing diagnostic"]);

    fs::write(root.join("AGENTS.md"), [0xff]).expect("write invalid AGENTS.md");
    let invalid = server_options_from_loaded_config(
        "127.0.0.1".parse().expect("host"),
        0,
        root.clone(),
        &root,
        loaded_config(),
        "default".to_string(),
    )
    .expect("server options with invalid instructions");
    assert_eq!(invalid.system_prompt, "base system prompt");
    assert_eq!(invalid.config_diagnostics.len(), 2);
    assert_eq!(invalid.config_diagnostics[0], "existing diagnostic");
    assert!(invalid.config_diagnostics[1].contains("not valid UTF-8"));

    fs::remove_dir_all(root).expect("remove root");
}

#[tokio::test]
async fn status_response_omits_api_key() {
    let response = status(State(test_state())).await;
    let value = serde_json::to_value(response.0).expect("status json");

    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(value["permissions"]["mode"], "workspace_write");
    assert!(
        value["subagent_store_path"]
            .as_str()
            .is_some_and(|path| path.ends_with("subagents.json"))
    );
    assert!(!value.to_string().contains("secret-test-key"));
}

#[tokio::test]
async fn status_response_includes_workspace_instruction_diagnostics() {
    let mut options = test_options();
    options.config_diagnostics = vec!["AGENTS.md could not be loaded".to_string()];
    let response = status(State(test_state_with_options(options))).await;
    let value = serde_json::to_value(response.0).expect("status json");

    assert_eq!(
        value["config_diagnostics"],
        json!(["AGENTS.md could not be loaded"])
    );
}

#[tokio::test]
async fn embedded_subagent_settings_routes_manage_the_global_profile_list() {
    let server = EmbeddedServer::new(test_options()).expect("embedded server");

    let settings = server
        .request("GET", "/api/subagent-settings", None)
        .await
        .expect("read subagent settings");
    assert_eq!(settings.status, 200);
    let settings = settings.body.expect("settings body");
    assert_eq!(settings["profiles"].as_array().map(Vec::len), Some(22));
    assert_eq!(settings["profiles"][0]["id"], "builtin-01");

    let created = server
        .request(
            "POST",
            "/api/subagents",
            Some(json!({"name": "测试成员", "avatar_data_url": null})),
        )
        .await
        .expect("create subagent");
    assert_eq!(created.status, 200);
    let id = created.body.expect("created body")["id"]
        .as_str()
        .expect("created id")
        .to_string();

    let updated = server
        .request(
            "PUT",
            &format!("/api/subagents/{id}"),
            Some(json!({"name": "更新成员", "avatar_data_url": null})),
        )
        .await
        .expect("update subagent");
    assert_eq!(updated.status, 200);
    assert_eq!(updated.body.expect("updated body")["name"], "更新成员");

    let duplicate = server
        .request(
            "POST",
            "/api/subagents",
            Some(json!({"name": "后藤一里", "avatar_data_url": null})),
        )
        .await
        .expect("duplicate response");
    assert_eq!(duplicate.status, 409);

    let deleted = server
        .request("DELETE", &format!("/api/subagents/{id}"), None)
        .await
        .expect("delete subagent");
    assert_eq!(deleted.status, 204);

    let reset = server
        .request("POST", "/api/subagent-settings/reset", None)
        .await
        .expect("reset subagents");
    assert_eq!(reset.status, 200);
    assert_eq!(
        reset.body.expect("reset body")["profiles"]
            .as_array()
            .map(Vec::len),
        Some(22)
    );
}

#[tokio::test]
async fn embedded_hooks_routes_list_trust_and_revoke_project_configuration() {
    let mut options = test_options();
    options.workspace_root = unique_test_dir("hooks-api-workspace");
    options.workspace_location = WorkspaceLocation::Local {
        path: options.workspace_root.clone(),
    };
    let hook_home = unique_test_dir("hooks-api-home");
    options.hook_home_dir = hook_home.clone();
    let project_config = options.workspace_root.join(".morrow").join("hooks.toml");
    fs::create_dir_all(project_config.parent().expect("project hook parent"))
        .expect("create project hook parent");
    fs::write(
            &project_config,
            "schema_version = 1\n[[hooks]]\nid = \"project\"\nevent = \"before_prompt\"\ncommand = [\"true\"]\n",
        )
        .expect("write project hooks");
    let server = EmbeddedServer::new(options).expect("embedded server");

    let listed = server
        .request("GET", "/api/hooks", None)
        .await
        .expect("list hooks");
    assert_eq!(listed.status, 200);
    let listed = listed.body.expect("list body");
    assert_eq!(listed["project_trusted"], false);
    assert_eq!(listed["hooks"][0]["active"], false);

    let trusted = server
        .request("POST", "/api/hooks/trust", None)
        .await
        .expect("trust hooks");
    assert_eq!(trusted.status, 200);
    let trusted = trusted.body.expect("trust body");
    assert_eq!(trusted["project_trusted"], true);
    assert_eq!(trusted["hooks"][0]["active"], true);
    assert!(hook_home.join(".morrow/hook-trust.json").is_file());

    let revoked = server
        .request("POST", "/api/hooks/revoke", None)
        .await
        .expect("revoke hooks");
    assert_eq!(revoked.status, 200);
    assert_eq!(revoked.body.expect("revoke body")["project_trusted"], false);
}

#[tokio::test]
async fn before_prompt_hook_denial_persists_audit_without_creating_turn() {
    let _lock = ENV_LOCK.get_or_init(|| AsyncMutex::new(())).lock().await;
    let session_home = unique_test_dir("before-prompt-deny-session-home");
    let _home = HomeGuard::set(&session_home);
    let mut options = test_options();
    options.workspace_root = unique_test_dir("before-prompt-deny-workspace");
    options.workspace_location = WorkspaceLocation::Local {
        path: options.workspace_root.clone(),
    };
    let hook_home = unique_test_dir("before-prompt-deny-home");
    options.hook_home_dir = hook_home.clone();
    let script = options.workspace_root.join("deny-prompt-hook.sh");
    fs::write(
            &script,
            "#!/bin/sh\nprintf '%s' '{\"decision\":\"deny\",\"reason\":\"blocked by policy\",\"additional_context\":[]}'\n",
        )
        .expect("write deny script");
    let user_config = hook_home.join(".morrow/hooks.toml");
    fs::create_dir_all(user_config.parent().expect("user hook parent"))
        .expect("create user hook parent");
    fs::write(
            &user_config,
            format!(
                "schema_version = 1\n[[hooks]]\nid = \"deny-prompt\"\nevent = \"before_prompt\"\ncommand = [\"/bin/sh\", {:?}]\n",
                script.to_string_lossy()
            ),
        )
        .expect("write user hooks");
    let state = test_state_with_options(options);
    SessionStore::for_workspace(&state.inner.options.workspace_root, "default")
        .expect("session store")
        .reset()
        .expect("create session");
    let tx = session_sender(&state, "default").await;

    let error = start_turn(
        state.clone(),
        "default".to_string(),
        StartTurnRequest {
            prompt: "blocked prompt content".to_string(),
            prompt_resolved: true,
            permission_mode: None,
            model_selection: None,
            resolved_model: None,
            mcp_servers: None,
            subagent_identities: None,
            subagent_role_overrides: None,
            subagent_role_models: None,
        },
        tx,
    )
    .await
    .expect_err("prompt must be denied");
    assert!(error.contains("blocked by middleware"), "{error}");

    let resources = ensure_session_resources(&state, "default")
        .await
        .expect("session resources");
    let projection = resources.handle.projection().await;
    assert!(projection.turns.is_empty());
    assert_eq!(projection.middleware_audit.len(), 1);
    assert_eq!(
        projection.middleware_audit[0].outcome,
        agent_protocol::MiddlewareOutcome::Deny
    );
    let exported = String::from_utf8(
        resources
            .handle
            .export_document_bytes()
            .await
            .expect("export session"),
    )
    .expect("export UTF-8");
    // 被拒 prompt 以 prompt_rejected fact 落盘（只作审计，不创建 turn）。
    let rejected = exported
        .lines()
        .skip(1)
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|line| line["fact"]["type"] == "prompt_rejected")
        .expect("prompt_rejected fact");
    assert_eq!(
        rejected["fact"]["data"]["prompt"],
        serde_json::json!("blocked prompt content")
    );
    assert!(
        rejected["fact"]["data"]["reasons"]
            .as_array()
            .expect("reasons array")
            .iter()
            .any(|reason| reason
                .as_str()
                .is_some_and(|reason| reason.contains("blocked by policy")))
    );
}

#[test]
fn router_registers_model_routes_without_conflicts() {
    let _ = router(test_options()).expect("router");
}

#[test]
fn embedded_index_references_assets_present_at_the_tauri_root() {
    let html = include_str!("../assets/index.html");

    assert!(html.contains(r#"src="/app.js""#));
    assert!(html.contains(r#"href="/style.css""#));
}

#[tokio::test]
async fn browser_router_serves_root_and_legacy_asset_paths() {
    let router = router(test_options()).expect("browser router");
    for (path, content_type) in [
        ("/app.js", "application/javascript; charset=utf-8"),
        ("/style.css", "text/css; charset=utf-8"),
        ("/assets/app.js", "application/javascript; charset=utf-8"),
        ("/assets/style.css", "text/css; charset=utf-8"),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("asset request"),
            )
            .await
            .expect("asset response");

        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&HeaderValue::from_static(content_type)),
            "{path}"
        );
    }
}

#[tokio::test]
async fn embedded_settings_prepare_ephemeral_remote_turn_runtime() {
    let server = EmbeddedServer::new(test_options()).expect("embedded server");
    let provider = server
        .request(
            "POST",
            "/api/model-providers",
            Some(serde_json::json!({
                "name": "Managed",
                "base_url": "https://models.example/v1",
                "api_key": "managed-model-secret",
                "enabled": true,
                "timeout_secs": 30,
                "models": [{
                    "id": "managed-model",
                    "name": "Managed model",
                    "context_window_tokens": 32_000,
                    "reserved_output_tokens": 4_000,
                    "supports_tools": true,
                    "reasoning_profile": "none"
                }]
            })),
        )
        .await
        .expect("create provider");
    assert_eq!(provider.status, 200);
    let provider_id = provider.body.expect("provider body")["id"]
        .as_str()
        .expect("provider id")
        .to_string();
    let mcp = server
        .request(
            "POST",
            "/api/mcp-servers",
            Some(serde_json::json!({
                "name": "managed-mcp",
                "transport": "stdio",
                "command": "managed-mcp",
                "args": [],
                "env": {"TOKEN": "managed-mcp-secret"},
                "enabled": true,
                "startup_timeout_sec": 10,
                "tool_timeout_sec": 60
            })),
        )
        .await
        .expect("create MCP server");
    assert_eq!(mcp.status, 200);

    let turn = server
        .prepare_remote_turn(
            "default",
            serde_json::json!({
                "type": "start_turn",
                "data": {
                    "request_id": "request-1",
                    "prompt": "hello",
                    "prompt_resolved": true,
                    "permission_mode": "workspace_write",
                    "model_selection": {
                        "provider_id": provider_id,
                        "model_id": "managed-model",
                        "reasoning": "off"
                    }
                }
            }),
        )
        .await
        .expect("prepare remote turn");

    let RemoteTurnModel::Managed(model) = turn.model else {
        panic!("managed model expected");
    };
    assert_eq!(model.api_key, "managed-model-secret");
    assert_eq!(turn.managed_mcp_servers.len(), 1);
    assert_eq!(turn.subagent_identities.len(), 22);
    assert_eq!(turn.subagent_identities[0].id, "builtin-01");
    assert_eq!(
        turn.managed_mcp_servers[0]
            .env
            .get("TOKEN")
            .map(String::as_str),
        Some("managed-mcp-secret")
    );

    let command = server
        .prepare_remote_subagent_message(
            "default",
            serde_json::json!({
                "type": "send_subagent",
                "data": {
                    "request_id": "request-2",
                    "instance_id": "subagent-1",
                    "message": "continue",
                    "model_selection": {
                        "provider_id": provider_id,
                        "model_id": "managed-model",
                        "reasoning": "off"
                    }
                }
            }),
        )
        .await
        .expect("prepare remote subagent follow-up");
    assert_eq!(command.subagent_roles.len(), SubagentRole::ALL.len());
    let Some(RemoteTurnModel::Managed(resume_model)) = command.resume_model else {
        panic!("managed resume model expected");
    };
    assert_eq!(resume_model.api_key, "managed-model-secret");
    assert!(!format!("{resume_model:?}").contains("managed-model-secret"));
}

#[tokio::test]
async fn workspace_accepts_managed_model_resolved_by_desktop() {
    let _lock = ENV_LOCK.get_or_init(|| AsyncMutex::new(())).lock().await;
    let home = unique_test_dir("managed-workspace-home");
    let _home = HomeGuard::set(&home);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind model listener");
    let mut options = test_options();
    options.fallback_model = None;
    let server = EmbeddedServer::new_workspace(options).expect("workspace server");
    let created = server
        .request("POST", "/api/sessions", Some(json!({ "name": "remote" })))
        .await
        .expect("create session");
    assert_eq!(created.status, StatusCode::CREATED.as_u16());
    let mut subscription = server
        .subscribe_session("remote")
        .await
        .expect("subscribe session");
    let remote_model = RemoteModelSpec {
        base_url: format!(
            "http://{}/v1",
            listener.local_addr().expect("model address")
        ),
        model: "deepseek-v4-pro".to_string(),
        api_key: "remote-model-secret".to_string(),
        timeout_secs: 30,
        context_window_tokens: 65_536,
        reserved_output_tokens: 8_192,
        reasoning_profile: ReasoningProfile::Deepseek,
        supports_tools: true,
        invocation: ModelInvocation {
            provider_id: "opencode".to_string(),
            provider_name: "opencode".to_string(),
            model_id: "deepseek-v4-pro".to_string(),
            model_name: "DeepSeek V4 Pro".to_string(),
            reasoning: ReasoningLevel::High,
        },
    };
    let subagent_roles = SubagentRole::ALL
        .into_iter()
        .map(|role| RemoteSubagentRoleSpec {
            role,
            overrides: SubagentRoleOverride::default(),
            model: RemoteTurnModel::Managed(remote_model.clone()),
        })
        .collect();

    server
        .start_remote_turn(RemoteTurnSpec {
            session: "remote".to_string(),
            request_id: "request-remote-model".to_string(),
            prompt: "hello".to_string(),
            permission_mode: Some(PermissionMode::WorkspaceWrite),
            model: RemoteTurnModel::Managed(remote_model),
            managed_mcp_servers: Vec::new(),
            subagent_identities: agent_protocol::default_subagent_identities(),
            subagent_roles,
        })
        .await
        .expect("start remote turn");

    let accepted = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let message = subscription.recv().await.map_err(|error| error.to_string())?;
                if matches!(
                    message,
                    SessionStreamFrame::Event(event)
                        if matches!(event.update, agent_protocol::SessionUpdate::OperationReplaced(Some(_)))
                ) {
                    return Ok::<(), String>(());
                }
            }
        })
        .await
        .expect("remote turn acceptance event");

    assert!(accepted.is_ok(), "remote turn was rejected: {accepted:?}");
    server.shutdown(true).await;
}

#[tokio::test]
async fn workspace_embedded_server_keeps_managed_settings_in_memory() {
    let options = test_options();
    let model_store = options.model_store_path.clone();
    let mcp_store = options.mcp_store_path.clone();
    let server = EmbeddedServer::new_workspace(options).expect("workspace server");

    let response = server
        .request("GET", "/api/model-settings", None)
        .await
        .expect("embedded response");

    assert_eq!(response.status, 404);
    assert!(!model_store.exists());
    assert!(!mcp_store.exists());
}

#[tokio::test]
async fn desktop_access_bootstraps_cookie_and_rejects_unauthorized_requests() {
    let mut options = test_options();
    options.port = 43123;
    let (router, _) = build_router(options, ServerAccessPolicy::desktop("desktop-test-token"))
        .expect("desktop router");
    let host = "127.0.0.1:43123";

    let unauthorized = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header(header::HOST, host)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let wrong_host = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header(header::HOST, "localhost:43123")
                .header(header::COOKIE, "morrow_session=desktop-test-token")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(wrong_host.status(), StatusCode::UNAUTHORIZED);

    let bootstrap = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/?bootstrap=desktop-test-token")
                .header(header::HOST, host)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(bootstrap.status(), StatusCode::SEE_OTHER);
    let cookie = bootstrap
        .headers()
        .get(header::SET_COOKIE)
        .expect("session cookie")
        .to_str()
        .expect("cookie text")
        .to_string();
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));

    let authorized = router
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header(header::HOST, host)
                .header(header::COOKIE, "morrow_session=desktop-test-token")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(authorized.status(), StatusCode::OK);
    assert!(
        authorized
            .headers()
            .contains_key(header::CONTENT_SECURITY_POLICY)
    );
}

#[tokio::test]
async fn browser_access_with_token_requires_bootstrap_and_origin() {
    let mut options = test_options();
    options.port = 43125;
    let (router, _) = build_router(
        options,
        ServerAccessPolicy::browser(Some("browser-test-token".to_string())),
    )
    .expect("browser router");
    let host = "127.0.0.1:43125";

    let unauthorized = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header(header::HOST, host)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let wrong_token = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/?bootstrap=wrong-token")
                .header(header::HOST, host)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(wrong_token.status(), StatusCode::UNAUTHORIZED);

    let bootstrap = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/?bootstrap=browser-test-token")
                .header(header::HOST, host)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(bootstrap.status(), StatusCode::SEE_OTHER);
    let cookie = bootstrap
        .headers()
        .get(header::SET_COOKIE)
        .expect("session cookie")
        .to_str()
        .expect("cookie text")
        .to_string();
    assert!(cookie.contains("morrow_session=browser-test-token"));
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));

    let authorized = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header(header::HOST, host)
                .header(header::COOKIE, "morrow_session=browser-test-token")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(authorized.status(), StatusCode::OK);

    let cross_origin = router
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/commands/resolve")
                .header(header::HOST, host)
                .header(header::ORIGIN, "https://example.com")
                .header(header::COOKIE, "morrow_session=browser-test-token")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"input":"hello"}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(cross_origin.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn browser_access_without_token_passes_through_for_no_auth() {
    let (router, _) =
        build_router(test_options(), ServerAccessPolicy::browser(None)).expect("browser router");
    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn desktop_access_rejects_cross_origin_mutations_and_websockets() {
    let mut options = test_options();
    options.port = 43124;
    let (router, _) =
        build_router(options, ServerAccessPolicy::desktop("token")).expect("desktop router");
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/commands/resolve")
        .header(header::HOST, "127.0.0.1:43124")
        .header(header::ORIGIN, "https://example.com")
        .header(header::COOKIE, "morrow_session=token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"input":"hello"}"#))
        .expect("request");

    let response = router.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let websocket = Request::builder()
        .uri("/api/sessions/default/ws")
        .header(header::HOST, "127.0.0.1:43124")
        .header(header::ORIGIN, "https://example.com")
        .header(header::COOKIE, "morrow_session=token")
        .body(Body::empty())
        .expect("request");
    let response = router.oneshot(websocket).await.expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn spawned_local_server_reports_address_and_shuts_down() {
    let mut server = spawn_local(test_options(), ServerAccessPolicy::browser(None))
        .await
        .expect("spawn server");

    assert_ne!(server.addr().port(), 0);
    assert!(server.base_url().starts_with("http://127.0.0.1:"));
    assert!(server.activity().await.is_idle());
    server
        .shutdown(ShutdownPolicy::RequireIdle)
        .await
        .expect("shutdown server");
}

#[tokio::test]
async fn require_idle_rejection_keeps_the_server_available() {
    let mut server = spawn_local(test_options(), ServerAccessPolicy::browser(None))
        .await
        .expect("spawn server");
    let worker = tokio::spawn(std::future::pending::<()>());
    {
        let mut sessions = server.state.inner.sessions.lock().await;
        let runtime = sessions
            .entry("default".to_string())
            .or_insert_with(SessionRuntime::new);
        runtime.running = Some(RunningTurn {
            turn_id: "turn-1".to_string(),
            cancellation: CancellationToken::new(),
            handle: worker.abort_handle(),
        });
    }

    let result = server.shutdown(ShutdownPolicy::RequireIdle).await;

    assert!(matches!(result, Err(ServerError::RunningTurns(1))));
    assert_eq!(server.activity().await.running_turns, 1);
    server
        .shutdown(ShutdownPolicy::CancelRunning {
            timeout: Duration::from_millis(10),
        })
        .await
        .expect("cancel and shutdown server");
}

#[tokio::test]
async fn reset_rejects_running_session() {
    let _lock = ENV_LOCK.get_or_init(|| AsyncMutex::new(())).lock().await;
    let home = unique_test_dir("reset-running-home");
    let _home = HomeGuard::set(&home);
    let state = test_state();
    SessionStore::for_workspace(&state.inner.options.workspace_root, "default")
        .expect("store")
        .reset()
        .expect("create session");
    let worker = tokio::spawn(std::future::pending::<()>());
    {
        let mut sessions = state.inner.sessions.lock().await;
        let runtime = sessions
            .entry("default".to_string())
            .or_insert_with(SessionRuntime::new);
        runtime.running = Some(RunningTurn {
            turn_id: "turn-1".to_string(),
            cancellation: CancellationToken::new(),
            handle: worker.abort_handle(),
        });
    }

    let result = reset_session(State(state), Path("default".to_string())).await;

    assert!(matches!(
        result,
        Err(ApiError {
            status: StatusCode::CONFLICT,
            ..
        })
    ));
    worker.abort();
    let _ = worker.await;
}

#[tokio::test]
async fn cancellation_keeps_session_reserved_until_worker_cleanup() {
    let state = test_state();
    let tx = session_sender(&state, "default").await;
    let worker = tokio::spawn(std::future::pending::<()>());
    {
        let mut sessions = state.inner.sessions.lock().await;
        let runtime = sessions
            .entry("default".to_string())
            .or_insert_with(SessionRuntime::new);
        runtime.running = Some(RunningTurn {
            turn_id: "turn-1".to_string(),
            cancellation: CancellationToken::new(),
            handle: worker.abort_handle(),
        });
    }

    cancel_turn(&state, "default", "turn-1".to_string(), &tx).await;

    let sessions = state.inner.sessions.lock().await;
    let running = sessions
        .get("default")
        .and_then(|runtime| runtime.running.as_ref())
        .expect("running turn remains reserved");
    assert!(running.cancellation.is_cancelled());
    assert!(
        !worker.is_finished(),
        "cooperative cancellation must not abort the worker immediately"
    );
    drop(sessions);

    clear_running_turn(&state, "default", "turn-1").await;
    worker.abort();
    let _ = worker.await;
}

#[tokio::test]
async fn worker_panic_releases_session_slot() {
    let state = test_state();
    let tx = session_sender(&state, "default").await;
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let worker = tokio::spawn(async move {
        let _ = release_rx.await;
        panic!("test worker panic");
    });
    {
        let mut sessions = state.inner.sessions.lock().await;
        let runtime = sessions
            .entry("default".to_string())
            .or_insert_with(SessionRuntime::new);
        runtime.running = Some(RunningTurn {
            turn_id: "turn-panic".to_string(),
            cancellation: CancellationToken::new(),
            handle: worker.abort_handle(),
        });
    }

    let supervisor = tokio::spawn(supervise_turn_worker(
        state.clone(),
        "default".to_string(),
        "turn-panic".to_string(),
        tx,
        worker,
    ));
    release_tx.send(()).expect("release worker");
    tokio::time::timeout(std::time::Duration::from_secs(1), supervisor)
        .await
        .expect("supervisor must finish")
        .expect("supervisor task");

    let sessions = state.inner.sessions.lock().await;
    assert!(
        sessions
            .get("default")
            .is_some_and(|runtime| runtime.running.is_none())
    );
}

#[tokio::test]
async fn create_session_saves_empty_session() {
    let lock = ENV_LOCK.get_or_init(|| AsyncMutex::new(())).lock().await;
    let home = unique_test_dir("create-home");
    let previous_home = std::env::var_os("HOME");
    unsafe {
        std::env::set_var("HOME", &home);
    }

    let state = test_state();
    let workspace = state.inner.options.workspace_root.clone();
    let response = create_session(
        State(state.clone()),
        Json(CreateSessionRequest {
            name: "fresh".to_string(),
        }),
    )
    .await
    .expect("create session");
    let store = SessionStore::for_workspace(&workspace, "fresh").expect("store");
    let session = store.load_existing().expect("load created session");

    assert_eq!(response.0, StatusCode::CREATED);
    assert_eq!(response.1.0.name, "fresh");
    assert!(!response.1.0.archived);
    let reset = reset_session(State(state), Path("fresh".to_string()))
        .await
        .expect("reset session");
    assert_eq!(reset.0.name, "fresh");
    assert_eq!(reset.0.turns, 0);
    assert!(!reset.0.archived);
    assert_eq!(session, Session::new());
    assert!(store.path().is_file());

    match previous_home {
        Some(value) => unsafe {
            std::env::set_var("HOME", value);
        },
        None => unsafe {
            std::env::remove_var("HOME");
        },
    }
    drop(lock);
}

#[tokio::test]
async fn create_session_rejects_existing_session() {
    let lock = ENV_LOCK.get_or_init(|| AsyncMutex::new(())).lock().await;
    let home = unique_test_dir("create-existing-home");
    let previous_home = std::env::var_os("HOME");
    unsafe {
        std::env::set_var("HOME", &home);
    }

    let state = test_state();
    let store = SessionStore::for_workspace(&state.inner.options.workspace_root, "existing")
        .expect("store");
    store.save(&Session::new()).expect("save existing session");

    let result = create_session(
        State(state),
        Json(CreateSessionRequest {
            name: "existing".to_string(),
        }),
    )
    .await;

    assert!(matches!(
        result,
        Err(ApiError {
            status: StatusCode::CONFLICT,
            ..
        })
    ));

    match previous_home {
        Some(value) => unsafe {
            std::env::set_var("HOME", value);
        },
        None => unsafe {
            std::env::remove_var("HOME");
        },
    }
    drop(lock);
}

#[tokio::test]
async fn session_collection_is_versioned_and_legacy_projection_routes_are_absent() {
    let _lock = ENV_LOCK.get_or_init(|| AsyncMutex::new(())).lock().await;
    let home = unique_test_dir("collection-home");
    let _home = HomeGuard::set(&home);
    let router = router(test_options()).expect("router");

    let empty = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/sessions")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("list response");
    assert_eq!(empty.status(), StatusCode::OK);
    let body = axum::body::to_bytes(empty.into_body(), 1024 * 1024)
        .await
        .expect("directory body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("directory json");
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["sessions"], json!([]));
    assert_eq!(body["diagnostics"], json!([]));

    let created = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/sessions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"task_one"}"#))
                .expect("request"),
        )
        .await
        .expect("create response");
    assert_eq!(created.status(), StatusCode::CREATED);

    for method in [Method::GET, Method::POST] {
        let legacy = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/api/sessions/task_one")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("legacy response");
        assert_eq!(legacy.status(), StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn create_session_rejects_invalid_and_archived_names() {
    let _lock = ENV_LOCK.get_or_init(|| AsyncMutex::new(())).lock().await;
    let home = unique_test_dir("create-validation-home");
    let _home = HomeGuard::set(&home);
    let state = test_state();

    for name in ["bad name", " padded "] {
        let invalid = create_session(
            State(state.clone()),
            Json(CreateSessionRequest {
                name: name.to_string(),
            }),
        )
        .await;
        assert!(matches!(
            invalid,
            Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                ..
            })
        ));
    }

    let store = SessionStore::for_workspace(&state.inner.options.workspace_root, "archived")
        .expect("store");
    store.save(&Session::new()).expect("save session");
    store.archive().expect("archive session");
    let archived = create_session(
        State(state),
        Json(CreateSessionRequest {
            name: "archived".to_string(),
        }),
    )
    .await;
    assert!(matches!(
        archived,
        Err(ApiError {
            status: StatusCode::CONFLICT,
            ..
        })
    ));
}

#[tokio::test]
async fn missing_subscription_and_lifecycle_calls_do_not_create_session_files() {
    let _lock = ENV_LOCK.get_or_init(|| AsyncMutex::new(())).lock().await;
    let home = unique_test_dir("missing-session-home");
    let _home = HomeGuard::set(&home);
    let state = test_state();
    let store =
        SessionStore::for_workspace(&state.inner.options.workspace_root, "missing").expect("store");
    let lock_path = store.path().with_extension("lock");

    assert!(state.subscribe_session("missing").await.is_err());
    assert!(matches!(
        reset_session(State(state.clone()), Path("missing".to_string())).await,
        Err(ApiError {
            status: StatusCode::NOT_FOUND,
            ..
        })
    ));
    assert!(matches!(
        archive_session(State(state.clone()), Path("missing".to_string())).await,
        Err(ApiError {
            status: StatusCode::NOT_FOUND,
            ..
        })
    ));
    assert!(matches!(
        get_session_model_selection(State(state), Path("missing".to_string())).await,
        Err(ApiError {
            status: StatusCode::NOT_FOUND,
            ..
        })
    ));
    assert!(!store.path().exists());
    assert!(!lock_path.exists());
}

#[tokio::test]
async fn corrupt_session_is_reported_without_blocking_other_sessions() {
    let _lock = ENV_LOCK.get_or_init(|| AsyncMutex::new(())).lock().await;
    let home = unique_test_dir("corrupt-directory-home");
    let _home = HomeGuard::set(&home);
    let state = test_state();
    let corrupt =
        SessionStore::for_workspace(&state.inner.options.workspace_root, "broken").expect("store");
    corrupt.reset().expect("create log");
    fs::write(corrupt.path(), b"{broken\n").expect("corrupt log");

    let directory = list_sessions(State(state.clone()))
        .await
        .expect("list sessions")
        .0;
    assert!(directory.sessions.is_empty());
    assert_eq!(directory.diagnostics.len(), 1);
    assert_eq!(directory.diagnostics[0].name.as_deref(), Some("broken"));
    assert!(state.subscribe_session("broken").await.is_err());

    let created = create_session(
        State(state.clone()),
        Json(CreateSessionRequest {
            name: "healthy".to_string(),
        }),
    )
    .await
    .expect("create healthy session");
    assert_eq!(created.0, StatusCode::CREATED);
    let directory = list_sessions(State(state)).await.expect("list sessions").0;
    assert!(
        directory
            .sessions
            .iter()
            .any(|entry| entry.name == "healthy")
    );
    assert_eq!(directory.diagnostics.len(), 1);
}

#[tokio::test]
async fn static_application_assets_are_never_cached() {
    let router = router(test_options()).expect("router");
    for path in ["/", "/app.js", "/style.css"] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("asset response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL),
            Some(&HeaderValue::from_static("no-store"))
        );
    }
}

#[tokio::test]
async fn archive_and_restore_session_updates_session_listing() {
    let lock = ENV_LOCK.get_or_init(|| AsyncMutex::new(())).lock().await;
    let home = unique_test_dir("archive-home");
    let previous_home = std::env::var_os("HOME");
    unsafe {
        std::env::set_var("HOME", &home);
    }

    let state = test_state();
    let store =
        SessionStore::for_workspace(&state.inner.options.workspace_root, "work").expect("store");
    store.save(&Session::new()).expect("save session");
    let mut subscription = state
        .subscribe_session("work")
        .await
        .expect("subscribe before archive");
    let snapshot_stream_id = match &subscription.snapshot {
        SessionStreamFrame::Snapshot(snapshot) => snapshot.cursor.stream_id.clone(),
        _ => panic!("expected snapshot"),
    };
    let exported = export_session(State(state.clone()), Path("work".to_string()))
        .await
        .expect("export subscribed session");
    assert_eq!(
        exported.headers().get(header::CONTENT_TYPE),
        Some(&HeaderValue::from_static("application/x-ndjson")),
    );
    let exported = axum::body::to_bytes(exported.into_body(), 1024 * 1024)
        .await
        .expect("read export");
    let header: agent_protocol::SessionLogHeader = serde_json::from_slice(
        exported
            .split(|byte| *byte == b'\n')
            .next()
            .expect("export header"),
    )
    .expect("parse export header");
    assert_eq!(
        header.schema_version,
        agent_protocol::SESSION_DOCUMENT_SCHEMA_VERSION
    );

    let archived = archive_session(State(state.clone()), Path("work".to_string()))
        .await
        .expect("archive session");
    let invalidation = subscription.recv().await.expect("archive invalidation");
    let SessionStreamFrame::Event(invalidation) = invalidation else {
        panic!("expected invalidation event");
    };
    let entries = list_sessions(State(state.clone()))
        .await
        .expect("list sessions");

    assert!(archived.0.archived);
    assert_ne!(invalidation.stream_id, snapshot_stream_id);
    assert!(store.is_archived());
    assert!(state.subscribe_session("work").await.is_err());
    assert!(matches!(
        get_session_model_selection(State(state.clone()), Path("work".to_string())).await,
        Err(ApiError {
            status: StatusCode::CONFLICT,
            ..
        })
    ));
    assert!(
        entries
            .0
            .sessions
            .iter()
            .any(|entry| entry.name == "work" && entry.archived)
    );

    let restored = restore_session(State(state), Path("work".to_string()))
        .await
        .expect("restore session");

    assert!(!restored.0.archived);
    assert!(!store.is_archived());
    assert!(store.load_existing().is_ok());

    match previous_home {
        Some(value) => unsafe {
            std::env::set_var("HOME", value);
        },
        None => unsafe {
            std::env::remove_var("HOME");
        },
    }
    drop(lock);
}

#[test]
fn start_turn_message_accepts_optional_permission_mode() {
    let selected = serde_json::from_value::<ClientMessage>(json!({
        "type": "start_turn",
        "data": {
            "request_id": "request-1",
            "prompt": "edit the workspace",
            "prompt_resolved": true,
            "permission_mode": "workspace_write",
            "model_selection": {
                "provider_id": "deepseek",
                "model_id": "deepseek-v4-pro",
                "reasoning": "max"
            }
        }
    }))
    .expect("parse selected permissions");
    let legacy = serde_json::from_value::<ClientMessage>(json!({
        "type": "start_turn",
        "data": {
            "request_id": "request-2",
            "prompt": "inspect the workspace"
        }
    }))
    .expect("parse legacy message");

    assert!(matches!(
        selected,
        ClientMessage::StartTurn {
            prompt_resolved: true,
            permission_mode: Some(PermissionMode::WorkspaceWrite),
            model_selection: Some(ModelSelection {
                reasoning: agent_protocol::ReasoningLevel::Max,
                ..
            }),
            ..
        }
    ));
    assert!(matches!(
        legacy,
        ClientMessage::StartTurn {
            prompt_resolved: false,
            permission_mode: None,
            ..
        }
    ));
}

#[test]
fn requested_permissions_clamp_requested_mode_to_the_ceiling() {
    let read_only = PermissionProfile {
        mode: PermissionMode::ReadOnly,
        shell: ShellPolicy::Deny,
    };

    assert_eq!(
        requested_permissions(
            read_only,
            Some(PermissionMode::WorkspaceWrite),
            PermissionMode::DangerFullAccess
        ),
        PermissionProfile::for_mode(PermissionMode::WorkspaceWrite)
    );
    assert_eq!(
        requested_permissions(
            read_only,
            Some(PermissionMode::DangerFullAccess),
            PermissionMode::DangerFullAccess
        ),
        PermissionProfile::for_mode(PermissionMode::DangerFullAccess)
    );
    assert_eq!(
        requested_permissions(
            read_only,
            Some(PermissionMode::DangerFullAccess),
            PermissionMode::WorkspaceWrite
        ),
        PermissionProfile::for_mode(PermissionMode::WorkspaceWrite)
    );
    assert_eq!(
        requested_permissions(
            read_only,
            Some(PermissionMode::WorkspaceWrite),
            PermissionMode::ReadOnly
        ),
        PermissionProfile::for_mode(PermissionMode::ReadOnly)
    );
    assert_eq!(
        requested_permissions(read_only, None, PermissionMode::DangerFullAccess),
        read_only
    );
    // The ceiling also caps the server-side default profile.
    let workspace_write = PermissionProfile::for_mode(PermissionMode::WorkspaceWrite);
    assert_eq!(
        requested_permissions(workspace_write, None, PermissionMode::ReadOnly).mode,
        PermissionMode::ReadOnly
    );
    assert_eq!(DEFAULT_WEB_PERMISSION_MODE, PermissionMode::WorkspaceWrite);
}

#[tokio::test]
async fn wrong_approval_request_id_is_rejected() {
    let state = test_state();
    let tx = session_sender(&state, "default").await;
    let mut rx = tx.subscribe();
    let worker = tokio::spawn(std::future::pending::<()>());
    {
        let (sender, _receiver) = oneshot::channel();
        let mut sessions = state.inner.sessions.lock().await;
        let runtime = sessions
            .entry("default".to_string())
            .or_insert_with(SessionRuntime::new);
        runtime.running = Some(RunningTurn {
            turn_id: "turn-1".to_string(),
            cancellation: CancellationToken::new(),
            handle: worker.abort_handle(),
        });
        runtime.approvals.push_back(PendingApproval {
            request: ApprovalRequest::shell_command("approval-call_1", "pwd", ".", 30, "test"),
            sender,
        });
    }

    resolve_approval(&state, "default", "approval-wrong".to_string(), true, &tx).await;

    let message = rx.recv().await.expect("error message");
    assert!(matches!(message, ServerMessage::Error { .. }));
    assert!(
        running_snapshot(&state, "default")
            .await
            .expect("running")
            .pending_approval
            .is_some()
    );
    clear_running_turn(&state, "default", "turn-1").await;
    worker.abort();
    let _ = worker.await;
}

#[tokio::test]
async fn approval_queue_is_fifo_across_parent_and_subagent_sources() {
    let _lock = ENV_LOCK.get_or_init(|| AsyncMutex::new(())).lock().await;
    let home = unique_test_dir("approval-queue-home");
    let _home = HomeGuard::set(&home);
    let state = test_state();
    SessionStore::for_workspace(&state.inner.options.workspace_root, "default")
        .expect("store")
        .reset()
        .expect("create session");
    let tx = session_sender(&state, "default").await;
    let mut rx = tx.subscribe();
    let parent = ApprovalRequest::shell_command(
        "approval-parent",
        "cargo test",
        ".",
        30,
        "verify parent changes",
    )
    .with_origin(ApprovalOrigin::ParentTurn {
        turn_id: Some("turn-1".to_string()),
        tool_call_id: Some("parent-call".to_string()),
    });
    let child = ApprovalRequest::shell_command(
        "approval-child",
        "cargo clippy",
        ".",
        30,
        "verify child changes",
    )
    .with_origin(ApprovalOrigin::SubagentRun {
        instance_id: "subagent-1".to_string(),
        run_id: "subrun-1".to_string(),
        role: SubagentRole::Reviewer,
        identity_id: Some("builtin-01".to_string()),
        identity_name: Some("Reviewer".to_string()),
        tool_call_id: Some("child-call".to_string()),
    });

    let first = tokio::spawn({
        let state = state.clone();
        let tx = tx.clone();
        async move { enqueue_approval(&state, "default", parent, &tx).await }
    });
    wait_for_approval_count(&state, "default", 1).await;
    let second = tokio::spawn({
        let state = state.clone();
        let tx = tx.clone();
        async move { enqueue_approval(&state, "default", child, &tx).await }
    });
    wait_for_approval_count(&state, "default", 2).await;

    let resources = ensure_session_resources(&state, "default")
        .await
        .expect("session resources");
    let approvals = resources.handle.snapshot().await.approvals;
    assert_eq!(
        approvals
            .iter()
            .map(|request| request.id.as_str())
            .collect::<Vec<_>>(),
        vec!["approval-parent", "approval-child"]
    );
    assert!(matches!(
        approvals[1].origin,
        ApprovalOrigin::SubagentRun { .. }
    ));

    resolve_approval(&state, "default", "approval-child".to_string(), true, &tx).await;
    let queued_error = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let ServerMessage::Error { message } = rx.recv().await.expect("queue event")
                && message.contains("queued behind")
            {
                break message;
            }
        }
    })
    .await
    .expect("queued decision rejected");
    assert!(queued_error.contains("approval-parent"));

    resolve_approval(&state, "default", "approval-parent".to_string(), true, &tx).await;
    assert!(
        first
            .await
            .expect("first task")
            .expect("first decision")
            .approved
    );
    resolve_approval(&state, "default", "approval-child".to_string(), false, &tx).await;
    assert!(
        !second
            .await
            .expect("second task")
            .expect("second decision")
            .approved
    );
    assert!(approval_snapshots(&state, "default").await.is_empty());
}

#[tokio::test]
async fn cancelling_a_subagent_run_denies_only_its_queued_approvals() {
    let state = test_state();
    let tx = session_sender(&state, "default").await;
    let child =
        ApprovalRequest::shell_command("approval-child-cancel", "pwd", ".", 30, "child request")
            .with_origin(ApprovalOrigin::SubagentRun {
                instance_id: "subagent-cancel".to_string(),
                run_id: "subrun-cancel".to_string(),
                role: SubagentRole::Worker,
                identity_id: None,
                identity_name: None,
                tool_call_id: None,
            });
    let pending = tokio::spawn({
        let state = state.clone();
        let tx = tx.clone();
        async move { enqueue_approval(&state, "default", child, &tx).await }
    });
    wait_for_approval_count(&state, "default", 1).await;

    cancel_matching_approvals(&state, "default", &tx, |request| {
        matches!(
            &request.origin,
            ApprovalOrigin::SubagentRun { instance_id, run_id, .. }
                if instance_id == "subagent-cancel" && run_id == "subrun-cancel"
        )
    })
    .await;

    let decision = pending
        .await
        .expect("approval task")
        .expect("cancel decision");
    assert!(!decision.approved);
    assert_eq!(decision.request_id, "approval-child-cancel");
    assert!(approval_snapshots(&state, "default").await.is_empty());
}

async fn wait_for_approval_count(state: &AppState, session: &str, expected: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if approval_snapshots(state, session).await.len() == expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("approval queue reached expected length");
}
