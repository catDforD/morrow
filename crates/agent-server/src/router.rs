use super::*;
use axum::http::uri::{Authority, Uri};

pub const DEFAULT_WEB_PERMISSION_MODE: PermissionMode = PermissionMode::WorkspaceWrite;
#[derive(Clone)]
pub enum ServerAccessPolicy {
    /// Web dashboard access. `Some(token)` requires the bootstrap/cookie flow;
    /// `None` disables authentication (only via explicit `--no-auth`).
    Browser { token: Option<String> },
}

impl Default for ServerAccessPolicy {
    fn default() -> Self {
        Self::Browser { token: None }
    }
}

impl ServerAccessPolicy {
    pub fn browser(token: Option<String>) -> Self {
        Self::Browser { token }
    }
}

impl std::fmt::Debug for ServerAccessPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Browser { token } => formatter
                .debug_struct("Browser")
                .field("token", &token.as_ref().map(|_| "<redacted>"))
                .finish(),
        }
    }
}

/// Generate a cryptographically random browser session token.
pub fn generate_browser_token() -> Result<String, ServerError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| ServerError::Random(error.to_string()))?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut token, "{byte:02x}").expect("writing to a string cannot fail");
    }
    Ok(token)
}

pub(crate) fn build_router(
    options: ServerOptions,
    access_policy: ServerAccessPolicy,
) -> Result<(Router, AppState), ModelRegistryError> {
    let model_registry = ModelRegistry::load(
        options.model_store_path.clone(),
        &options.workspace_root,
        options.fallback_model.clone(),
    )?;
    let mcp_registry =
        McpRegistry::load(options.mcp_store_path.clone(), options.mcp_servers.clone())
            .map_err(|error| ModelRegistryError::Validation(error.to_string()))?;
    let command_registry = CommandRegistry::new(options.command_store_path.clone());
    let hook_manager = HookManager::new(
        options.hook_home_dir.clone(),
        options.workspace_root.clone(),
    );
    let subagent_registry = SubagentRegistry::load(options.subagent_store_path.clone())
        .map_err(|error| ModelRegistryError::Validation(error.to_string()))?;
    let state = AppState {
        inner: Arc::new(ServerState {
            options,
            model_registry,
            mcp_registry,
            command_registry,
            hook_manager,
            subagent_registry,
            sessions: Mutex::new(HashMap::new()),
            mcp_cache: RwLock::new(Arc::new(McpToolCache::new())),
            access_policy,
            shutting_down: AtomicBool::new(false),
        }),
    };

    let mut router = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/assets/{*path}", get(asset))
        .route("/api/status", get(status))
        .route("/api/hooks", get(hook_settings))
        .route("/api/hooks/trust", post(trust_project_hooks))
        .route("/api/hooks/revoke", post(revoke_project_hooks))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{name}/reset", post(reset_session))
        .route("/api/sessions/{name}/archive", post(archive_session))
        .route("/api/sessions/{name}/restore", post(restore_session))
        .route("/api/sessions/{name}/export", get(export_session))
        .route("/api/sessions/{name}/ws", get(session_ws));
    router = router
        .route("/api/model-settings", get(model_settings))
        .route("/api/model-providers", post(create_model_provider))
        .route(
            "/api/model-providers/{provider_id}",
            put(update_model_provider).delete(delete_model_provider),
        )
        .route(
            "/api/model-providers/discover",
            post(discover_model_provider),
        )
        .route("/api/model-default", put(set_default_model))
        .route("/api/mcp-settings", get(mcp_settings))
        .route("/api/mcp-servers", post(create_mcp_server))
        .route("/api/mcp-servers/import", post(import_mcp_servers))
        .route("/api/mcp-servers/test", post(test_mcp_server))
        .route(
            "/api/mcp-servers/{name}",
            put(update_mcp_server).delete(delete_mcp_server),
        )
        .route("/api/commands", get(command_settings).post(create_command))
        .route("/api/commands/resolve", post(resolve_command))
        .route(
            "/api/commands/{name}",
            put(update_command).delete(delete_command),
        )
        .route("/api/subagent-settings", get(subagent_settings))
        .route(
            "/api/subagent-settings/roles/{role}",
            put(update_subagent_role),
        )
        .route(
            "/api/subagent-settings/roles/reset",
            post(reset_subagent_roles),
        )
        .route("/api/subagents", post(create_subagent))
        .route(
            "/api/subagents/{id}",
            put(update_subagent).delete(delete_subagent),
        )
        .route("/api/subagent-settings/reset", post(reset_subagents))
        .route(
            "/api/sessions/{name}/model-selection",
            get(get_session_model_selection).put(set_session_model_selection),
        );
    let router = router
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            access_middleware,
        ));
    Ok((router, state))
}

async fn access_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let token: &str = match &state.inner.access_policy {
        ServerAccessPolicy::Browser { token: Some(token) } => token.as_str(),
        ServerAccessPolicy::Browser { token: None } => {
            return with_security_headers(next.run(request).await);
        }
    };
    let expected_authority = ExpectedAuthority {
        host: state.inner.options.host,
        port: state.inner.options.port,
    };
    let response = match token_guard(&request, token, expected_authority) {
        Some(rejection) => rejection,
        None => next.run(request).await,
    };

    with_security_headers(response)
}

/// Shared Host/bootstrap/cookie/Origin guard for token-protected access policies.
/// Returns `Some(response)` when the request is handled (bootstrap redirect or
/// rejection) and `None` when it may proceed.
#[derive(Clone, Copy)]
struct ExpectedAuthority {
    host: IpAddr,
    port: u16,
}

impl ExpectedAuthority {
    fn matches(&self, authority: &Authority) -> bool {
        if authority.as_str().contains('@') {
            return false;
        }
        let host = authority.host();
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        host.parse::<IpAddr>().ok() == Some(self.host)
            && authority.port_u16().unwrap_or(80) == self.port
    }

    fn matches_host_header(&self, value: &str) -> bool {
        value
            .parse::<Authority>()
            .ok()
            .is_some_and(|authority| self.matches(&authority))
    }

    fn matches_origin(&self, value: &str) -> bool {
        let Ok(origin) = value.parse::<Uri>() else {
            return false;
        };
        origin.scheme_str() == Some("http")
            && matches!(origin.path(), "" | "/")
            && origin.query().is_none()
            && origin
                .authority()
                .is_some_and(|authority| self.matches(authority))
    }
}

fn token_guard(
    request: &Request<Body>,
    token: &str,
    expected: ExpectedAuthority,
) -> Option<Response> {
    let host_matches = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| expected.matches_host_header(host));
    if !host_matches {
        return Some(StatusCode::UNAUTHORIZED.into_response());
    }
    if is_bootstrap_request(request, token) {
        let mut response = StatusCode::SEE_OTHER.into_response();
        response
            .headers_mut()
            .insert(header::LOCATION, HeaderValue::from_static("/"));
        let cookie = format!("morrow_session={token}; HttpOnly; SameSite=Strict; Path=/");
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            response.headers_mut().insert(header::SET_COOKIE, value);
        }
        return Some(response);
    }
    if !has_session_cookie(request, token) || !origin_is_allowed(request, expected) {
        return Some(StatusCode::UNAUTHORIZED.into_response());
    }
    None
}

fn is_bootstrap_request(request: &Request<Body>, token: &str) -> bool {
    request.method() == Method::GET
        && request.uri().path() == "/"
        && request
            .uri()
            .query()
            .and_then(|query| {
                query.split('&').find_map(|pair| {
                    pair.strip_prefix("bootstrap=")
                        .filter(|value| !value.contains('='))
                })
            })
            .is_some_and(|provided| constant_time_eq(provided.as_bytes(), token.as_bytes()))
}

fn has_session_cookie(request: &Request<Body>, token: &str) -> bool {
    request
        .headers()
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|cookies| {
            cookies.split(';').any(|cookie| {
                cookie
                    .trim()
                    .strip_prefix("morrow_session=")
                    .is_some_and(|provided| constant_time_eq(provided.as_bytes(), token.as_bytes()))
            })
        })
}

fn origin_is_allowed(request: &Request<Body>, expected: ExpectedAuthority) -> bool {
    let requires_origin = request.uri().path().ends_with("/ws")
        || !matches!(*request.method(), Method::GET | Method::HEAD);
    if !requires_origin {
        return true;
    }
    request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| expected.matches_origin(origin))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn with_security_headers(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self' ws: wss:; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
        .headers_mut()
        .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

pub(crate) async fn index() -> Response {
    no_store(Html(include_str!("../assets/index.html")).into_response())
}

pub(crate) async fn app_js() -> Response {
    asset_response("app.js")
}

pub(crate) async fn style_css() -> Response {
    asset_response("style.css")
}

pub(crate) async fn asset(Path(path): Path<String>) -> Response {
    asset_response(&path)
}

fn asset_response(path: &str) -> Response {
    let response = match path {
        "app.js" => (
            [(
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            )],
            include_str!("../assets/app.js"),
        )
            .into_response(),
        "style.css" => (
            [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
            include_str!("../assets/style.css"),
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    };
    no_store(response)
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
