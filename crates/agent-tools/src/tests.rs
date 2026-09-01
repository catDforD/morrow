use super::*;
use agent_protocol::{ApprovalAction, PermissionMode, ShellPolicy};
use async_trait::async_trait;
use std::future::Future;
use std::time::Instant;

static UNIQUE_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestTool(&'static str);

struct TestSubagentExecutor;

#[test]
fn subagent_role_tool_matrix_is_explicit_and_permission_filtered() {
    let workspace_write = PermissionProfile {
        mode: PermissionMode::WorkspaceWrite,
        shell: ShellPolicy::Prompt,
    };
    let explore = BuiltInToolAllowlist::for_subagent(SubagentRole::Explore, workspace_write);
    let plan = BuiltInToolAllowlist::for_subagent(SubagentRole::Plan, workspace_write);
    for allowlist in [&explore, &plan] {
        assert!(allowlist.contains(READ_FILE_TOOL_NAME));
        assert!(allowlist.contains(LIST_FILES_TOOL_NAME));
        assert!(allowlist.contains(SEARCH_TEXT_TOOL_NAME));
        assert!(allowlist.contains(WEB_FETCH_TOOL_NAME));
        assert!(!allowlist.contains(EDIT_FILE_TOOL_NAME));
        assert!(!allowlist.contains(WRITE_FILE_TOOL_NAME));
        assert!(!allowlist.contains(APPLY_PATCH_TOOL_NAME));
        assert!(!allowlist.contains(SHELL_COMMAND_TOOL_NAME));
    }

    let worker = BuiltInToolAllowlist::for_subagent(SubagentRole::Worker, workspace_write);
    for name in [
        READ_FILE_TOOL_NAME,
        LIST_FILES_TOOL_NAME,
        SEARCH_TEXT_TOOL_NAME,
        EDIT_FILE_TOOL_NAME,
        WRITE_FILE_TOOL_NAME,
        APPLY_PATCH_TOOL_NAME,
        SHELL_COMMAND_TOOL_NAME,
        WEB_FETCH_TOOL_NAME,
    ] {
        assert!(worker.contains(name), "worker should expose {name}");
    }

    let reviewer = BuiltInToolAllowlist::for_subagent(SubagentRole::Reviewer, workspace_write);
    assert!(reviewer.contains(READ_FILE_TOOL_NAME));
    assert!(reviewer.contains(LIST_FILES_TOOL_NAME));
    assert!(reviewer.contains(SEARCH_TEXT_TOOL_NAME));
    assert!(reviewer.contains(SHELL_COMMAND_TOOL_NAME));
    assert!(reviewer.contains(WEB_FETCH_TOOL_NAME));
    assert!(!reviewer.contains(EDIT_FILE_TOOL_NAME));
    assert!(!reviewer.contains(WRITE_FILE_TOOL_NAME));
    assert!(!reviewer.contains(APPLY_PATCH_TOOL_NAME));

    let denied = BuiltInToolAllowlist::for_subagent(
        SubagentRole::Worker,
        PermissionProfile {
            mode: PermissionMode::ReadOnly,
            shell: ShellPolicy::Deny,
        },
    );
    assert!(!denied.contains(EDIT_FILE_TOOL_NAME));
    assert!(!denied.contains(WRITE_FILE_TOOL_NAME));
    assert!(!denied.contains(APPLY_PATCH_TOOL_NAME));
    assert!(!denied.contains(SHELL_COMMAND_TOOL_NAME));
    assert!(denied.contains(WEB_FETCH_TOOL_NAME));
}

#[test]
fn subagent_permissions_are_the_intersection_of_parent_and_role_ceiling() {
    let full = PermissionProfile {
        mode: PermissionMode::DangerFullAccess,
        shell: ShellPolicy::Allow,
    };
    assert_eq!(
        effective_subagent_permissions(full, SubagentRole::Explore),
        PermissionProfile {
            mode: PermissionMode::ReadOnly,
            shell: ShellPolicy::Deny,
        }
    );
    assert_eq!(
        effective_subagent_permissions(full, SubagentRole::Worker),
        PermissionProfile {
            mode: PermissionMode::WorkspaceWrite,
            shell: ShellPolicy::Prompt,
        }
    );
    assert_eq!(
        effective_subagent_permissions(full, SubagentRole::Reviewer),
        PermissionProfile {
            mode: PermissionMode::ReadOnly,
            shell: ShellPolicy::Prompt,
        }
    );

    let parent_read_only = PermissionProfile {
        mode: PermissionMode::ReadOnly,
        shell: ShellPolicy::Deny,
    };
    assert_eq!(
        effective_subagent_permissions(parent_read_only, SubagentRole::Worker),
        parent_read_only
    );
    let parent_shell_denied = PermissionProfile {
        mode: PermissionMode::WorkspaceWrite,
        shell: ShellPolicy::Deny,
    };
    assert_eq!(
        effective_subagent_permissions(parent_shell_denied, SubagentRole::Reviewer),
        PermissionProfile {
            mode: PermissionMode::ReadOnly,
            shell: ShellPolicy::Deny,
        }
    );
}

impl SubagentExecutor for TestSubagentExecutor {
    fn execute(
        &self,
        task: String,
        _cancellation: CancellationToken,
    ) -> BoxFuture<'static, SubagentExecutionSummary> {
        async move { SubagentExecutionSummary::success(task, "research complete", 2, 1, false) }
            .boxed()
    }
}

#[async_trait]
impl Tool for TestTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition::function(
            self.0,
            "test tool",
            json!({"type": "object", "properties": {}}),
        )]
    }

    async fn execute(
        &self,
        _call: ToolCall,
        _approval: Option<ToolApproval>,
        _context: ToolExecutionContext,
    ) -> ToolExecution {
        ToolExecution::Completed(ToolResult {
            ok: true,
            content: "{}".to_string(),
            error: None,
            summary: None,
        })
    }
}

fn unique_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "morrow-tools-{name}-{}-{}",
        std::process::id(),
        UNIQUE_DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

fn outside_path(root: &Path, name: &str) -> PathBuf {
    let root_name = root.file_name().expect("root file name").to_string_lossy();
    root.parent()
        .expect("root parent")
        .join(format!("{root_name}-{name}"))
}

fn registry(root: &Path) -> ToolRegistry {
    ToolRegistry::built_in(
        root,
        PermissionProfile::for_mode(PermissionMode::WorkspaceWrite),
    )
    .expect("tool registry")
}

/// 旧行为：workspace_write 下文件变更仍需逐次审批（`workspace_write_require_approval = true`）。
fn registry_requiring_approval(root: &Path) -> ToolRegistry {
    ToolRegistry::built_in_with_allowlist_and_writer_lease_and_artifact_root(
        root,
        PermissionProfile::for_mode(PermissionMode::WorkspaceWrite),
        BuiltInToolAllowlist::all(),
        None,
        None,
        false,
    )
    .expect("tool registry")
}

fn registry_with_permissions(root: &Path, permissions: PermissionProfile) -> ToolRegistry {
    ToolRegistry::built_in(root, permissions).expect("tool registry")
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall::function("call_1", name, arguments.to_string())
}

fn patch_call(patch: &str) -> ToolCall {
    call("apply_patch", json!({"patch": patch}))
}

fn wait_tool<F>(execution: F) -> ToolExecution
where
    F: Future<Output = ToolExecution>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(execution)
}

fn content<F>(execution: F) -> Value
where
    F: Future<Output = ToolExecution>,
{
    let result = completed_result(execution);
    serde_json::from_str(&result.content).expect("tool JSON")
}

fn completed_result<F>(execution: F) -> ToolResult
where
    F: Future<Output = ToolExecution>,
{
    let execution = wait_tool(execution);
    let ToolExecution::Completed(result) = execution else {
        panic!("expected completed tool execution");
    };
    result
}

fn approval_request<F>(execution: F) -> ApprovalRequest
where
    F: Future<Output = ToolExecution>,
{
    let execution = wait_tool(execution);
    let ToolExecution::ApprovalRequired(request) = execution else {
        panic!("expected approval request");
    };
    request
}

fn approved_content(tools: &ToolRegistry, call: &ToolCall) -> Value {
    let request = approval_request(tools.execute(call));
    assert!(matches!(request.action, ApprovalAction::FileChanges { .. }));
    content(tools.execute_approved(
        call,
        &ApprovalDecision::approve(request.id.clone()),
        &request,
    ))
}

#[test]
fn registry_rejects_duplicate_tool_names() {
    let mut registry = ToolRegistry::empty();
    registry
        .register(Arc::new(TestTool("same")))
        .expect("first registration");

    let error = registry
        .register(Arc::new(TestTool("same")))
        .expect_err("duplicate must fail");

    assert!(matches!(error, ToolRegistryError::DuplicateTool { name } if name == "same"));
}

#[test]
fn research_registry_only_exposes_read_only_tools() {
    let root = unique_dir("research-registry-root");
    let registry = ToolRegistry::research(&root).expect("research registry");
    let names = registry
        .definitions()
        .into_iter()
        .map(|definition| definition.function.name)
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        vec!["read_file", "list_files", "search_text", "web_fetch"]
    );
    let result = completed_result(registry.execute(&call(
        "write_file",
        json!({"path": "blocked.txt", "content": "blocked"}),
    )));
    assert!(!result.ok);
    assert!(!root.join("blocked.txt").exists());
}

#[tokio::test]
async fn delegate_task_returns_structured_subagent_result() {
    let mut registry = ToolRegistry::empty();
    let identities = default_subagent_identities();
    registry
        .register_subagent(Arc::new(TestSubagentExecutor), &identities)
        .expect("register subagent");
    let call = call(
        DELEGATE_TASK_TOOL_NAME,
        json!({"task": "Inspect the runtime"}),
    );

    let ToolExecutionKind::Subagent { task, identity } = registry.execution_kind(&call) else {
        panic!("expected subagent execution kind");
    };
    assert_eq!(task, "Inspect the runtime");
    assert!(identities.contains(&identity));
    let ToolExecution::Completed(result) = registry.execute(&call).await else {
        panic!("expected completed delegation");
    };
    assert!(result.ok);
    let content = serde_json::from_str::<Value>(&result.content).expect("subagent JSON");
    assert_eq!(
        content,
        json!({
            "ok": true,
            "agent_id": identity.id,
            "agent_name": identity.name,
            "task": "Inspect the runtime",
            "result": "research complete",
            "model_calls": 2,
            "tool_calls": 1,
            "truncated": false
        })
    );
    let summary = result
        .summary
        .and_then(|summary| summary.subagent)
        .expect("subagent summary");
    assert_eq!(summary.agent_id.as_deref(), content["agent_id"].as_str());
    assert_eq!(
        summary.agent_name.as_deref(),
        content["agent_name"].as_str()
    );
    assert_eq!(summary.result.as_deref(), Some("research complete"));
}

#[test]
fn subagent_identities_are_stable_per_call_and_unique_within_a_turn() {
    let identities = default_subagent_identities();
    let mut allocator = SubagentIdentityAllocator::with_seed(7, &identities);
    let first = allocator.identity_for("call-1");
    let assigned = (1..=4)
        .map(|index| allocator.identity_for(&format!("call-{index}")))
        .collect::<HashSet<_>>();

    assert_eq!(allocator.identity_for("call-1"), first);
    assert_eq!(assigned.len(), 4);
    assert!(
        assigned
            .iter()
            .all(|identity| identities.contains(identity))
    );
}

#[test]
fn read_file_limits_lines_and_rejects_path_escape() {
    let root = unique_dir("read-root");
    fs::write(root.join("note.txt"), "a\nb\nc\nd\n").expect("write file");
    let outside = root
        .parent()
        .expect("parent")
        .join("outside-morrow-tools.txt");
    fs::write(&outside, "secret").expect("write outside");
    let tools = registry(&root);

    let value = content(tools.execute(&call(
        "read_file",
        json!({"path": "note.txt", "start_line": 2, "max_lines": 2}),
    )));

    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["content"], "b\nc");
    assert_eq!(value["data"]["start_line"], 2);
    assert_eq!(value["data"]["end_line"], 3);
    assert_eq!(value["data"]["truncated"], true);

    let escaped = content(tools.execute(&call(
        "read_file",
        json!({"path": outside.display().to_string()}),
    )));

    assert_eq!(escaped["ok"], false);
    assert!(
        escaped["error"]
            .as_str()
            .expect("error")
            .contains("outside the workspace root")
    );
}

#[test]
fn file_tools_share_only_the_current_session_artifact_root() {
    let workspace = unique_dir("artifact-read-workspace");
    let sessions = unique_dir("artifact-read-sessions");
    let current = sessions.join("current");
    let other = sessions.join("other");
    fs::create_dir_all(&other).expect("create other artifacts");
    let current_file = current.join("page.md");
    let other_file = other.join("secret.md");
    fs::write(&other_file, "other session secret").expect("write other artifact");
    let tools = ToolRegistry::built_in_with_allowlist_and_writer_lease_and_artifact_root(
        &workspace,
        PermissionProfile::for_mode(PermissionMode::WorkspaceWrite),
        BuiltInToolAllowlist::all(),
        None,
        Some(current.clone()),
        true,
    )
    .expect("artifact-aware registry");
    fs::create_dir_all(&current).expect("create current artifacts after registry");
    fs::write(&current_file, "shared artifact needle").expect("write current artifact");

    let read = content(tools.execute(&call(
        READ_FILE_TOOL_NAME,
        json!({"path": current_file.display().to_string()}),
    )));
    assert_eq!(read["ok"], true);
    assert_eq!(read["data"]["content"], "shared artifact needle");

    let listed = content(tools.execute(&call(
        LIST_FILES_TOOL_NAME,
        json!({"path": current.display().to_string()}),
    )));
    assert_eq!(listed["ok"], true);
    assert!(listed["data"]["entries"].as_array().is_some_and(|entries| {
        entries.iter().any(|entry| {
            entry["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("page.md"))
        })
    }));

    let searched = content(tools.execute(&call(
        SEARCH_TEXT_TOOL_NAME,
        json!({"query": "needle", "path": current.display().to_string()}),
    )));
    assert_eq!(searched["ok"], true);
    assert_eq!(
        searched["data"]["results"][0]["text"],
        "shared artifact needle"
    );

    let other_read = content(tools.execute(&call(
        READ_FILE_TOOL_NAME,
        json!({"path": other_file.display().to_string()}),
    )));
    assert_eq!(other_read["ok"], false);

    let traversed = content(tools.execute(&call(
        READ_FILE_TOOL_NAME,
        json!({
            "path": current
                .join("..")
                .join("other/secret.md")
                .display()
                .to_string()
        }),
    )));
    assert_eq!(traversed["ok"], false);

    let write = content(tools.execute(&call(
        WRITE_FILE_TOOL_NAME,
        json!({
            "path": current.join("blocked.md").display().to_string(),
            "content": "blocked"
        }),
    )));
    assert_eq!(write["ok"], false);
    assert!(!current.join("blocked.md").exists());
}

#[test]
fn list_files_skips_git_and_target() {
    let root = unique_dir("list-root");
    fs::write(root.join("a.txt"), "").expect("write file");
    fs::create_dir(root.join(".git")).expect("create git");
    fs::create_dir(root.join("target")).expect("create target");
    fs::create_dir(root.join("src")).expect("create src");
    fs::write(root.join("src").join("lib.rs"), "").expect("write lib");
    let tools = registry(&root);

    let value =
        content(tools.execute(&call("list_files", json!({"path": ".", "recursive": true}))));

    assert_eq!(value["ok"], true);
    let entries = value["data"]["entries"].as_array().expect("entries");
    let paths = entries
        .iter()
        .map(|entry| entry["path"].as_str().expect("path"))
        .collect::<Vec<_>>();
    assert_eq!(paths, vec!["a.txt", "src", "src/lib.rs"]);
}

#[test]
fn search_text_finds_literal_matches_with_limit() {
    let root = unique_dir("search-root");
    fs::write(root.join("a.txt"), "Alpha\nbeta\nalpha\n").expect("write file");
    let tools = registry(&root);

    let value = content(tools.execute(&call(
        "search_text",
        json!({"query": "alpha", "path": ".", "max_results": 1}),
    )));

    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["truncated"], true);
    assert_eq!(value["data"]["result_truncated"], true);
    let results = value["data"]["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["path"], "a.txt");
    assert_eq!(results[0]["line"], 1);
    assert_eq!(results[0]["text_truncated"], false);
}

#[test]
fn ripgrep_json_parser_reads_match_events_only() {
    let match_frame = json!({
        "type": "match",
        "data": {
            "path": {"text": "src/lib.rs"},
            "lines": {"text": "robot doctor\n"},
            "line_number": 42
        }
    })
    .to_string();
    let begin_frame = json!({
        "type": "begin",
        "data": {"path": {"text": "src/lib.rs"}}
    })
    .to_string();

    assert_eq!(parse_ripgrep_match(&begin_frame).expect("begin"), None);
    assert_eq!(
        parse_ripgrep_match(&match_frame).expect("match"),
        Some(RipgrepMatch {
            path: "src/lib.rs".to_string(),
            line: 42,
            text: "robot doctor\n".to_string(),
        })
    );
}

#[test]
fn search_output_truncates_long_lines() {
    let mut output = SearchOutput::new("needle", ".", false, 10);
    assert!(output.push_match(
        "long.txt".to_string(),
        1,
        format!("needle {}", "x".repeat(MAX_SEARCH_LINE_CHARS + 20)),
    ));

    let value = output.into_value();
    let result = &value["results"][0];
    assert_eq!(value["result_truncated"], false);
    assert_eq!(result["text_truncated"], true);
    assert_eq!(
        result["text"].as_str().expect("text").chars().count(),
        MAX_SEARCH_LINE_CHARS
    );
}

#[test]
fn search_output_marks_result_truncation_for_limits() {
    let mut output = SearchOutput::new("needle", ".", false, 1);
    assert!(output.push_match("a.txt".to_string(), 1, "needle".to_string()));
    assert!(!output.push_match("b.txt".to_string(), 1, "needle".to_string()));

    let value = output.into_value();
    assert_eq!(value["truncated"], true);
    assert_eq!(value["result_truncated"], true);
    assert_eq!(value["results"].as_array().expect("results").len(), 1);
}

#[test]
fn search_output_marks_result_truncation_for_total_budget() {
    let mut output = SearchOutput::new("needle", ".", false, MAX_SEARCH_RESULTS);
    let long = format!("needle {}", "x".repeat(MAX_SEARCH_LINE_CHARS));

    while output.push_match("budget.txt".to_string(), 1, long.clone()) {}

    let value = output.into_value();
    assert_eq!(value["result_truncated"], true);
    assert!(
        value["results"].as_array().expect("results").len() < MAX_SEARCH_RESULTS,
        "total byte budget should truncate before max_results"
    );
}

#[test]
fn search_text_respects_case_sensitivity() {
    let root = unique_dir("search-case-root");
    fs::write(root.join("a.txt"), "Alpha\n").expect("write file");
    let tools = registry(&root);

    let insensitive = content(tools.execute(&call(
        "search_text",
        json!({"query": "alpha", "path": ".", "case_sensitive": false}),
    )));
    let sensitive = content(tools.execute(&call(
        "search_text",
        json!({"query": "alpha", "path": ".", "case_sensitive": true}),
    )));

    assert_eq!(
        insensitive["data"]["results"]
            .as_array()
            .expect("insensitive results")
            .len(),
        1
    );
    assert_eq!(
        sensitive["data"]["results"]
            .as_array()
            .expect("sensitive results")
            .len(),
        0
    );
}

#[test]
fn search_text_skips_generated_directories() {
    let root = unique_dir("search-skip-root");
    fs::write(root.join("keep.txt"), "needle\n").expect("write keep");
    for dir in SEARCH_SKIP_NAMES {
        let skipped = root.join(dir);
        fs::create_dir_all(&skipped).expect("create skipped dir");
        fs::write(skipped.join("skip.txt"), "needle\n").expect("write skipped");
    }
    let tools = registry(&root);

    let value = content(tools.execute(&call(
        "search_text",
        json!({"query": "needle", "path": ".", "max_results": 10}),
    )));

    let paths = value["data"]["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|result| result["path"].as_str().expect("path"))
        .collect::<Vec<_>>();
    assert_eq!(paths, vec!["keep.txt"]);
}

#[test]
fn fallback_search_applies_output_budget() {
    let root = unique_dir("search-fallback-root");
    let path = root.join("long.txt");
    fs::write(
        &path,
        format!("needle {}\n", "x".repeat(MAX_SEARCH_LINE_CHARS + 20)),
    )
    .expect("write long file");
    let tools = BuiltInTools {
        evaluator: PermissionEvaluator::new(
            &root,
            PermissionProfile::for_mode(PermissionMode::WorkspaceWrite),
        )
        .expect("permission evaluator"),
        allowed: BuiltInToolAllowlist::all(),
        writer_lease: None,
    };
    let options = SearchOptions {
        query: "needle",
        case_sensitive: false,
        max_results: 10,
    };

    let output = tools
        .search_text_fallback(&path.canonicalize().expect("canonical path"), &options)
        .expect("fallback search")
        .into_value();

    assert_eq!(output["results"][0]["text_truncated"], true);
}

#[test]
fn edit_file_replaces_unique_match() {
    let root = unique_dir("edit-root");
    fs::write(root.join("note.txt"), "before old after\n").expect("write file");
    let tools = registry_requiring_approval(&root);
    let call = call(
        "edit_file",
        json!({"path": "note.txt", "old_text": "old", "new_text": "new"}),
    );

    let request = approval_request(tools.execute(&call));
    assert!(matches!(request.action, ApprovalAction::FileChanges { .. }));
    assert_eq!(
        fs::read_to_string(root.join("note.txt")).expect("read before approval"),
        "before old after\n"
    );
    let value = content(tools.execute_approved(
        &call,
        &ApprovalDecision::approve(request.id.clone()),
        &request,
    ));

    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["path"], "note.txt");
    assert_eq!(value["data"]["replacements"], 1);
    assert_eq!(value["data"]["created"], false);
    assert_eq!(value["data"]["overwritten"], true);
    assert_eq!(
        fs::read_to_string(root.join("note.txt")).expect("read file"),
        "before new after\n"
    );
}

#[test]
fn edit_file_rejects_invalid_matches_and_targets() {
    let root = unique_dir("edit-invalid-root");
    fs::write(root.join("no-match.txt"), "alpha\n").expect("write no match");
    fs::write(root.join("many.txt"), "alpha alpha\n").expect("write many");
    fs::create_dir(root.join("dir")).expect("create dir");
    let tools = registry(&root);

    let no_match = content(tools.execute(&call(
        "edit_file",
        json!({"path": "no-match.txt", "old_text": "beta", "new_text": "gamma"}),
    )));
    assert_eq!(no_match["ok"], false);
    assert!(
        no_match["error"]
            .as_str()
            .expect("error")
            .contains("found 0")
    );

    let many = content(tools.execute(&call(
        "edit_file",
        json!({"path": "many.txt", "old_text": "alpha", "new_text": "beta"}),
    )));
    assert_eq!(many["ok"], false);
    assert!(many["error"].as_str().expect("error").contains("found 2"));

    let empty = content(tools.execute(&call(
        "edit_file",
        json!({"path": "no-match.txt", "old_text": "", "new_text": "gamma"}),
    )));
    assert_eq!(empty["ok"], false);
    assert!(
        empty["error"]
            .as_str()
            .expect("error")
            .contains("old_text must not be empty")
    );

    let missing = content(tools.execute(&call(
        "edit_file",
        json!({"path": "missing.txt", "old_text": "a", "new_text": "b"}),
    )));
    assert_eq!(missing["ok"], false);
    assert!(
        missing["error"]
            .as_str()
            .expect("error")
            .contains("failed to inspect")
    );

    let directory = content(tools.execute(&call(
        "edit_file",
        json!({"path": "dir", "old_text": "a", "new_text": "b"}),
    )));
    assert_eq!(directory["ok"], false);
    assert!(
        directory["error"]
            .as_str()
            .expect("error")
            .contains("is not a file")
    );

    assert_eq!(
        fs::read_to_string(root.join("no-match.txt")).expect("read no match"),
        "alpha\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("many.txt")).expect("read many"),
        "alpha alpha\n"
    );
}

#[test]
fn write_file_creates_new_file() {
    let root = unique_dir("write-create-root");
    let tools = registry_requiring_approval(&root);
    let call = call(
        "write_file",
        json!({"path": "note.txt", "content": "created\n"}),
    );

    let request = approval_request(tools.execute(&call));
    assert!(matches!(request.action, ApprovalAction::FileChanges { .. }));
    assert!(!root.join("note.txt").exists());
    let value = content(tools.execute_approved(
        &call,
        &ApprovalDecision::approve(request.id.clone()),
        &request,
    ));

    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["path"], "note.txt");
    assert_eq!(value["data"]["replacements"], 0);
    assert_eq!(value["data"]["created"], true);
    assert_eq!(value["data"]["overwritten"], false);
    assert_eq!(
        fs::read_to_string(root.join("note.txt")).expect("read file"),
        "created\n"
    );
}

#[test]
fn workspace_write_auto_approves_file_changes_without_approval() {
    let root = unique_dir("write-auto-approve-root");
    fs::write(root.join("note.txt"), "before old after\n").expect("write file");
    let tools = registry(&root);

    // content() 只在 Completed 时返回，ApprovalRequired 会直接 panic。
    let written = content(tools.execute(&call(
        "write_file",
        json!({"path": "created.txt", "content": "created\n"}),
    )));
    assert_eq!(written["ok"], true);
    assert_eq!(
        fs::read_to_string(root.join("created.txt")).expect("read created"),
        "created\n"
    );

    let edited = content(tools.execute(&call(
        "edit_file",
        json!({"path": "note.txt", "old_text": "old", "new_text": "new"}),
    )));
    assert_eq!(edited["ok"], true);
    assert_eq!(
        fs::read_to_string(root.join("note.txt")).expect("read edited"),
        "before new after\n"
    );

    let patched = content(tools.execute(&patch_call(
        r#"*** Begin Patch
*** Update File: note.txt
@@
-before new after
+AFTER new after
*** End Patch"#,
    )));
    assert_eq!(patched["ok"], true);
    assert_eq!(
        fs::read_to_string(root.join("note.txt")).expect("read patched"),
        "AFTER new after\n"
    );
}

#[test]
fn file_change_approval_returns_diff_summary() {
    let root = unique_dir("write-summary-root");
    let tools = registry_requiring_approval(&root);
    let call = call(
        "write_file",
        json!({"path": "note.txt", "content": "created\n"}),
    );

    let request = approval_request(tools.execute(&call));
    let ApprovalAction::FileChanges { files, diff } = &request.action else {
        panic!("expected file changes approval");
    };
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].operation, FileChangeOperation::Add);
    assert!(diff.contains("+++ note.txt"));
    assert!(diff.contains("+created"));

    let result = completed_result(tools.execute_approved(
        &call,
        &ApprovalDecision::approve(request.id.clone()),
        &request,
    ));
    let summary = result.summary.as_ref().expect("summary");

    assert_eq!(summary.files.len(), 1);
    assert_eq!(summary.files[0].path, "note.txt");
    assert!(summary.diff.as_deref().expect("diff").contains("+created"));
}

#[test]
fn write_file_rejects_default_overwrite_and_preserves_file() {
    let root = unique_dir("write-default-overwrite-root");
    fs::write(root.join("note.txt"), "old\n").expect("write file");
    let tools = registry(&root);

    let value = content(tools.execute(&call(
        "write_file",
        json!({"path": "note.txt", "content": "new\n"}),
    )));

    assert_eq!(value["ok"], false);
    assert!(
        value["error"]
            .as_str()
            .expect("error")
            .contains("already exists")
    );
    assert_eq!(
        fs::read_to_string(root.join("note.txt")).expect("read file"),
        "old\n"
    );
}

#[test]
fn write_file_overwrites_existing_file_when_requested() {
    let root = unique_dir("write-overwrite-root");
    fs::write(root.join("note.txt"), "old\n").expect("write file");
    let tools = registry_requiring_approval(&root);
    let call = call(
        "write_file",
        json!({"path": "note.txt", "content": "new\n", "overwrite": true}),
    );

    let request = approval_request(tools.execute(&call));
    assert!(matches!(request.action, ApprovalAction::FileChanges { .. }));
    assert_eq!(
        fs::read_to_string(root.join("note.txt")).expect("read before approval"),
        "old\n"
    );
    let value = content(tools.execute_approved(
        &call,
        &ApprovalDecision::approve(request.id.clone()),
        &request,
    ));

    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["created"], false);
    assert_eq!(value["data"]["overwritten"], true);
    assert_eq!(
        fs::read_to_string(root.join("note.txt")).expect("read file"),
        "new\n"
    );
}

#[test]
fn file_change_approval_rejects_drift_before_commit() {
    let root = unique_dir("approval-drift-root");
    fs::write(root.join("note.txt"), "old\n").expect("write file");
    let tools = registry_requiring_approval(&root);
    let call = call(
        "edit_file",
        json!({"path": "note.txt", "old_text": "old", "new_text": "new"}),
    );
    let request = approval_request(tools.execute(&call));
    fs::write(root.join("note.txt"), "old\nextra\n").expect("change after approval");

    let value = content(tools.execute_approved(
        &call,
        &ApprovalDecision::approve(request.id.clone()),
        &request,
    ));

    assert_eq!(value["ok"], false);
    assert!(
        value["error"]
            .as_str()
            .expect("error")
            .contains("approval no longer matches")
    );
    assert_eq!(
        fs::read_to_string(root.join("note.txt")).expect("read drifted file"),
        "old\nextra\n"
    );
}

#[test]
fn cancelled_approved_file_change_does_not_commit() {
    let root = unique_dir("cancelled-file-change-root");
    fs::write(root.join("note.txt"), "old\n").expect("write file");
    let tools = registry_requiring_approval(&root);
    let call = call(
        "write_file",
        json!({"path": "note.txt", "content": "new\n", "overwrite": true}),
    );
    let request = approval_request(tools.execute(&call));
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let result = completed_result(tools.execute_approved_with_context(
        &call,
        &ApprovalDecision::approve(request.id.clone()),
        &request,
        ToolExecutionContext { cancellation },
    ));

    assert_eq!(result.error.as_deref(), Some(TOOL_CANCELLED_ERROR));
    assert_eq!(
        fs::read_to_string(root.join("note.txt")).expect("read unchanged file"),
        "old\n"
    );
}

#[test]
fn write_file_rejects_missing_parent_directory() {
    let root = unique_dir("write-missing-parent-root");
    let tools = registry(&root);

    let value = content(tools.execute(&call(
        "write_file",
        json!({"path": "missing/note.txt", "content": "new\n"}),
    )));

    assert_eq!(value["ok"], false);
    assert!(
        value["error"]
            .as_str()
            .expect("error")
            .contains("failed to resolve parent directory")
    );
    assert!(!root.join("missing").exists());
}

#[test]
fn read_only_rejects_file_write_tools() {
    let root = unique_dir("read-only-tools-root");
    fs::write(root.join("note.txt"), "old\n").expect("write file");
    let tools =
        registry_with_permissions(&root, PermissionProfile::for_mode(PermissionMode::ReadOnly));

    let edit = content(tools.execute(&call(
        "edit_file",
        json!({"path": "note.txt", "old_text": "old", "new_text": "new"}),
    )));
    let write = content(tools.execute(&call(
        "write_file",
        json!({"path": "created.txt", "content": "created\n"}),
    )));

    assert_eq!(edit["ok"], false);
    assert!(
        edit["error"]
            .as_str()
            .expect("error")
            .contains("file writes are denied")
    );
    assert_eq!(write["ok"], false);
    assert!(
        write["error"]
            .as_str()
            .expect("error")
            .contains("file writes are denied")
    );
    assert_eq!(
        fs::read_to_string(root.join("note.txt")).expect("read file"),
        "old\n"
    );
    assert!(!root.join("created.txt").exists());
}

#[test]
fn workspace_write_rejects_file_write_tools_outside_workspace() {
    let root = unique_dir("workspace-write-tools-root");
    let outside = outside_path(&root, "outside.txt");
    fs::write(&outside, "old\n").expect("write outside");
    let tools = registry(&root);

    let edit = content(tools.execute(&call(
        "edit_file",
        json!({"path": outside.display().to_string(), "old_text": "old", "new_text": "new"}),
    )));
    let write = content(tools.execute(&call(
        "write_file",
        json!({"path": outside.display().to_string(), "content": "new\n", "overwrite": true}),
    )));

    assert_eq!(edit["ok"], false);
    assert!(
        edit["error"]
            .as_str()
            .expect("error")
            .contains("outside the workspace root")
    );
    assert_eq!(write["ok"], false);
    assert!(
        write["error"]
            .as_str()
            .expect("error")
            .contains("outside the workspace root")
    );
    assert_eq!(fs::read_to_string(outside).expect("read outside"), "old\n");
}

#[test]
fn danger_full_access_can_write_absolute_paths_outside_workspace() {
    let root = unique_dir("danger-write-root");
    let outside = outside_path(&root, "outside-danger.txt");
    let tools = registry_with_permissions(
        &root,
        PermissionProfile::for_mode(PermissionMode::DangerFullAccess),
    );

    let value = content(tools.execute(&call(
        "write_file",
        json!({"path": outside.display().to_string(), "content": "outside\n"}),
    )));

    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["created"], true);
    assert_eq!(value["data"]["overwritten"], false);
    assert_eq!(
        fs::read_to_string(outside).expect("read outside"),
        "outside\n"
    );
}

#[test]
fn apply_patch_adds_updates_and_deletes_files() {
    let root = unique_dir("patch-basic-root");
    fs::write(root.join("update.txt"), "alpha\nbeta\ngamma\n").expect("write update");
    fs::write(root.join("delete.txt"), "delete me\n").expect("write delete");
    let tools = registry_requiring_approval(&root);

    let call = patch_call(
        r#"*** Begin Patch
*** Add File: added.txt
+hello
+world
*** Update File: update.txt
@@
 alpha
-beta
+BETA
 gamma
*** Delete File: delete.txt
*** End Patch"#,
    );
    let request = approval_request(tools.execute(&call));
    assert!(matches!(request.action, ApprovalAction::FileChanges { .. }));
    assert!(!root.join("added.txt").exists());
    assert_eq!(
        fs::read_to_string(root.join("update.txt")).expect("read before approval"),
        "alpha\nbeta\ngamma\n"
    );
    assert!(root.join("delete.txt").exists());
    let value = content(tools.execute_approved(
        &call,
        &ApprovalDecision::approve(request.id.clone()),
        &request,
    ));

    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["changed_files"], 3);
    assert_eq!(
        fs::read_to_string(root.join("added.txt")).expect("read added"),
        "hello\nworld\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("update.txt")).expect("read update"),
        "alpha\nBETA\ngamma\n"
    );
    assert!(!root.join("delete.txt").exists());
    let files = value["data"]["files"].as_array().expect("files");
    assert_eq!(files[0]["operation"], "add");
    assert_eq!(files[1]["operation"], "update");
    assert_eq!(files[1]["replacements"], 1);
    assert_eq!(files[2]["operation"], "delete");
}

#[test]
fn apply_patch_updates_multiple_files_and_hunks() {
    let root = unique_dir("patch-multi-root");
    fs::write(root.join("a.txt"), "one\ntwo\nthree\nfour\n").expect("write a");
    fs::write(root.join("b.txt"), "red\nblue\n").expect("write b");
    let tools = registry_requiring_approval(&root);

    let call = patch_call(
        r#"*** Begin Patch
*** Update File: a.txt
@@
 one
-two
+TWO
 three
@@
 three
-four
+FOUR
*** Update File: b.txt
@@
-red
+RED
 blue
*** End Patch"#,
    );
    let value = approved_content(&tools, &call);

    assert_eq!(value["ok"], true);
    assert_eq!(
        fs::read_to_string(root.join("a.txt")).expect("read a"),
        "one\nTWO\nthree\nFOUR\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("b.txt")).expect("read b"),
        "RED\nblue\n"
    );
    let files = value["data"]["files"].as_array().expect("files");
    assert_eq!(files[0]["replacements"], 2);
    assert_eq!(files[1]["replacements"], 1);
}

#[test]
fn apply_patch_rejects_invalid_targets() {
    let root = unique_dir("patch-invalid-targets-root");
    fs::write(root.join("existing.txt"), "old\n").expect("write existing");
    fs::create_dir(root.join("dir")).expect("create dir");
    fs::write(root.join("binary.bin"), [0xff, 0xfe]).expect("write binary");
    let tools = registry(&root);

    let add_existing = content(tools.execute(&patch_call(
        r#"*** Begin Patch
*** Add File: existing.txt
+new
*** End Patch"#,
    )));
    assert_eq!(add_existing["ok"], false);
    assert!(
        add_existing["error"]
            .as_str()
            .expect("error")
            .contains("already exists")
    );

    let update_missing = content(tools.execute(&patch_call(
        r#"*** Begin Patch
*** Update File: missing.txt
@@
-old
+new
*** End Patch"#,
    )));
    assert_eq!(update_missing["ok"], false);
    assert!(
        update_missing["error"]
            .as_str()
            .expect("error")
            .contains("failed to inspect")
    );

    let delete_missing = content(tools.execute(&patch_call(
        r#"*** Begin Patch
*** Delete File: missing.txt
*** End Patch"#,
    )));
    assert_eq!(delete_missing["ok"], false);
    assert!(
        delete_missing["error"]
            .as_str()
            .expect("error")
            .contains("failed to inspect")
    );

    let update_dir = content(tools.execute(&patch_call(
        r#"*** Begin Patch
*** Update File: dir
@@
-old
+new
*** End Patch"#,
    )));
    assert_eq!(update_dir["ok"], false);
    assert!(
        update_dir["error"]
            .as_str()
            .expect("error")
            .contains("is not a file")
    );

    let update_binary = content(tools.execute(&patch_call(
        r#"*** Begin Patch
*** Update File: binary.bin
@@
-old
+new
*** End Patch"#,
    )));
    assert_eq!(update_binary["ok"], false);
    assert!(
        update_binary["error"]
            .as_str()
            .expect("error")
            .contains("UTF-8")
    );
}

#[test]
fn apply_patch_rejects_invalid_update_hunks() {
    let root = unique_dir("patch-invalid-hunks-root");
    fs::write(root.join("no-match.txt"), "alpha\n").expect("write no match");
    fs::write(root.join("many.txt"), "alpha\nalpha\n").expect("write many");
    fs::write(root.join("same.txt"), "alpha\n").expect("write same");
    let tools = registry(&root);

    let no_match = content(tools.execute(&patch_call(
        r#"*** Begin Patch
*** Update File: no-match.txt
@@
-beta
+gamma
*** End Patch"#,
    )));
    assert_eq!(no_match["ok"], false);
    assert!(
        no_match["error"]
            .as_str()
            .expect("error")
            .contains("found 0")
    );

    let many = content(tools.execute(&patch_call(
        r#"*** Begin Patch
*** Update File: many.txt
@@
-alpha
+beta
*** End Patch"#,
    )));
    assert_eq!(many["ok"], false);
    assert!(many["error"].as_str().expect("error").contains("found 2"));

    let empty = content(tools.execute(&patch_call(
        r#"*** Begin Patch
*** Update File: same.txt
@@
*** End Patch"#,
    )));
    assert_eq!(empty["ok"], false);
    assert!(
        empty["error"]
            .as_str()
            .expect("error")
            .contains("empty hunk")
    );

    let no_old_text = content(tools.execute(&patch_call(
        r#"*** Begin Patch
*** Update File: same.txt
@@
+insert
*** End Patch"#,
    )));
    assert_eq!(no_old_text["ok"], false);
    assert!(
        no_old_text["error"]
            .as_str()
            .expect("error")
            .contains("must include context or removed lines")
    );

    let no_change = content(tools.execute(&patch_call(
        r#"*** Begin Patch
*** Update File: same.txt
@@
 alpha
*** End Patch"#,
    )));
    assert_eq!(no_change["ok"], false);
    assert!(
        no_change["error"]
            .as_str()
            .expect("error")
            .contains("has no changes")
    );
}

#[test]
fn apply_patch_rejects_invalid_patch_syntax() {
    let root = unique_dir("patch-invalid-syntax-root");
    let tools = registry(&root);

    for patch in [
        "*** Add File: a.txt\n+x\n*** End Patch",
        "*** Begin Patch\n*** Add File: a.txt\n+x",
        "*** Begin Patch\n*** Move to: b.txt\n*** End Patch",
        "*** Begin Patch\n*** Rename File: a.txt\n*** End Patch",
        "*** Begin Patch\n*** Add File: a.txt\nx\n*** End Patch",
        "*** Begin Patch\n*** Update File: a.txt\n@@\n?bad\n*** End Patch",
    ] {
        let value = content(tools.execute(&patch_call(patch)));
        assert_eq!(value["ok"], false, "patch should fail: {patch}");
    }
}

#[test]
fn apply_patch_rejects_duplicate_paths_and_preserves_files() {
    let root = unique_dir("patch-duplicate-root");
    fs::write(root.join("same.txt"), "old\n").expect("write same");
    let tools = registry(&root);

    let value = content(tools.execute(&patch_call(
        r#"*** Begin Patch
*** Update File: same.txt
@@
-old
+new
*** Delete File: ./same.txt
*** End Patch"#,
    )));

    assert_eq!(value["ok"], false);
    assert!(
        value["error"]
            .as_str()
            .expect("error")
            .contains("more than once")
    );
    assert_eq!(
        fs::read_to_string(root.join("same.txt")).expect("read same"),
        "old\n"
    );
}

#[test]
fn apply_patch_validation_failure_preserves_all_files() {
    let root = unique_dir("patch-atomic-validation-root");
    fs::write(root.join("first.txt"), "old\n").expect("write first");
    fs::write(root.join("second.txt"), "keep\n").expect("write second");
    let tools = registry(&root);

    let value = content(tools.execute(&patch_call(
        r#"*** Begin Patch
*** Update File: first.txt
@@
-old
+new
*** Update File: second.txt
@@
-missing
+changed
*** End Patch"#,
    )));

    assert_eq!(value["ok"], false);
    assert_eq!(
        fs::read_to_string(root.join("first.txt")).expect("read first"),
        "old\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("second.txt")).expect("read second"),
        "keep\n"
    );
}

#[test]
fn failed_update_install_restores_original_file() {
    let root = unique_dir("patch-rollback-update-root");
    let path = root.join("note.txt");
    fs::write(&path, "old\n").expect("write original");
    let tools = BuiltInTools {
        evaluator: PermissionEvaluator::new(
            &root,
            PermissionProfile::for_mode(PermissionMode::DangerFullAccess),
        )
        .expect("permission evaluator"),
        allowed: BuiltInToolAllowlist::all(),
        writer_lease: None,
    };
    let change = StagedPatchChange {
        path: path.clone(),
        kind: PatchOperationKind::Update,
        content: None,
        permissions: None,
        summary: FileChangeSummary {
            path: "note.txt".to_string(),
            operation: FileChangeOperation::Update,
            replacements: 1,
            created: false,
            overwritten: true,
            deleted: false,
        },
        before: Some("old\n".to_string()),
        after: Some("new\n".to_string()),
        temp_path: Some(root.join("missing-staged-content")),
    };

    let error = commit_patch_changes(vec![change], &tools).expect_err("install must fail");

    assert!(error.contains("failed to replace note.txt"));
    assert_eq!(
        fs::read_to_string(path).expect("read restored file"),
        "old\n"
    );
}

#[test]
fn read_only_rejects_apply_patch() {
    let root = unique_dir("patch-read-only-root");
    let tools =
        registry_with_permissions(&root, PermissionProfile::for_mode(PermissionMode::ReadOnly));

    let value = content(tools.execute(&patch_call(
        r#"*** Begin Patch
*** Add File: created.txt
+content
*** End Patch"#,
    )));

    assert_eq!(value["ok"], false);
    assert!(
        value["error"]
            .as_str()
            .expect("error")
            .contains("file writes are denied")
    );
    assert!(!root.join("created.txt").exists());
}

#[test]
fn workspace_write_rejects_apply_patch_outside_workspace() {
    let root = unique_dir("patch-workspace-write-root");
    let outside = outside_path(&root, "outside-patch.txt");
    let tools = registry(&root);

    let value = content(tools.execute(&patch_call(&format!(
        "*** Begin Patch\n*** Add File: {}\n+outside\n*** End Patch",
        outside.display()
    ))));

    assert_eq!(value["ok"], false);
    assert!(
        value["error"]
            .as_str()
            .expect("error")
            .contains("outside the workspace root")
    );
    assert!(!outside.exists());
}

#[test]
fn danger_full_access_can_apply_patch_outside_workspace() {
    let root = unique_dir("patch-danger-root");
    let outside = outside_path(&root, "outside-patch-danger.txt");
    let tools = registry_with_permissions(
        &root,
        PermissionProfile::for_mode(PermissionMode::DangerFullAccess),
    );

    let value = content(tools.execute(&patch_call(&format!(
        "*** Begin Patch\n*** Add File: {}\n+outside\n*** End Patch",
        outside.display()
    ))));

    assert_eq!(value["ok"], true);
    assert_eq!(
        fs::read_to_string(outside).expect("read outside"),
        "outside\n"
    );
}

#[test]
fn shell_command_runs_in_workspace_and_reports_exit_code() {
    let root = unique_dir("shell-root");
    let tools = registry_with_permissions(
        &root,
        PermissionProfile::for_mode(PermissionMode::DangerFullAccess),
    );

    let result = completed_result(tools.execute(&call(
        "shell_command",
        json!({"command": "pwd && exit 7", "timeout_secs": 5}),
    )));
    let value: Value = serde_json::from_str(&result.content).expect("tool JSON");
    let shell = result
        .summary
        .as_ref()
        .and_then(|summary| summary.shell.as_ref())
        .expect("shell summary");

    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["exit_code"], 7);
    assert_eq!(value["data"]["timed_out"], false);
    assert_eq!(shell.exit_code, Some(7));
    assert!(!shell.timed_out);
    assert!(!shell.stdout_truncated);
    assert!(!shell.stderr_truncated);
    assert_eq!(
        value["data"]["stdout"].as_str().expect("stdout").trim(),
        root.canonicalize()
            .expect("canonical root")
            .display()
            .to_string()
    );
}

#[test]
fn shell_command_times_out() {
    let root = unique_dir("timeout-root");
    let tools = registry_with_permissions(
        &root,
        PermissionProfile::for_mode(PermissionMode::DangerFullAccess),
    );

    let started = Instant::now();
    let value = content(tools.execute(&call(
        "shell_command",
        json!({"command": "sleep 2", "timeout_secs": 1}),
    )));

    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["timed_out"], true);
    assert!(
        started.elapsed() < Duration::from_millis(1800),
        "timeout must terminate descendants instead of waiting for inherited pipes"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn shell_command_cancellation_kills_process_group() {
    let root = unique_dir("shell-cancel-root");
    let tools = registry_with_permissions(
        &root,
        PermissionProfile::for_mode(PermissionMode::DangerFullAccess),
    );
    let cancellation = CancellationToken::new();
    let context = ToolExecutionContext {
        cancellation: cancellation.clone(),
    };
    let execution_tools = tools.clone();
    let execution = tokio::spawn(async move {
        execution_tools
            .execute_with_context(
                call(
                    "shell_command",
                    json!({
                        "command": "printf started > started.txt; sleep 1; printf late > late.txt",
                        "timeout_secs": 5
                    }),
                ),
                context,
            )
            .await
    });

    for _ in 0..100 {
        if root.join("started.txt").is_file() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(root.join("started.txt").is_file(), "shell did not start");

    let cancelled_at = Instant::now();
    cancellation.cancel();
    let execution = tokio::time::timeout(Duration::from_millis(500), execution)
        .await
        .expect("cancelled shell must return promptly")
        .expect("shell execution task");
    let ToolExecution::Completed(result) = execution else {
        panic!("cancelled shell must complete with an error result");
    };
    assert_eq!(result.error.as_deref(), Some(TOOL_CANCELLED_ERROR));
    assert!(cancelled_at.elapsed() < Duration::from_millis(500));

    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        !root.join("late.txt").exists(),
        "a descendant survived cancellation and wrote after the turn stopped"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn shell_timeout_covers_background_process_pipes() {
    let root = unique_dir("shell-background-timeout-root");
    let cancellation = CancellationToken::new();
    let started = Instant::now();

    let (_, summary) = run_shell_command(
        &root,
        "(sleep 1; printf late > late.txt) &",
        Duration::from_millis(100),
        &cancellation,
    )
    .await
    .expect("background command must be terminated at the total deadline");

    assert!(summary.timed_out);
    assert!(started.elapsed() < Duration::from_millis(600));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        !root.join("late.txt").exists(),
        "background descendant survived the command deadline"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_cancelled_shell_future_kills_process_group() {
    let root = unique_dir("shell-drop-cancel-root");
    let task_root = root.clone();
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let execution = tokio::spawn(async move {
        run_shell_command(
            &task_root,
            "printf started > started.txt; sleep 1; printf late > late.txt",
            Duration::from_secs(5),
            &task_cancellation,
        )
        .await
    });

    for _ in 0..100 {
        if root.join("started.txt").is_file() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(root.join("started.txt").is_file(), "shell did not start");

    cancellation.cancel();
    execution.abort();
    let _ = execution.await;

    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        !root.join("late.txt").exists(),
        "dropping the tool future left a descendant process running"
    );
}

#[test]
fn shell_command_requires_approval_in_workspace_write() {
    let root = unique_dir("shell-approval-root");
    let tools = registry(&root);

    let request = approval_request(tools.execute(&call(
        "shell_command",
        json!({"command": "pwd", "timeout_secs": 5}),
    )));

    assert_eq!(request.id, "approval-call_1");
}

#[test]
fn shell_command_runs_after_matching_approval() {
    let root = unique_dir("shell-approved-root");
    let tools = registry(&root);
    let call = call(
        "shell_command",
        json!({"command": "pwd && exit 3", "timeout_secs": 5}),
    );
    let request = approval_request(tools.execute(&call));

    let value = content(tools.execute_approved(
        &call,
        &ApprovalDecision::approve(request.id.clone()),
        &request,
    ));

    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["exit_code"], 3);
}

#[test]
fn shell_command_rejects_denied_approval() {
    let root = unique_dir("shell-denied-root");
    let tools = registry(&root);
    let call = call(
        "shell_command",
        json!({"command": "pwd", "timeout_secs": 5}),
    );
    let request = approval_request(tools.execute(&call));

    let value = content(tools.execute_approved(
        &call,
        &ApprovalDecision::deny(request.id.clone()),
        &request,
    ));

    assert_eq!(value["ok"], false);
    assert_eq!(value["error"], "shell command approval denied");
}

#[test]
fn shell_command_can_be_denied_by_policy() {
    let root = unique_dir("shell-policy-denied-root");
    let tools = registry_with_permissions(
        &root,
        PermissionProfile {
            mode: PermissionMode::WorkspaceWrite,
            shell: ShellPolicy::Deny,
        },
    );

    let value = content(tools.execute(&call(
        "shell_command",
        json!({"command": "pwd", "timeout_secs": 5}),
    )));

    assert_eq!(value["ok"], false);
    assert!(
        value["error"]
            .as_str()
            .expect("error")
            .contains("shell commands are denied")
    );
}

#[test]
fn danger_full_access_can_read_absolute_paths_outside_workspace() {
    let root = unique_dir("danger-read-root");
    let outside = root
        .parent()
        .expect("parent")
        .join("outside-morrow-tools-danger.txt");
    fs::write(&outside, "secret").expect("write outside");
    let tools = registry_with_permissions(
        &root,
        PermissionProfile::for_mode(PermissionMode::DangerFullAccess),
    );

    let value = content(tools.execute(&call(
        "read_file",
        json!({"path": outside.display().to_string()}),
    )));

    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["content"], "secret");
}

#[test]
fn all_tool_schemas_describe_every_property() {
    let definitions = built_in_definitions()
        .into_iter()
        .chain(std::iter::once(delegate_task_definition()))
        .chain(subagent_lifecycle_definitions())
        .chain(std::iter::once(web_fetch::web_fetch_definition()))
        .collect::<Vec<_>>();

    assert_eq!(definitions.len(), 14);
    for definition in &definitions {
        let name = definition.function.name.as_str();
        let properties = definition.function.parameters["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{name} schema has no properties object"));
        assert!(
            !properties.is_empty(),
            "{name} schema must declare its properties"
        );
        for (property, schema) in properties {
            let description = schema["description"].as_str().unwrap_or_default();
            assert!(
                !description.trim().is_empty(),
                "{name}.{property} is missing a description"
            );
        }
    }

    let edit_file = definitions
        .iter()
        .find(|definition| definition.function.name == EDIT_FILE_TOOL_NAME)
        .expect("edit_file definition");
    assert!(edit_file.function.description.contains("apply_patch"));
    let apply_patch = definitions
        .iter()
        .find(|definition| definition.function.name == APPLY_PATCH_TOOL_NAME)
        .expect("apply_patch definition");
    assert!(apply_patch.function.description.contains("edit_file"));
}

#[test]
fn invalid_arguments_error_echoes_required_fields_and_schema() {
    let root = unique_dir("invalid-args-root");
    let tools = registry(&root);

    let result = completed_result(tools.execute(&call("read_file", json!({"path": 42}))));
    let error = result.error.as_deref().expect("error");

    assert!(
        error.contains("invalid arguments for tool read_file"),
        "{error}"
    );
    assert!(error.contains("required: [path]"), "{error}");
    assert!(error.contains("path: string"), "{error}");
    assert!(error.contains("start_line: integer"), "{error}");
    assert!(error.contains("max_lines: integer"), "{error}");
    assert!(error.len() <= MAX_ERROR_MESSAGE_CHARS + 3, "{error}");
}

#[test]
fn unknown_tool_error_lists_available_tools_and_suggestions() {
    let root = unique_dir("unknown-tool-root");
    let tools = registry(&root);

    let result = completed_result(tools.execute(&call("read_fil", json!({}))));
    let error = result.error.as_deref().expect("error");

    assert!(error.contains("unknown tool \"read_fil\""), "{error}");
    assert!(error.contains("Available tools:"), "{error}");
    assert!(error.contains("write_file"), "{error}");
    assert!(error.contains("Did you mean: read_file"), "{error}");
}

#[tokio::test]
async fn tool_filter_trims_built_in_tool_definitions() {
    let root = unique_dir("tool-filter-root");
    let cache = McpToolCache::new();
    let names = |build: &ToolRegistryBuild| {
        let mut names = build
            .registry
            .definitions()
            .into_iter()
            .map(|definition| definition.function.name)
            .collect::<Vec<_>>();
        names.sort();
        names
    };

    let all =
        ToolRegistry::with_mcp_cache_and_writer_lease_and_artifact_root_and_tool_filter_async(
            &root,
            PermissionProfile::for_mode(PermissionMode::WorkspaceWrite),
            &[],
            &cache,
            None,
            None,
            &ToolsConfig::default(),
            true,
        )
        .await
        .expect("registry");
    assert!(names(&all).contains(&SHELL_COMMAND_TOOL_NAME.to_string()));

    let denied =
        ToolRegistry::with_mcp_cache_and_writer_lease_and_artifact_root_and_tool_filter_async(
            &root,
            PermissionProfile::for_mode(PermissionMode::WorkspaceWrite),
            &[],
            &cache,
            None,
            None,
            &ToolsConfig {
                allow: Vec::new(),
                deny: vec!["shell_command".to_string()],
            },
            true,
        )
        .await
        .expect("registry");
    let denied_names = names(&denied);
    assert!(!denied_names.contains(&SHELL_COMMAND_TOOL_NAME.to_string()));
    assert!(denied_names.contains(&READ_FILE_TOOL_NAME.to_string()));

    let allow_only =
        ToolRegistry::with_mcp_cache_and_writer_lease_and_artifact_root_and_tool_filter_async(
            &root,
            PermissionProfile::for_mode(PermissionMode::WorkspaceWrite),
            &[],
            &cache,
            None,
            None,
            &ToolsConfig {
                allow: vec!["read_file".to_string()],
                deny: Vec::new(),
            },
            true,
        )
        .await
        .expect("registry");
    assert_eq!(names(&allow_only), [READ_FILE_TOOL_NAME.to_string()]);

    let deny_wins =
        ToolRegistry::with_mcp_cache_and_writer_lease_and_artifact_root_and_tool_filter_async(
            &root,
            PermissionProfile::for_mode(PermissionMode::WorkspaceWrite),
            &[],
            &cache,
            None,
            None,
            &ToolsConfig {
                allow: vec!["read_file".to_string(), "shell_command".to_string()],
                deny: vec!["shell_command".to_string()],
            },
            true,
        )
        .await
        .expect("registry");
    assert_eq!(names(&deny_wins), [READ_FILE_TOOL_NAME.to_string()]);
}
