use super::*;
use agent_core::GateDecision;
use agent_model::{OpenAiCompatClient, OpenAiCompatConfig};
use agent_protocol::{FileChangeOperation, ReasoningLevel, SessionContext, Thread, Turn};
use futures_util::future::BoxFuture;
use serde_json::json;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn test_model_invocation() -> &'static ModelInvocation {
    static MODEL: OnceLock<ModelInvocation> = OnceLock::new();
    MODEL.get_or_init(|| ModelInvocation {
        provider_id: "test-provider".to_string(),
        provider_name: "Test Provider".to_string(),
        model_id: "test-model".to_string(),
        model_name: "Test Model".to_string(),
        reasoning: ReasoningLevel::Off,
    })
}

async fn spawn_recording_sse_server(
    bodies: Vec<&'static str>,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let addr = listener.local_addr().expect("server addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    tokio::spawn(async move {
        for body in bodies {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = vec![0_u8; 8192];
            let read = socket.read(&mut request).await.expect("read request");
            captured_requests
                .lock()
                .expect("requests lock poisoned")
                .push(String::from_utf8_lossy(&request[..read]).to_string());
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        }
    });
    (format!("http://{addr}/v1"), requests)
}

fn client(base_url: String) -> OpenAiCompatClient {
    OpenAiCompatClient::new_without_proxy(OpenAiCompatConfig {
        base_url,
        model: "test-model".to_string(),
        api_key: "test-key".to_string(),
        timeout: Duration::from_secs(5),
        max_retries: 1,
    })
    .expect("client")
}

fn sse_text_body(text: &str) -> &'static str {
    let body = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "choices": [{
                "delta": {"content": text},
                "finish_reason": null
            }]
        })
    );
    Box::leak(body.into_boxed_str())
}

fn tool_call_body(id: &str, name: &str, arguments: serde_json::Value) -> &'static str {
    let body = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments.to_string()
                        }
                    }]
                },
                "finish_reason": null
            }]
        }),
        json!({
            "choices": [{
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        })
    );
    Box::leak(body.into_boxed_str())
}

fn unique_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("morrow-runtime-{name}-{stamp}"));
    fs::create_dir_all(&path).expect("create root");
    path
}

#[test]
fn workspace_instructions_append_agents_md_to_the_base_prompt() {
    let root = unique_dir("agents-valid");
    let path = root.join(AGENTS_MD_FILE_NAME);
    fs::write(&path, "\nUse the repository test commands.\n").expect("write AGENTS.md");

    let loaded = load_workspace_instructions(&root, "base system prompt\n");
    assert!(loaded.diagnostics.is_empty());
    assert_eq!(
        loaded.effective_system_prompt,
        format!(
            "base system prompt\n\n{PROJECT_INSTRUCTIONS_PREFIX}\nUse the repository test commands.\n</project_instructions>"
        )
    );

    fs::remove_dir_all(root).expect("remove root");
}

fn bump_mtime(path: &Path) {
    // 显式推进 mtime，避免依赖文件系统的时间粒度。
    fs::File::open(path)
        .expect("open AGENTS.md")
        .set_modified(SystemTime::now() + Duration::from_secs(5))
        .expect("bump mtime");
}

#[test]
fn workspace_instructions_cache_reloads_agents_md_after_mtime_change() {
    let root = unique_dir("agents-reload");
    let path = root.join(AGENTS_MD_FILE_NAME);
    fs::write(&path, "Use the repository test commands.").expect("write AGENTS.md");

    let cache = WorkspaceInstructionsCache::new(&root);
    assert!(cache.prewarm().is_empty());
    assert_eq!(
        cache.apply("base system prompt\n"),
        format!(
            "base system prompt\n\n{PROJECT_INSTRUCTIONS_PREFIX}\nUse the repository test commands.\n</project_instructions>"
        )
    );

    // mtime 变化后下一轮读取生效（turn 语义由 per-turn 调用方保证）。
    fs::write(&path, "Use the updated commands.").expect("update AGENTS.md");
    bump_mtime(&path);
    let reloaded = cache.apply("base system prompt");
    assert!(reloaded.contains("updated"));
    assert!(!reloaded.contains("repository test commands"));

    // 文件删除后段落消失。
    fs::remove_file(&path).expect("remove AGENTS.md");
    assert_eq!(cache.apply("base system prompt"), "base system prompt");

    fs::remove_dir_all(root).expect("remove root");
}

#[test]
fn workspace_instructions_cache_hits_without_rereading_when_mtime_is_unchanged() {
    let root = unique_dir("agents-cache-hit");
    let path = root.join(AGENTS_MD_FILE_NAME);
    fs::write(&path, "cached instructions").expect("write AGENTS.md");

    let cache = WorkspaceInstructionsCache::new(&root);
    assert!(cache.section().contains("cached instructions"));

    // 改写内容但把 mtime 拨回原值：缓存命中时必须返回旧段落，证明没有重读。
    let mtime = fs::symlink_metadata(&path)
        .expect("metadata")
        .modified()
        .expect("mtime");
    fs::write(&path, "rewritten instructions").expect("rewrite AGENTS.md");
    fs::File::open(&path)
        .expect("open AGENTS.md")
        .set_modified(mtime)
        .expect("restore mtime");
    assert!(cache.section().contains("cached instructions"));

    fs::remove_dir_all(root).expect("remove root");
}

#[test]
fn utc_date_string_formats_civil_dates() {
    assert_eq!(utc_date_string(UNIX_EPOCH), "1970-01-01");
    assert_eq!(
        utc_date_string(UNIX_EPOCH + Duration::from_secs(1_735_689_600)),
        "2025-01-01"
    );
    assert_eq!(
        utc_date_string(UNIX_EPOCH + Duration::from_secs(1_709_251_200)),
        "2024-03-01"
    );
    // 2000 是 400 整除的闰年。
    assert_eq!(
        utc_date_string(UNIX_EPOCH + Duration::from_secs(951_782_400)),
        "2000-02-29"
    );
    // 2100 不是闰年。
    assert_eq!(
        utc_date_string(UNIX_EPOCH + Duration::from_secs(4_102_444_800)),
        "2100-01-01"
    );
}

#[tokio::test]
async fn environment_context_block_lists_workspace_platform_and_date_without_git() {
    let root = unique_dir("environment-block");
    let block = environment_context_block(&root).await;

    assert!(block.starts_with("<environment>\n"));
    assert!(block.ends_with("\n</environment>"));
    assert!(block.contains(&format!("workspace_root: {}", root.display())));
    assert!(block.contains(&format!("os: {}", std::env::consts::OS)));
    assert!(block.contains(&format!("arch: {}", std::env::consts::ARCH)));
    // YYYY-MM-DD
    let date = block
        .lines()
        .find_map(|line| line.strip_prefix("date: "))
        .expect("date line");
    assert_eq!(date.len(), 10);
    assert_eq!(date.as_bytes()[4], b'-');
    assert_eq!(date.as_bytes()[7], b'-');
    assert!(date.chars().all(|c| c.is_ascii_digit() || c == '-'));
    // 临时目录不是 git repo：静默省略分支行。
    assert!(!block.contains("git_branch"));

    fs::remove_dir_all(root).expect("remove root");
}

#[tokio::test]
async fn environment_context_block_includes_git_branch_in_a_repository() {
    let root = unique_dir("environment-git");
    let init = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["init", "-b", "main"])
        .output()
        .expect("run git init");
    if !init.status.success() {
        // 测试环境没有 git 时跳过分支断言。
        fs::remove_dir_all(root).expect("remove root");
        return;
    }
    let commit = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args([
            "-c",
            "user.name=morrow-test",
            "-c",
            "user.email=morrow-test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ])
        .output()
        .expect("run git commit");
    assert!(commit.status.success());

    assert_eq!(current_git_branch(&root).await.as_deref(), Some("main"));
    let block = environment_context_block(&root).await;
    assert!(block.contains("git_branch: main"));

    fs::remove_dir_all(root).expect("remove root");
}

#[tokio::test]
async fn assembled_turn_system_prompt_orders_base_instructions_environment_then_guidance() {
    let root = unique_dir("assembled-order");
    fs::write(root.join(AGENTS_MD_FILE_NAME), "project rule").expect("write AGENTS.md");
    let cache = WorkspaceInstructionsCache::new(&root);
    let client = client("http://127.0.0.1:1/v1".to_string());
    let mcp_cache = McpToolCache::new();
    let context = RunAgentTurnContext {
        client: &client,
        model: test_model_invocation(),
        subagent_identities: &[],
        system_prompt: "config base",
        context_config: context_config(2),
        model_limits: model_limits(10_000),
        workspace_root: &root,
        workspace_instructions: Some(&cache),
        permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
        mcp_servers: &[],
        mcp_cache: &mcp_cache,
        tools: None,
        auto_approve_workspace_writes: true,
        session_name: "default",
        turn_index: 0,
    };

    let turn_base = assembled_turn_system_prompt(context).await;
    let full = effective_turn_system_prompt(&turn_base, true, false);

    let base_at = full.find("config base").expect("base");
    let instructions_at = full.find("<project_instructions>").expect("instructions");
    let environment_at = full.find("<environment>").expect("environment");
    let guidance_at = full.find(PARENT_SUBAGENT_GUIDANCE).expect("guidance");
    assert!(base_at < instructions_at);
    assert!(instructions_at < environment_at);
    assert!(environment_at < guidance_at);

    fs::remove_dir_all(root).expect("remove root");
}

#[test]
fn workspace_instructions_ignore_missing_empty_and_nested_agents_md() {
    let root = unique_dir("agents-noop");
    let nested = root.join("nested");
    fs::create_dir_all(&nested).expect("create nested root");
    fs::write(nested.join(AGENTS_MD_FILE_NAME), "nested rule").expect("write nested rules");

    let missing = load_workspace_instructions(&root, "base");
    assert_eq!(missing.effective_system_prompt, "base");
    assert!(missing.diagnostics.is_empty());

    fs::write(root.join(AGENTS_MD_FILE_NAME), " \n\t").expect("write empty rules");
    let empty = load_workspace_instructions(&root, "base");
    assert_eq!(empty.effective_system_prompt, "base");
    assert!(empty.diagnostics.is_empty());

    fs::remove_dir_all(root).expect("remove root");
}

#[test]
fn workspace_instructions_warn_and_keep_base_for_invalid_files() {
    let root = unique_dir("agents-invalid");
    let path = root.join(AGENTS_MD_FILE_NAME);

    fs::write(&path, [0xff, 0xfe]).expect("write invalid UTF-8");
    let invalid = load_workspace_instructions(&root, "base");
    assert_eq!(invalid.effective_system_prompt, "base");
    assert_eq!(invalid.diagnostics.len(), 1);
    assert!(invalid.diagnostics[0].contains("not valid UTF-8"));

    fs::write(&path, vec![b'x'; MAX_AGENTS_MD_BYTES as usize + 1]).expect("write oversized rules");
    let oversized = load_workspace_instructions(&root, "base");
    assert_eq!(oversized.effective_system_prompt, "base");
    assert_eq!(oversized.diagnostics.len(), 1);
    assert!(oversized.diagnostics[0].contains("exceeds the 32768-byte limit"));

    fs::remove_file(&path).expect("remove oversized rules");
    fs::create_dir(&path).expect("create AGENTS.md directory");
    let directory = load_workspace_instructions(&root, "base");
    assert_eq!(directory.effective_system_prompt, "base");
    assert_eq!(directory.diagnostics.len(), 1);
    assert!(directory.diagnostics[0].contains("must be a regular file"));

    fs::remove_dir_all(root).expect("remove root");
}

#[cfg(unix)]
#[test]
fn workspace_instructions_do_not_follow_symbolic_links() {
    let root = unique_dir("agents-symlink");
    let target = root.join("shared-instructions.md");
    fs::write(&target, "shared rule").expect("write symlink target");
    std::os::unix::fs::symlink(&target, root.join(AGENTS_MD_FILE_NAME))
        .expect("create AGENTS.md symlink");

    let loaded = load_workspace_instructions(&root, "base");
    assert_eq!(loaded.effective_system_prompt, "base");
    assert_eq!(loaded.diagnostics.len(), 1);
    assert!(loaded.diagnostics[0].contains("symbolic links are not supported"));

    fs::remove_dir_all(root).expect("remove root");
}

fn context_config(retain_recent_turns: usize) -> ContextConfig {
    ContextConfig {
        auto_compact: true,
        auto_compact_threshold: 0.835,
        retain_recent_turns,
        summary_target_tokens: 256,
        compact_max_retries: 2,
        max_context_tokens: Some(300_000),
    }
}

fn model_limits(context_window_tokens: usize) -> ModelContextLimits {
    ModelContextLimits {
        context_window_tokens,
        reserved_output_tokens: 1,
    }
}

fn valid_compact_summary_text(current_progress: &str) -> String {
    format!(
        r#"User Goals and Constraints
- keep user intent

Important Decisions
- compact

Files and Code State
- none

Commands, Results, and Errors
- none

Current Progress
- {current_progress}

Pending Tasks
- none

Open Questions
- none"#
    )
}

fn valid_compact_summary(current_progress: &str) -> String {
    format!(
        r#"<analysis>
compact test
</analysis>
<summary>
{}
</summary>"#,
        valid_compact_summary_text(current_progress)
    )
}

fn completed_record(user: &str, assistant: &str) -> TurnRecord {
    let user_message = Message::user(user);
    let assistant_message = Message::assistant(assistant);
    let mut turn = Turn::running(user_message.clone());
    turn.complete(assistant_message.clone());
    TurnRecord::new(turn, vec![user_message, assistant_message])
}

fn compactable_session() -> Session {
    let turns = vec![
        completed_record("u0", "a0"),
        completed_record("u1", "a1"),
        TurnRecord::failed_user_prompt("broken", "failure reason"),
        completed_record("u3", "a3"),
        completed_record("u4", "a4"),
    ];
    let mut session = Session {
        active_thread: Thread::new(),
        turns,
        context: SessionContext::new(),
    };
    rebuild_active_thread(&mut session);
    session
}

#[derive(Default)]
struct RecordingHandler {
    events: Vec<AgentEventEnvelope>,
}

impl TurnEventHandler for RecordingHandler {
    fn on_event(&mut self, event: &AgentEventEnvelope) -> Result<(), RuntimeError> {
        self.events.push(event.clone());
        Ok(())
    }
}

struct PromptMiddleware {
    decision: GateDecision,
    context: Vec<agent_core::ContextBlock>,
}

impl RuntimeMiddleware for PromptMiddleware {
    fn id(&self) -> &str {
        "prompt-policy"
    }

    fn before_prompt(
        &self,
        _input: BeforePromptInput,
    ) -> Option<agent_core::MiddlewareFuture<agent_core::GateOutput>> {
        let output = agent_core::GateOutput {
            decision: self.decision.clone(),
            additional_context: self.context.clone(),
        };
        Some(async move { Ok(output) }.boxed())
    }
}

struct FailingPostCompactMiddleware;

impl RuntimeMiddleware for FailingPostCompactMiddleware {
    fn id(&self) -> &str {
        "post-compact-policy"
    }

    fn post_compact(
        &self,
        _input: PostCompactInput,
    ) -> Option<agent_core::MiddlewareFuture<agent_core::ObservationOutput>> {
        Some(async move { Err(agent_core::MiddlewareError::new("rejected compact draft")) }.boxed())
    }
}

struct ScopeRecordingMiddleware {
    scopes: Arc<Mutex<Vec<MiddlewareAgentScope>>>,
}

impl RuntimeMiddleware for ScopeRecordingMiddleware {
    fn id(&self) -> &str {
        "scope-recorder"
    }

    fn before_prompt(
        &self,
        input: BeforePromptInput,
    ) -> Option<agent_core::MiddlewareFuture<agent_core::GateOutput>> {
        self.scopes
            .lock()
            .expect("scope recorder")
            .push(input.context.agent_scope);
        Some(async move { Ok(agent_core::GateOutput::default()) }.boxed())
    }
}

struct FailOnAgentMessage;

impl TurnEventHandler for FailOnAgentMessage {
    fn on_event(&mut self, event: &AgentEventEnvelope) -> Result<(), RuntimeError> {
        if matches!(event.event, AgentEvent::AgentMessage(_)) {
            return Err(RuntimeError::event_handler("simulated output failure"));
        }
        Ok(())
    }
}

struct FailOnFirstEvent;

impl TurnEventHandler for FailOnFirstEvent {
    fn on_event(&mut self, _event: &AgentEventEnvelope) -> Result<(), RuntimeError> {
        Err(RuntimeError::event_handler(
            "simulated middleware delivery failure",
        ))
    }
}

struct FailOnTextDelta;

impl TurnEventHandler for FailOnTextDelta {
    fn on_event(&mut self, event: &AgentEventEnvelope) -> Result<(), RuntimeError> {
        if matches!(event.event, AgentEvent::TextDelta(_)) {
            return Err(RuntimeError::event_handler("simulated streaming failure"));
        }
        Ok(())
    }
}

struct PendingModel;

impl Model for PendingModel {
    fn stream(&self, _request: ModelRequest) -> agent_core::ModelFuture {
        async move {
            let stream: agent_core::ModelStream = Box::pin(futures_util::stream::pending::<
                Result<ModelEvent, ModelFailure>,
            >());
            Ok(stream)
        }
        .boxed()
    }
}

#[derive(Clone)]
struct ConstantModel {
    text: String,
}

impl Model for ConstantModel {
    fn stream(&self, _request: ModelRequest) -> agent_core::ModelFuture {
        let text = self.text.clone();
        async move {
            let stream: agent_core::ModelStream = futures_util::stream::iter(vec![
                Ok(ModelEvent::TextDelta(text)),
                Ok(ModelEvent::Completed),
            ])
            .boxed();
            Ok(stream)
        }
        .boxed()
    }
}

#[derive(Clone)]
struct GatedModel {
    started: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Barrier>,
}

impl Model for GatedModel {
    fn stream(&self, _request: ModelRequest) -> agent_core::ModelFuture {
        let started = Arc::clone(&self.started);
        let release = Arc::clone(&self.release);
        async move {
            started.fetch_add(1, Ordering::AcqRel);
            release.wait().await;
            let stream: agent_core::ModelStream = futures_util::stream::iter(vec![
                Ok(ModelEvent::TextDelta("done".to_string())),
                Ok(ModelEvent::Completed),
            ])
            .boxed();
            Ok(stream)
        }
        .boxed()
    }
}

struct CancelOnTurnStarted {
    cancellation: CancellationToken,
    events: Vec<AgentEventEnvelope>,
}

impl TurnEventHandler for CancelOnTurnStarted {
    fn on_event(&mut self, event: &AgentEventEnvelope) -> Result<(), RuntimeError> {
        self.events.push(event.clone());
        if matches!(event.event, AgentEvent::TurnStarted) {
            self.cancellation.cancel();
        }
        Ok(())
    }
}

struct ApprovalHandler {
    events: Vec<AgentEventEnvelope>,
    approved: bool,
}

impl TurnEventHandler for ApprovalHandler {
    fn on_event(&mut self, event: &AgentEventEnvelope) -> Result<(), RuntimeError> {
        self.events.push(event.clone());
        Ok(())
    }

    fn resolve_approval<'a>(
        &'a mut self,
        request: &'a ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, RuntimeError>> {
        async move {
            Ok(if self.approved {
                ApprovalDecision::approve(request.id.clone())
            } else {
                ApprovalDecision::deny(request.id.clone())
            })
        }
        .boxed()
    }
}

#[test]
fn event_envelope_uses_stable_schema_and_indices() {
    let root = unique_dir("envelope");
    let envelope = make_event_envelope("default", &root, 7, 3, AgentEvent::TurnStarted);

    assert_eq!(envelope.schema_version, EVENT_SCHEMA_VERSION);
    assert!(envelope.timestamp_ms > 0);
    assert_eq!(envelope.session, "default");
    assert_eq!(envelope.workspace_root, root.display().to_string());
    assert_eq!(envelope.turn_index, 7);
    assert_eq!(envelope.event_index, 3);
    assert_eq!(envelope.event, AgentEvent::TurnStarted);
}

#[tokio::test]
async fn subagent_executor_runs_four_tasks_concurrently_and_rejects_the_fifth() {
    let root = unique_dir("subagent-limit");
    let started = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Barrier::new(MAX_SUBAGENTS_PER_TURN + 1));
    let executor = RuntimeSubagentExecutor::new(
        Arc::new(GatedModel {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        }),
        Arc::<str>::from("system"),
        Arc::new(root),
    );
    let cancellation = CancellationToken::new();
    let futures = (0..MAX_SUBAGENTS_PER_TURN)
        .map(|index| executor.execute(format!("task {index}"), cancellation.clone()))
        .collect::<Vec<_>>();
    let join = tokio::spawn(async move { futures_util::future::join_all(futures).await });

    tokio::time::timeout(Duration::from_secs(1), async {
        while started.load(Ordering::Acquire) < MAX_SUBAGENTS_PER_TURN {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all subagents should start concurrently");
    release.wait().await;
    let summaries = join.await.expect("join subagents");
    assert!(summaries.iter().all(|summary| summary.error.is_none()));

    let rejected = executor
        .execute("fifth task".to_string(), cancellation)
        .await;
    assert_eq!(
        rejected.error.as_deref(),
        Some("subagent limit exceeded (4 per turn)")
    );
    assert_eq!(started.load(Ordering::Acquire), MAX_SUBAGENTS_PER_TURN);
}

#[tokio::test]
async fn subagent_timeout_does_not_cancel_the_parent_token() {
    let root = unique_dir("subagent-timeout");
    let mut executor = RuntimeSubagentExecutor::new(
        Arc::new(PendingModel),
        Arc::<str>::from("system"),
        Arc::new(root),
    );
    executor.timeout = Duration::from_millis(10);
    let parent_cancellation = CancellationToken::new();

    let summary = executor
        .execute("wait forever".to_string(), parent_cancellation.clone())
        .await;

    assert!(
        summary
            .error
            .as_deref()
            .is_some_and(|error| error.contains("timed out"))
    );
    assert!(!parent_cancellation.is_cancelled());
}

#[tokio::test]
async fn parent_cancellation_stops_a_running_subagent() {
    let root = unique_dir("subagent-cancel");
    let executor = RuntimeSubagentExecutor::new(
        Arc::new(PendingModel),
        Arc::<str>::from("system"),
        Arc::new(root),
    );
    let parent_cancellation = CancellationToken::new();
    let run = executor.execute("wait forever".to_string(), parent_cancellation.clone());
    let worker = tokio::spawn(run);

    tokio::task::yield_now().await;
    parent_cancellation.cancel();
    let summary = tokio::time::timeout(Duration::from_secs(1), worker)
        .await
        .expect("cancelled subagent should stop")
        .expect("subagent worker");

    assert_eq!(
        summary.error.as_deref(),
        Some("subagent execution cancelled")
    );
}

#[tokio::test]
async fn delegated_subagent_inherits_middleware_with_delegated_scope() {
    let root = unique_dir("delegated-middleware-scope");
    let scopes = Arc::new(Mutex::new(Vec::new()));
    let mut middleware = MiddlewareRegistry::new();
    middleware.register_runtime(Arc::new(ScopeRecordingMiddleware {
        scopes: scopes.clone(),
    }));
    let executor = RuntimeSubagentExecutor::new(
        Arc::new(ConstantModel {
            text: "done".to_string(),
        }),
        Arc::<str>::from("system"),
        Arc::new(root),
    )
    .with_middleware_context(
        Arc::new(middleware),
        test_model_invocation().clone(),
        Arc::<str>::from("default"),
        3,
    );

    let summary = executor
        .execute("inspect".to_string(), CancellationToken::new())
        .await;

    assert!(summary.error.is_none(), "{:?}", summary.error);
    assert_eq!(
        *scopes.lock().expect("recorded scopes"),
        vec![MiddlewareAgentScope::DelegatedSubagent]
    );
}

#[tokio::test]
async fn subagent_results_are_truncated_on_unicode_boundaries() {
    let root = unique_dir("subagent-truncate");
    let mut executor = RuntimeSubagentExecutor::new(
        Arc::new(ConstantModel {
            text: "甲乙丙丁".to_string(),
        }),
        Arc::<str>::from("system"),
        Arc::new(root),
    );
    executor.max_result_chars = 3;

    let summary = executor
        .execute("unicode result".to_string(), CancellationToken::new())
        .await;

    assert_eq!(summary.result.as_deref(), Some("甲乙丙"));
    assert!(summary.truncated);
    assert_eq!(summary.model_calls, 1);
    assert_eq!(summary.tool_calls, 0);
}

#[test]
fn context_estimate_includes_tool_definitions() {
    let session = Session::new();
    let without_tools = estimate_context_tokens("system", &session, "hello", &[]);
    let tools = vec![ToolDefinition::function(
        "large_tool",
        "x".repeat(4_000),
        json!({"type": "object", "properties": {}}),
    )];

    let with_tools = estimate_context_tokens("system", &session, "hello", &tools);

    assert!(with_tools > without_tools + 1_000);
}

#[test]
fn context_estimate_includes_reasoning_content() {
    let mut without_reasoning = Session::new();
    without_reasoning
        .active_thread
        .push(Message::assistant("answer"));
    let mut with_reasoning = without_reasoning.clone();
    with_reasoning.active_thread.messages[0].reasoning_content = Some("r".repeat(4_000));

    let without = estimate_context_tokens("system", &without_reasoning, "hello", &[]);
    let with = estimate_context_tokens("system", &with_reasoning, "hello", &[]);

    assert!(with > without + 1_000);
}

#[test]
fn auto_compact_trigger_is_capped_by_max_context_tokens() {
    let mut config = context_config(6);
    // 窗口阈值：(1_000_000 - 1) * 0.835 = 834_999，远超绝对上限。
    let limits = model_limits(1_000_000);

    config.max_context_tokens = Some(300_000);
    assert_eq!(auto_compact_trigger_tokens(limits, config), 300_000);

    // 窗口阈值低于上限时不受上限影响。
    config.max_context_tokens = Some(900_000);
    assert_eq!(auto_compact_trigger_tokens(limits, config), 834_999);

    // None 只保留窗口百分比阈值。
    config.max_context_tokens = None;
    assert_eq!(auto_compact_trigger_tokens(limits, config), 834_999);
}

#[test]
fn mid_turn_guard_follows_auto_compact_switch() {
    let mut config = context_config(6);
    let limits = model_limits(131_072);

    assert_eq!(
        mid_turn_context_token_limit(config, limits),
        Some(auto_compact_trigger_tokens(limits, config))
    );

    config.auto_compact = false;
    assert_eq!(mid_turn_context_token_limit(config, limits), None);
}

#[test]
fn summary_prompt_omits_reasoning_content() {
    let user = Message::user("question");
    let assistant = Message::assistant("answer").with_reasoning_content("private reasoning chain");
    let mut turn = Turn::running(user.clone());
    turn.complete(assistant.clone());
    let record = TurnRecord::new(turn, vec![user, assistant]);

    let prompt = build_summary_prompt(None, 256, None, &[record], 0);

    assert!(prompt.contains("answer"));
    assert!(!prompt.contains("private reasoning chain"));
}

#[tokio::test]
async fn manual_compaction_summarizes_old_turns_and_rebuilds_active_context() {
    let summary = valid_compact_summary("new summary");
    let summary_text = valid_compact_summary_text("new summary");
    let (base_url, requests) = spawn_recording_sse_server(vec![sse_text_body(&summary)]).await;
    let mut session = compactable_session();

    let outcome = compact_session(&client(base_url), &mut session, context_config(2))
        .await
        .expect("compact session");

    assert_eq!(outcome, CompactionOutcome::Changed);
    assert_eq!(
        session.context.summary.as_deref(),
        Some(summary_text.as_str())
    );
    assert_eq!(session.context.summarized_turns, 3);
    assert_eq!(
        session.active_thread.messages,
        vec![
            Message::system(format!("Session summary:\n{summary_text}")),
            Message::user("u3"),
            Message::assistant("a3"),
            Message::user("u4"),
            Message::assistant("a4"),
        ]
    );

    let requests = requests.lock().expect("requests lock poisoned");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("failure reason"));
    assert!(requests[0].contains("Target length: at most 256 tokens"));
}

#[tokio::test]
async fn manual_post_compact_failure_keeps_draft_out_and_returns_audit_events() {
    let root = unique_dir("manual-post-compact-failure");
    let summary = valid_compact_summary("must be discarded");
    let (base_url, _) = spawn_recording_sse_server(vec![sse_text_body(&summary)]).await;
    let mut session = compactable_session();
    let original = session.clone();
    let mut middleware = MiddlewareRegistry::new();
    middleware.register_runtime(Arc::new(FailingPostCompactMiddleware));

    let failure = compact_session_with_middleware_audit(
        &client(base_url),
        &mut session,
        context_config(2),
        MiddlewareExecutionContext {
            invocation_id: None,
            session: "default".to_string(),
            workspace_root: root,
            turn_index: original.turns.len(),
            operation_id: None,
            turn_id: None,
            model: test_model_invocation().clone(),
            permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
            agent_scope: MiddlewareAgentScope::Main,
            cancellation: CancellationToken::new(),
        },
        &middleware,
    )
    .await
    .expect_err("post compact must fail closed");

    assert_eq!(session, original);
    assert!(failure.to_string().contains("rejected compact draft"));
    assert_eq!(failure.events.len(), 2);
    assert!(matches!(
        &failure.events[1],
        AgentEvent::MiddlewareFinished(invocation)
            if invocation.outcome == agent_protocol::MiddlewareOutcome::FailedClosed
    ));
}

#[test]
fn compact_summary_parser_accepts_markdown_fenced_contract() {
    let summary_text = valid_compact_summary_text("fenced summary");
    let raw = format!(
        "```xml\n<analysis>\nprivate\n</analysis>\n<summary>\n{summary_text}\n</summary>\n```"
    );

    let parsed = parse_compact_summary_output(&raw).expect("parse summary");

    assert_eq!(parsed, summary_text);
}

#[tokio::test]
async fn compaction_retries_invalid_contract_with_repair_feedback() {
    let valid_summary = valid_compact_summary("retry summary");
    let valid_summary_text = valid_compact_summary_text("retry summary");
    let (base_url, requests) = spawn_recording_sse_server(vec![
        sse_text_body("<analysis>bad</analysis><summary>too short</summary>"),
        sse_text_body(&valid_summary),
    ])
    .await;
    let mut session = compactable_session();

    let outcome = compact_session(&client(base_url), &mut session, context_config(2))
        .await
        .expect("compact session");

    assert_eq!(outcome, CompactionOutcome::Changed);
    assert_eq!(
        session.context.summary.as_deref(),
        Some(valid_summary_text.as_str())
    );
    let requests = requests.lock().expect("requests lock poisoned");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("Repair feedback"));
    assert!(requests[1].contains("missing required section"));
}

#[tokio::test]
async fn run_agent_turn_records_completed_turn_and_event_envelopes() {
    let root = unique_dir("run-success");
    let (base_url, requests) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
    let client = client(base_url);
    let mut session = Session::new();
    let mut handler = RecordingHandler::default();
    let mcp_cache = McpToolCache::new();

    let outcome = run_agent_turn(
        RunAgentTurnContext {
            client: &client,
            model: test_model_invocation(),
            subagent_identities: &[],
            system_prompt: "system",
            context_config: context_config(2),
            model_limits: model_limits(10_000),
            workspace_root: &root,
            workspace_instructions: None,
            permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
            mcp_servers: &[],
            mcp_cache: &mcp_cache,
            tools: None,
            auto_approve_workspace_writes: true,
            session_name: "default",
            turn_index: 0,
        },
        &mut session,
        "hello",
        &mut handler,
    )
    .await
    .expect("run turn");

    assert_eq!(
        outcome,
        RunAgentTurnOutcome {
            session_changed: true,
            error: None,
        }
    );
    assert_eq!(
        session.active_thread.messages,
        vec![Message::user("hello"), Message::assistant("ok")]
    );
    assert_eq!(session.turns.len(), 1);
    assert_eq!(session.turns[0].turn.status, TurnStatus::Completed);
    assert_eq!(
        session.turns[0].turn.model.as_ref(),
        Some(test_model_invocation())
    );
    assert_eq!(session.turns[0].messages, session.active_thread.messages);
    assert_eq!(requests.lock().expect("requests lock poisoned").len(), 1);
    assert_eq!(
        handler
            .events
            .iter()
            .map(|event| event.event_index)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4, 5]
    );
    assert_eq!(handler.events[1].event, AgentEvent::ModelCallStarted);
    assert_eq!(
        handler.events[2].event,
        AgentEvent::TextDelta("ok".to_string())
    );
    assert!(matches!(
        handler.events[3].event,
        AgentEvent::ModelMessageCommitted { .. }
    ));
}

#[tokio::test]
async fn denied_before_prompt_is_logged_and_still_broadcast() {
    let root = unique_dir("middleware-deny-workspace");
    let sessions = unique_dir("middleware-deny-sessions");
    let legacy = unique_dir("middleware-deny-legacy");
    let store = SessionStore::new(&sessions, &legacy, &root, "default").expect("store");
    let handle = SessionHandle::open(
        store,
        "default",
        PermissionProfile::for_mode(PermissionMode::ReadOnly),
    )
    .expect("handle");
    let mut subscription = handle.subscribe().await.expect("subscribe");
    let client = client("http://127.0.0.1:1/v1".to_string());
    let cache = McpToolCache::new();
    let mut handler = RecordingHandler::default();
    let mut middleware = MiddlewareRegistry::new();
    middleware.register_runtime(Arc::new(PromptMiddleware {
        decision: GateDecision::Deny {
            reason: "secret detected".to_string(),
        },
        context: Vec::new(),
    }));

    let outcome = run_agent_turn_with_session_handle_and_middleware_context(
        MiddlewareAgentTurnContext::new(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                subagent_identities: &[],
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                workspace_instructions: None,
                permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
                mcp_servers: &[],
                mcp_cache: &cache,
                tools: None,
                auto_approve_workspace_writes: true,
                session_name: "default",
                turn_index: 0,
            },
            &middleware,
            MiddlewareAgentScope::Main,
        ),
        &handle,
        "persist this rejected secret",
        &mut handler,
        CancellationToken::new(),
        None,
    )
    .await
    .expect("middleware denial");

    assert!(!outcome.session_changed);
    assert!(outcome.error.is_some());
    let projection = handle.projection().await;
    assert!(projection.turns.is_empty());
    assert_eq!(projection.middleware_audit.len(), 1);
    assert_eq!(
        projection.middleware_audit[0].outcome,
        agent_protocol::MiddlewareOutcome::Deny
    );
    // 拒绝仍通过原有 notice 广播通知订阅者。
    let notice = loop {
        let envelope = subscription.recv().await.expect("subscription event");
        if let agent_protocol::SessionUpdate::Notice { message } = envelope.update {
            break message;
        }
    };
    assert!(notice.contains("prompt blocked by middleware"));
    // 被拒 prompt 以 PromptRejected fact 落盘，只作审计，不进入投影。
    let exported =
        String::from_utf8(handle.export_document_bytes().await.expect("export")).expect("utf8");
    assert!(exported.contains("persist this rejected secret"));
    let facts = exported_facts(&exported);
    let rejected = facts
        .iter()
        .find(|line| line["fact"]["type"] == "prompt_rejected")
        .expect("prompt_rejected fact");
    assert_eq!(
        rejected["fact"]["data"]["prompt"],
        json!("persist this rejected secret")
    );
    assert_eq!(
        rejected["fact"]["data"]["reasons"],
        json!(["prompt-policy: secret detected"])
    );
    assert!(projection.turns.is_empty());
    assert!(projection.context.messages.is_empty());
}

#[tokio::test]
async fn denied_before_prompt_is_logged_when_event_delivery_fails() {
    let root = unique_dir("middleware-deny-handler-failure-workspace");
    let sessions = unique_dir("middleware-deny-handler-failure-sessions");
    let legacy = unique_dir("middleware-deny-handler-failure-legacy");
    let store = SessionStore::new(&sessions, &legacy, &root, "default").expect("store");
    let handle = SessionHandle::open(
        store,
        "default",
        PermissionProfile::for_mode(PermissionMode::ReadOnly),
    )
    .expect("handle");
    let client = client("http://127.0.0.1:1/v1".to_string());
    let cache = McpToolCache::new();
    let mut handler = FailOnFirstEvent;
    let mut middleware = MiddlewareRegistry::new();
    middleware.register_runtime(Arc::new(PromptMiddleware {
        decision: GateDecision::Deny {
            reason: "secret detected".to_string(),
        },
        context: Vec::new(),
    }));

    let result = run_agent_turn_with_session_handle_and_middleware_context(
        MiddlewareAgentTurnContext::new(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                subagent_identities: &[],
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                workspace_instructions: None,
                permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
                mcp_servers: &[],
                mcp_cache: &cache,
                tools: None,
                auto_approve_workspace_writes: true,
                session_name: "default",
                turn_index: 0,
            },
            &middleware,
            MiddlewareAgentScope::Main,
        ),
        &handle,
        "persist this rejected secret",
        &mut handler,
        CancellationToken::new(),
        None,
    )
    .await;
    assert!(result.is_err());

    let exported =
        String::from_utf8(handle.export_document_bytes().await.expect("export")).expect("utf8");
    let facts = exported_facts(&exported);
    let rejected = facts
        .iter()
        .find(|line| line["fact"]["type"] == "prompt_rejected")
        .expect("prompt_rejected fact");
    assert_eq!(
        rejected["fact"]["data"]["prompt"],
        json!("persist this rejected secret")
    );
}

#[tokio::test]
async fn before_prompt_context_is_sent_once_and_not_persisted() {
    let root = unique_dir("middleware-context");
    let (base_url, requests) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
    let client = client(base_url);
    let cache = McpToolCache::new();
    let mut session = Session::new();
    let mut handler = RecordingHandler::default();
    let mut middleware = MiddlewareRegistry::new();
    middleware.register_runtime(Arc::new(PromptMiddleware {
        decision: GateDecision::Continue,
        context: vec![agent_core::ContextBlock::new("ephemeral policy")],
    }));

    let outcome = run_agent_turn_with_middleware_context(
        MiddlewareAgentTurnContext::new(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                subagent_identities: &[],
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                workspace_instructions: None,
                permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
                mcp_servers: &[],
                mcp_cache: &cache,
                tools: None,
                auto_approve_workspace_writes: true,
                session_name: "default",
                turn_index: 0,
            },
            &middleware,
            MiddlewareAgentScope::Main,
        ),
        &mut session,
        "hello",
        &mut handler,
        CancellationToken::new(),
    )
    .await
    .expect("run");

    assert_eq!(outcome.error, None);
    assert!(
        requests.lock().expect("requests")[0].contains("ephemeral policy"),
        "middleware context must reach the model request"
    );
    assert!(
        session
            .active_thread
            .messages
            .iter()
            .all(|message| message.content.as_deref() != Some("ephemeral policy"))
    );
}

fn exported_facts(exported: &str) -> Vec<serde_json::Value> {
    exported
        .lines()
        .skip(1)
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect()
}

#[tokio::test]
async fn turn_started_fact_records_the_effective_system_prompt() {
    let root = unique_dir("system-prompt-workspace");
    let sessions = unique_dir("system-prompt-sessions");
    let legacy = unique_dir("system-prompt-legacy");
    let (base_url, requests) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
    let client = client(base_url);
    let cache = McpToolCache::new();
    let store = SessionStore::new(&sessions, &legacy, &root, "default").expect("store");
    let handle = SessionHandle::open(
        store,
        "default",
        PermissionProfile::for_mode(PermissionMode::ReadOnly),
    )
    .expect("handle");
    let mut handler = RecordingHandler::default();
    let middleware = MiddlewareRegistry::new();

    let outcome = run_agent_turn_with_session_handle_and_middleware_context(
        MiddlewareAgentTurnContext::new(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                subagent_identities: &[],
                system_prompt: "base prompt",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                workspace_instructions: None,
                permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
                mcp_servers: &[],
                mcp_cache: &cache,
                tools: None,
                auto_approve_workspace_writes: true,
                session_name: "default",
                turn_index: 0,
            },
            &middleware,
            MiddlewareAgentScope::Main,
        ),
        &handle,
        "hello",
        &mut handler,
        CancellationToken::new(),
        None,
    )
    .await
    .expect("run");

    assert_eq!(outcome.error, None);
    let exported =
        String::from_utf8(handle.export_document_bytes().await.expect("export")).expect("utf8");
    let facts = exported_facts(&exported);
    let turn_started = facts
        .iter()
        .find(|line| line["fact"]["type"] == "turn_started")
        .expect("turn_started fact");
    let logged_prompt = turn_started["fact"]["data"]["system_prompt"]
        .as_str()
        .expect("system prompt string");
    // 模型可见 prompt = base + <environment> 块 + 委派 guidance（无持久 subagent controller）。
    assert!(
        logged_prompt.starts_with(&format!(
            "base prompt\n\n<environment>\nworkspace_root: {}\n",
            root.display()
        )),
        "unexpected prompt: {logged_prompt}"
    );
    assert!(logged_prompt.contains("\nos: "));
    assert!(logged_prompt.contains("\narch: "));
    assert!(logged_prompt.contains("\ndate: "));
    assert!(logged_prompt.ends_with(&format!("</environment>\n\n{PARENT_SUBAGENT_GUIDANCE}")));
    // 与模型请求实际携带的 system 消息逐字节一致。
    let request = requests.lock().expect("requests")[0].clone();
    let body = request.split("\r\n\r\n").nth(1).expect("request body");
    let body: serde_json::Value = serde_json::from_str(body).expect("parse request body");
    assert_eq!(body["messages"][0]["role"], json!("system"));
    assert_eq!(body["messages"][0]["content"], json!(logged_prompt));
}

#[tokio::test]
async fn agents_md_edits_take_effect_on_the_next_turn() {
    let root = unique_dir("agents-per-turn");
    let agents_md = root.join(AGENTS_MD_FILE_NAME);
    fs::write(&agents_md, "first version of the rules").expect("write AGENTS.md");
    let (base_url, requests) =
        spawn_recording_sse_server(vec![sse_text_body("one"), sse_text_body("two")]).await;
    let client = client(base_url);
    let mcp_cache = McpToolCache::new();
    let instructions = WorkspaceInstructionsCache::new(&root);
    assert!(instructions.prewarm().is_empty());
    let mut session = Session::new();
    let mut handler = RecordingHandler::default();
    let context = RunAgentTurnContext {
        client: &client,
        model: test_model_invocation(),
        subagent_identities: &[],
        system_prompt: "base prompt",
        context_config: context_config(2),
        model_limits: model_limits(10_000),
        workspace_root: &root,
        workspace_instructions: Some(&instructions),
        permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
        mcp_servers: &[],
        mcp_cache: &mcp_cache,
        tools: None,
        auto_approve_workspace_writes: true,
        session_name: "default",
        turn_index: 0,
    };

    run_agent_turn(context, &mut session, "first", &mut handler)
        .await
        .expect("first turn");

    fs::write(&agents_md, "second version of the rules").expect("rewrite AGENTS.md");
    bump_mtime(&agents_md);
    run_agent_turn(context, &mut session, "second", &mut handler)
        .await
        .expect("second turn");

    let requests = requests.lock().expect("requests lock poisoned");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("first version of the rules"));
    assert!(!requests[0].contains("second version of the rules"));
    assert!(requests[1].contains("second version of the rules"));
    assert!(!requests[1].contains("first version of the rules"));
    // 两个 turn 都携带 <environment> 块。
    assert!(requests[0].contains("<environment>"));
    assert!(requests[1].contains("<environment>"));

    fs::remove_dir_all(root).expect("remove root");
}

#[tokio::test]
async fn middleware_injected_context_is_logged_with_the_invocation() {
    let root = unique_dir("middleware-log-workspace");
    let sessions = unique_dir("middleware-log-sessions");
    let legacy = unique_dir("middleware-log-legacy");
    let (base_url, requests) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
    let client = client(base_url);
    let cache = McpToolCache::new();
    let store = SessionStore::new(&sessions, &legacy, &root, "default").expect("store");
    let handle = SessionHandle::open(
        store,
        "default",
        PermissionProfile::for_mode(PermissionMode::ReadOnly),
    )
    .expect("handle");
    let mut handler = RecordingHandler::default();
    let mut middleware = MiddlewareRegistry::new();
    middleware.register_runtime(Arc::new(PromptMiddleware {
        decision: GateDecision::Continue,
        context: vec![agent_core::ContextBlock::new("ephemeral policy")],
    }));

    let outcome = run_agent_turn_with_session_handle_and_middleware_context(
        MiddlewareAgentTurnContext::new(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                subagent_identities: &[],
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                workspace_instructions: None,
                permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
                mcp_servers: &[],
                mcp_cache: &cache,
                tools: None,
                auto_approve_workspace_writes: true,
                session_name: "default",
                turn_index: 0,
            },
            &middleware,
            MiddlewareAgentScope::Main,
        ),
        &handle,
        "hello",
        &mut handler,
        CancellationToken::new(),
        None,
    )
    .await
    .expect("run");

    assert_eq!(outcome.error, None);
    assert!(
        requests.lock().expect("requests")[0].contains("ephemeral policy"),
        "middleware context must reach the model request"
    );
    let projection = handle.projection().await;
    let before_prompt = projection
        .middleware_audit
        .iter()
        .find(|invocation| invocation.stage == agent_protocol::MiddlewareStage::BeforePrompt)
        .expect("before_prompt audit");
    assert_eq!(before_prompt.injected_context.len(), 1);
    assert_eq!(
        before_prompt.injected_context[0].content,
        "ephemeral policy"
    );
    assert_eq!(
        before_prompt.injected_context[0].middleware_id,
        "prompt-policy"
    );
    // 注入内容只留在审计 fact 中，不进入模型上下文投影。
    assert!(
        projection
            .context
            .messages
            .iter()
            .all(|message| message.content.as_deref() != Some("ephemeral policy"))
    );
    let exported =
        String::from_utf8(handle.export_document_bytes().await.expect("export")).expect("utf8");
    assert!(exported.contains("ephemeral policy"));
}

#[tokio::test]
async fn delegate_task_runs_an_isolated_read_only_subagent() {
    let root = unique_dir("subagent-success");
    fs::write(root.join("note.txt"), "workspace evidence\n").expect("write note");
    fs::write(
        root.join(AGENTS_MD_FILE_NAME),
        "Always preserve the workspace evidence.",
    )
    .expect("write project instructions");
    let workspace_instructions = load_workspace_instructions(&root, "project policy");
    let (base_url, requests) = spawn_recording_sse_server(vec![
        tool_call_body(
            "delegate-1",
            "delegate_task",
            json!({"task": "Read note.txt and report the evidence"}),
        ),
        tool_call_body(
            "read-1",
            "read_file",
            json!({"path": "note.txt", "max_lines": 20}),
        ),
        sse_text_body("The file contains workspace evidence."),
        sse_text_body("The subagent confirmed the workspace evidence."),
    ])
    .await;
    let client = client(base_url);
    let mut session = Session::new();
    let mut handler = RecordingHandler::default();
    let mcp_cache = McpToolCache::new();
    let subagent_identities = [SubagentIdentity {
        id: "custom-researcher".to_string(),
        name: "测试研究员".to_string(),
    }];

    let outcome = run_agent_turn(
        RunAgentTurnContext {
            client: &client,
            model: test_model_invocation(),
            subagent_identities: &subagent_identities,
            system_prompt: &workspace_instructions.effective_system_prompt,
            context_config: context_config(2),
            model_limits: model_limits(100_000),
            workspace_root: &root,
            workspace_instructions: None,
            permissions: PermissionProfile::for_mode(
                agent_protocol::PermissionMode::DangerFullAccess,
            ),
            mcp_servers: &[],
            mcp_cache: &mcp_cache,
            tools: None,
            auto_approve_workspace_writes: true,
            session_name: "default",
            turn_index: 0,
        },
        &mut session,
        "Use a subagent to inspect the note",
        &mut handler,
    )
    .await
    .expect("run delegated turn");

    assert_eq!(outcome.error, None);
    assert_eq!(session.turns.len(), 1);
    assert_eq!(session.turns[0].turn.status, TurnStatus::Completed);
    assert_eq!(session.turns[0].messages.len(), 4);
    assert!(session.turns[0].messages.iter().any(|message| {
        message.role == agent_protocol::Role::Tool
            && message.content.as_deref().is_some_and(|content| {
                content.contains("The file contains workspace evidence.")
                    && content.contains("\"agent_id\":\"custom-researcher\"")
                    && content.contains("\"agent_name\":\"测试研究员\"")
            })
    }));
    assert!(!session.turns[0].messages.iter().any(|message| {
        message
            .content
            .as_deref()
            .is_some_and(|content| content == "workspace evidence\n")
    }));
    assert!(handler.events.iter().any(|event| matches!(
        &event.event,
            AgentEvent::SubagentStarted {
                id,
                agent_id: Some(agent_id),
                agent_name: Some(agent_name),
            task,
            ..
        }
            if id == "delegate-1"
                && agent_id == "custom-researcher"
                && agent_name == "测试研究员"
                && task == "Read note.txt and report the evidence"
    )));
    let started_name = handler
        .events
        .iter()
        .find_map(|event| match &event.event {
            AgentEvent::SubagentStarted { agent_name, .. } => agent_name.as_deref(),
            _ => None,
        })
        .expect("subagent start name");
    assert!(handler.events.iter().any(|event| matches!(
        &event.event,
        AgentEvent::SubagentFinished { id, ok: true, summary }
            if id == "delegate-1"
                && summary.agent_id.as_deref() == Some("custom-researcher")
                && summary.agent_name.as_deref() == Some(started_name)
                && summary.model_calls == 2
                && summary.tool_calls == 1
                && summary.result.as_deref() == Some("The file contains workspace evidence.")
    )));

    let requests = requests.lock().expect("requests lock poisoned");
    assert_eq!(requests.len(), 4);
    assert!(requests[0].contains("delegate_task"));
    assert!(requests[0].contains("web_fetch"));
    assert!(requests[0].contains(PARENT_SUBAGENT_GUIDANCE));
    assert!(requests[0].contains("Always preserve the workspace evidence."));
    assert!(requests[1].contains("Read note.txt and report the evidence"));
    assert!(requests[1].contains(CHILD_SUBAGENT_GUIDANCE));
    assert!(requests[1].contains("Always preserve the workspace evidence."));
    assert!(requests[1].contains("read_file"));
    assert!(requests[1].contains("list_files"));
    assert!(requests[1].contains("search_text"));
    assert!(requests[1].contains("web_fetch"));
    assert!(!requests[1].contains("delegate_task"));
    assert!(!requests[1].contains("write_file"));
    assert!(!requests[1].contains("shell_command"));
    assert!(!requests[1].contains("Use a subagent to inspect the note"));
    assert!(requests[2].contains("workspace evidence"));
    assert!(requests[3].contains("The file contains workspace evidence."));
}

#[tokio::test]
async fn delegated_subagent_respects_parent_tool_filter() {
    let root = unique_dir("subagent-tool-filter");
    let (base_url, requests) = spawn_recording_sse_server(vec![
        tool_call_body(
            "delegate-filter-1",
            "delegate_task",
            json!({"task": "Inspect the workspace"}),
        ),
        sse_text_body("The child completed without the denied tool."),
        sse_text_body("The parent completed."),
    ])
    .await;
    let client = client(base_url);
    let mut session = Session::new();
    let mut handler = RecordingHandler::default();
    let mcp_cache = McpToolCache::new();
    let tools = ToolsConfig {
        allow: Vec::new(),
        deny: vec!["read_file".to_string()],
    };
    let subagent_identities = [SubagentIdentity {
        id: "filtered-researcher".to_string(),
        name: "Filtered Researcher".to_string(),
    }];

    run_agent_turn(
        RunAgentTurnContext {
            client: &client,
            model: test_model_invocation(),
            subagent_identities: &subagent_identities,
            system_prompt: "system",
            context_config: context_config(2),
            model_limits: model_limits(100_000),
            workspace_root: &root,
            workspace_instructions: None,
            permissions: PermissionProfile::for_mode(PermissionMode::ReadOnly),
            mcp_servers: &[],
            mcp_cache: &mcp_cache,
            tools: Some(&tools),
            auto_approve_workspace_writes: true,
            session_name: "default",
            turn_index: 0,
        },
        &mut session,
        "Delegate a workspace inspection",
        &mut handler,
    )
    .await
    .expect("run delegated turn");

    let requests = requests.lock().expect("requests lock poisoned");
    assert_eq!(requests.len(), 3);
    assert!(requests[0].contains("\"name\":\"delegate_task\""));
    assert!(!requests[0].contains("\"name\":\"read_file\""));
    assert!(!requests[1].contains("\"name\":\"read_file\""));
    assert!(requests[1].contains("\"name\":\"list_files\""));
    assert!(requests[1].contains("\"name\":\"search_text\""));
    assert!(requests[1].contains("\"name\":\"web_fetch\""));
}

#[tokio::test]
async fn event_handler_failure_after_completion_commits_turn_and_reports_error() {
    let root = unique_dir("handler-failure");
    let (base_url, _) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
    let client = client(base_url);
    let mut session = Session::from_thread(Thread {
        messages: vec![Message::user("before"), Message::assistant("context")],
    });
    let mut handler = FailOnAgentMessage;
    let mcp_cache = McpToolCache::new();

    let outcome = run_agent_turn(
        RunAgentTurnContext {
            client: &client,
            model: test_model_invocation(),
            subagent_identities: &[],
            system_prompt: "system",
            context_config: context_config(2),
            model_limits: model_limits(10_000),
            workspace_root: &root,
            workspace_instructions: None,
            permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
            mcp_servers: &[],
            mcp_cache: &mcp_cache,
            tools: None,
            auto_approve_workspace_writes: true,
            session_name: "default",
            turn_index: 0,
        },
        &mut session,
        "hello",
        &mut handler,
    )
    .await
    .expect("handler failure is reported after committing the terminal turn");

    assert!(outcome.session_changed);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("simulated output failure"))
    );
    assert_eq!(session.turns.len(), 1);
    assert_eq!(session.turns[0].turn.status, TurnStatus::Completed);
    assert_eq!(
        session.active_thread.messages,
        vec![
            Message::user("before"),
            Message::assistant("context"),
            Message::user("hello"),
            Message::assistant("ok"),
        ]
    );
}

#[tokio::test]
async fn event_handler_failure_mid_turn_records_failed_turn() {
    let root = unique_dir("handler-streaming-failure");
    let (base_url, _) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
    let client = client(base_url);
    let original_thread = Thread {
        messages: vec![Message::user("before"), Message::assistant("context")],
    };
    let mut session = Session::from_thread(original_thread.clone());
    let mut handler = FailOnTextDelta;
    let mcp_cache = McpToolCache::new();

    let outcome = run_agent_turn(
        RunAgentTurnContext {
            client: &client,
            model: test_model_invocation(),
            subagent_identities: &[],
            system_prompt: "system",
            context_config: context_config(2),
            model_limits: model_limits(10_000),
            workspace_root: &root,
            workspace_instructions: None,
            permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
            mcp_servers: &[],
            mcp_cache: &mcp_cache,
            tools: None,
            auto_approve_workspace_writes: true,
            session_name: "default",
            turn_index: 0,
        },
        &mut session,
        "hello",
        &mut handler,
    )
    .await
    .expect("handler failure must still produce an auditable outcome");

    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("simulated streaming failure"))
    );
    assert_eq!(session.active_thread, original_thread);
    assert_eq!(session.turns.len(), 1);
    assert_eq!(session.turns[0].turn.status, TurnStatus::Failed);
    assert_eq!(
        session.turns[0].turn.model.as_ref(),
        Some(test_model_invocation())
    );
    assert!(
        session.turns[0]
            .turn
            .error
            .as_deref()
            .is_some_and(|error| error.contains("simulated streaming failure"))
    );
}

#[tokio::test]
async fn cancellation_records_failed_turn_without_changing_active_context() {
    let root = unique_dir("cancelled-turn");
    let model = PendingModel;
    let original_thread = Thread {
        messages: vec![Message::user("before"), Message::assistant("context")],
    };
    let mut session = Session::from_thread(original_thread.clone());
    let cancellation = CancellationToken::new();
    let mut handler = CancelOnTurnStarted {
        cancellation: cancellation.clone(),
        events: Vec::new(),
    };
    let mcp_cache = McpToolCache::new();

    let outcome = run_agent_turn_with_cancellation(
        RunAgentTurnContext {
            client: &model,
            model: test_model_invocation(),
            subagent_identities: &[],
            system_prompt: "system",
            context_config: context_config(2),
            model_limits: model_limits(1_000_000),
            workspace_root: &root,
            workspace_instructions: None,
            permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
            mcp_servers: &[],
            mcp_cache: &mcp_cache,
            tools: None,
            auto_approve_workspace_writes: true,
            session_name: "default",
            turn_index: 0,
        },
        &mut session,
        "cancel me",
        &mut handler,
        cancellation,
    )
    .await
    .expect("cancelled turn should close normally");

    assert_eq!(
        outcome,
        RunAgentTurnOutcome {
            session_changed: true,
            error: Some("turn cancelled".to_string()),
        }
    );
    assert_eq!(session.active_thread, original_thread);
    assert_eq!(session.turns.len(), 1);
    assert_eq!(session.turns[0].turn.status, TurnStatus::Failed);
    assert_eq!(
        session.turns[0].turn.model.as_ref(),
        Some(test_model_invocation())
    );
    assert_eq!(
        session.turns[0].turn.error.as_deref(),
        Some("turn cancelled")
    );
    assert_eq!(session.turns[0].messages, vec![Message::user("cancel me")]);
    assert!(
        handler
            .events
            .iter()
            .any(|event| event.event == AgentEvent::Error("turn cancelled".to_string()))
    );
}

#[cfg(not(windows))]
#[tokio::test]
async fn inspect_mcp_servers_returns_discovered_tools() {
    let root = unique_dir("inspect-mcp-tools");
    let server_script = root.join("fake-inspection-mcp.sh");
    fs::write(
            &server_script,
            r#"#!/bin/sh
count=0
while IFS= read -r line; do
  count=$((count + 1))
  if [ "$count" -eq 1 ]; then
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"1"}}}'
  elif [ "$count" -eq 3 ]; then
    printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"Search Docs","description":"Search docs","inputSchema":{"type":"object"}}]}}'
  fi
done
"#,
        )
        .expect("write fake MCP server");
    let server = McpServerConfig {
        name: "Docs".to_string(),
        transport: agent_config::McpTransport::Stdio,
        command: "sh".to_string(),
        args: vec![server_script.display().to_string()],
        env: Default::default(),
        cwd: None,
        url: None,
        http_headers: Default::default(),
        enabled: true,
        startup_timeout_sec: 5,
        tool_timeout_sec: 5,
        require_approval: None,
    };

    let inspection = inspect_mcp_servers(&root, &[server]).await;

    assert!(inspection.diagnostics.is_empty());
    assert_eq!(inspection.tools.len(), 1);
    assert_eq!(inspection.tools[0].name, "mcp__docs__search_docs");
    assert!(inspection.tools[0].description.contains("Search docs"));
}

#[cfg(not(windows))]
#[tokio::test]
async fn run_agent_turn_includes_mcp_tool_definitions_in_model_request() {
    let root = unique_dir("run-mcp-tools");
    let server_script = root.join("fake-mcp.sh");
    fs::write(
            &server_script,
            r#"#!/bin/sh
count=0
while IFS= read -r line; do
  count=$((count + 1))
  if [ "$count" -eq 1 ]; then
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake","version":"1"}}}'
  elif [ "$count" -eq 3 ]; then
    printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"Search Docs","description":"Search docs","inputSchema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}}]}}'
  fi
done
"#,
        )
        .expect("write fake MCP server");
    let (base_url, requests) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
    let client = client(base_url);
    let mut session = Session::new();
    let mut handler = RecordingHandler::default();
    let mcp_cache = McpToolCache::new();
    let mcp_servers = vec![McpServerConfig {
        name: "Docs".to_string(),
        transport: agent_config::McpTransport::Stdio,
        command: "sh".to_string(),
        args: vec![server_script.display().to_string()],
        env: Default::default(),
        cwd: None,
        url: None,
        http_headers: Default::default(),
        enabled: true,
        startup_timeout_sec: 5,
        tool_timeout_sec: 5,
        require_approval: None,
    }];

    let outcome = run_agent_turn(
        RunAgentTurnContext {
            client: &client,
            model: test_model_invocation(),
            subagent_identities: &[],
            system_prompt: "system",
            context_config: context_config(2),
            model_limits: model_limits(10_000),
            workspace_root: &root,
            workspace_instructions: None,
            permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
            mcp_servers: &mcp_servers,
            mcp_cache: &mcp_cache,
            tools: None,
            auto_approve_workspace_writes: true,
            session_name: "default",
            turn_index: 0,
        },
        &mut session,
        "hello",
        &mut handler,
    )
    .await
    .expect("run turn");

    assert_eq!(outcome.error, None);
    let requests = requests.lock().expect("requests lock poisoned");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("mcp__docs__search_docs"));
    assert!(requests[0].contains("Search docs"));
}

#[tokio::test]
async fn run_agent_turn_emits_mcp_diagnostics_as_warnings() {
    let root = unique_dir("run-mcp-warning");
    let (base_url, requests) = spawn_recording_sse_server(vec![sse_text_body("ok")]).await;
    let client = client(base_url);
    let mut session = Session::new();
    let mut handler = RecordingHandler::default();
    let mcp_cache = McpToolCache::new();
    let mcp_servers = vec![McpServerConfig {
        name: "bad".to_string(),
        transport: agent_config::McpTransport::Stdio,
        command: "definitely-not-a-real-morrow-mcp-command".to_string(),
        args: Vec::new(),
        env: Default::default(),
        cwd: None,
        url: None,
        http_headers: Default::default(),
        enabled: true,
        startup_timeout_sec: 1,
        tool_timeout_sec: 1,
        require_approval: None,
    }];

    let outcome = run_agent_turn(
        RunAgentTurnContext {
            client: &client,
            model: test_model_invocation(),
            subagent_identities: &[],
            system_prompt: "system",
            context_config: context_config(2),
            model_limits: model_limits(10_000),
            workspace_root: &root,
            workspace_instructions: None,
            permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
            mcp_servers: &mcp_servers,
            mcp_cache: &mcp_cache,
            tools: None,
            auto_approve_workspace_writes: true,
            session_name: "default",
            turn_index: 0,
        },
        &mut session,
        "hello",
        &mut handler,
    )
    .await
    .expect("run turn");

    assert_eq!(outcome.error, None);
    assert_eq!(requests.lock().expect("requests lock poisoned").len(), 1);
    assert!(matches!(
        &handler.events[0].event,
        AgentEvent::Warning(message)
            if message.contains("mcp server bad")
                && message.contains("failed to start MCP stdio server")
    ));
    assert_eq!(handler.events[0].event_index, 0);
    assert_eq!(handler.events[1].event, AgentEvent::TurnStarted);
    assert_eq!(handler.events[1].event_index, 1);
}

#[cfg(not(windows))]
#[tokio::test]
async fn run_agent_turn_reuses_mcp_cache_across_turns() {
    let root = unique_dir("run-mcp-cache");
    let server_script = root.join("fake-mcp.sh");
    let marker = root.join("started.txt");
    fs::write(
            &server_script,
            format!(
                r#"#!/bin/sh
printf 'started\n' >> '{}'
count=0
while IFS= read -r line; do
  count=$((count + 1))
  if [ "$count" -eq 1 ]; then
    printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-06-18","capabilities":{{}},"serverInfo":{{"name":"fake","version":"1"}}}}}}'
  elif [ "$count" -eq 3 ]; then
    printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"Search Docs","description":"Search docs","inputSchema":{{"type":"object"}}}}]}}}}'
  fi
done
"#,
                marker.display()
            ),
        )
        .expect("write fake MCP server");
    let (base_url, requests) =
        spawn_recording_sse_server(vec![sse_text_body("one"), sse_text_body("two")]).await;
    let client = client(base_url);
    let mut session = Session::new();
    let mut first_handler = RecordingHandler::default();
    let mut second_handler = RecordingHandler::default();
    let mcp_cache = McpToolCache::new();
    let mcp_servers = vec![McpServerConfig {
        name: "Docs".to_string(),
        transport: agent_config::McpTransport::Stdio,
        command: "sh".to_string(),
        args: vec![server_script.display().to_string()],
        env: Default::default(),
        cwd: None,
        url: None,
        http_headers: Default::default(),
        enabled: true,
        startup_timeout_sec: 5,
        tool_timeout_sec: 5,
        require_approval: None,
    }];

    for (turn_index, prompt, handler) in [
        (0, "hello", &mut first_handler),
        (1, "again", &mut second_handler),
    ] {
        let outcome = run_agent_turn(
            RunAgentTurnContext {
                client: &client,
                model: test_model_invocation(),
                subagent_identities: &[],
                system_prompt: "system",
                context_config: context_config(2),
                model_limits: model_limits(10_000),
                workspace_root: &root,
                workspace_instructions: None,
                permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
                mcp_servers: &mcp_servers,
                mcp_cache: &mcp_cache,
                tools: None,
                auto_approve_workspace_writes: true,
                session_name: "default",
                turn_index,
            },
            &mut session,
            prompt,
            handler,
        )
        .await
        .expect("run turn");
        assert_eq!(outcome.error, None);
    }

    assert_eq!(requests.lock().expect("requests lock poisoned").len(), 2);
    assert_eq!(
        fs::read_to_string(marker).expect("marker").lines().count(),
        1
    );
}

#[tokio::test]
async fn approval_deny_path_resumes_stream_and_records_turn() {
    let root = unique_dir("approval-deny");
    let first_body = tool_call_body(
        "call_1",
        "write_file",
        json!({
            "path": "note.txt",
            "content": "created\n"
        }),
    );
    let second_body = sse_text_body("Denied");
    let (base_url, _) = spawn_recording_sse_server(vec![first_body, second_body]).await;
    let client = client(base_url);
    let mut session = Session::new();
    let mut handler = ApprovalHandler {
        events: Vec::new(),
        approved: false,
    };
    let mcp_cache = McpToolCache::new();

    let outcome = run_agent_turn(
        RunAgentTurnContext {
            client: &client,
            model: test_model_invocation(),
            subagent_identities: &[],
            system_prompt: "system",
            context_config: context_config(2),
            model_limits: model_limits(10_000),
            workspace_root: &root,
            workspace_instructions: None,
            permissions: PermissionProfile::for_mode(
                agent_protocol::PermissionMode::WorkspaceWrite,
            ),
            mcp_servers: &[],
            mcp_cache: &mcp_cache,
            tools: None,
            // 该用例验证审批拒绝链路，显式回退到逐次审批旧行为。
            auto_approve_workspace_writes: false,
            session_name: "default",
            turn_index: 0,
        },
        &mut session,
        "write note",
        &mut handler,
    )
    .await
    .expect("run turn");

    assert_eq!(outcome.error, None);
    assert_eq!(session.turns.len(), 1);
    assert_eq!(session.turns[0].turn.status, TurnStatus::Completed);
    assert!(
        handler
            .events
            .iter()
            .any(|event| matches!(event.event, AgentEvent::ApprovalRequested(_)))
    );
    assert!(handler.events.iter().any(|event| {
        matches!(
            &event.event,
            AgentEvent::ApprovalResolved(decision) if !decision.approved
        )
    }));
}

#[tokio::test]
async fn auto_compaction_llm_failure_falls_back_and_runs_main_turn() {
    let root = unique_dir("run-compact-fallback");
    let (base_url, requests) =
        spawn_recording_sse_server(vec!["data: {not-json}\n\n", sse_text_body("ok")]).await;
    let client = client(base_url);
    let mut session = compactable_session();
    session.turns[0] = completed_record(&"older user context ".repeat(1_000), "a0");
    rebuild_active_thread(&mut session);
    let mut handler = RecordingHandler::default();
    let mcp_cache = McpToolCache::new();

    let outcome = run_agent_turn(
        RunAgentTurnContext {
            client: &client,
            model: test_model_invocation(),
            subagent_identities: &[],
            system_prompt: "system",
            context_config: context_config(2),
            // 工具定义计入上下文估算；窗口要同时触发压缩并容纳回退压缩后的保留内容。
            model_limits: model_limits(4_000),
            workspace_root: &root,
            workspace_instructions: None,
            permissions: PermissionProfile::for_mode(agent_protocol::PermissionMode::ReadOnly),
            mcp_servers: &[],
            mcp_cache: &mcp_cache,
            tools: None,
            auto_approve_workspace_writes: true,
            session_name: "default",
            turn_index: session.turns.len(),
        },
        &mut session,
        "hello",
        &mut handler,
    )
    .await
    .expect("run turn");

    assert_eq!(
        outcome,
        RunAgentTurnOutcome {
            session_changed: true,
            error: None,
        }
    );
    assert_eq!(session.turns.len(), 6);
    assert_eq!(
        session.turns.last().expect("failed turn").turn.status,
        TurnStatus::Completed
    );
    assert!(
        session
            .context
            .summary
            .as_deref()
            .expect("fallback summary")
            .contains("deterministic fallback")
    );
    assert_eq!(requests.lock().expect("requests lock poisoned").len(), 2);
    assert!(!handler.events.is_empty());
}

#[test]
fn file_summary_helper_is_available_to_tests() {
    let file = agent_protocol::FileChangeSummary {
        path: "note.txt".to_string(),
        operation: FileChangeOperation::Add,
        replacements: 0,
        created: true,
        overwritten: false,
        deleted: false,
    };

    assert_eq!(file.operation.as_str(), "add");
}
