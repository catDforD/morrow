use super::*;
use agent_protocol::{
    ModelInvocation, PermissionMode, PermissionProfile, ReasoningLevel, ShellPolicy,
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn unique_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("morrow-hooks-{name}-{stamp}"));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

fn write_config(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("create config parent");
    fs::write(path, body).expect("write config");
}

fn context(workspace_root: &Path) -> MiddlewareExecutionContext {
    MiddlewareExecutionContext {
        invocation_id: Some("invocation-1".to_string()),
        session: "default".to_string(),
        workspace_root: workspace_root.to_path_buf(),
        turn_index: 0,
        operation_id: None,
        turn_id: None,
        model: ModelInvocation {
            provider_id: "test".to_string(),
            provider_name: "Test".to_string(),
            model_id: "model".to_string(),
            model_name: "Model".to_string(),
            reasoning: ReasoningLevel::Off,
        },
        permissions: PermissionProfile {
            mode: PermissionMode::ReadOnly,
            shell: ShellPolicy::Deny,
        },
        agent_scope: MiddlewareAgentScope::Main,
        cancellation: agent_core::CancellationToken::new(),
    }
}

fn command_hook(event: HookEvent, shell: impl Into<String>) -> CommandHook {
    CommandHook {
        definition: HookDefinition {
            id: "command".to_string(),
            event,
            command: vec!["/bin/sh".to_string(), "-c".to_string(), shell.into()],
            timeout_secs: 10,
            failure_mode: HookFailureMode::Open,
            tool_names: None,
            agent_scopes: None,
        },
        source: MiddlewareSource::UserCommand,
        context_budget: Arc::new(AtomicUsize::new(0)),
    }
}

#[test]
fn config_merges_user_before_project_and_rejects_duplicate_ids_per_file() {
    let home = unique_dir("merge-home");
    let workspace = unique_dir("merge-workspace");
    let manager = HookManager::new(&home, &workspace);
    write_config(
        &manager.user_config_path(),
        "schema_version = 1\n[[hooks]]\nid = \"user\"\nevent = \"before_prompt\"\ncommand = [\"true\"]\n",
    );
    write_config(
        &manager.project_config_path(),
        "schema_version = 1\n[[hooks]]\nid = \"project\"\nevent = \"before_tool\"\ncommand = [\"true\"]\ntool_names = [\"shell_command\"]\n",
    );
    let settings = manager.settings().expect("settings");
    assert_eq!(settings.hooks[0].id, "user");
    assert_eq!(settings.hooks[1].id, "project");
    assert!(settings.hooks[0].active);
    assert!(!settings.hooks[1].active);

    write_config(
        &manager.user_config_path(),
        "schema_version = 1\n[[hooks]]\nid = \"same\"\nevent = \"before_prompt\"\ncommand = [\"true\"]\n[[hooks]]\nid = \"same\"\nevent = \"after_tool\"\ncommand = [\"true\"]\n",
    );
    assert!(matches!(
        manager.settings(),
        Err(HookError::InvalidConfig { .. })
    ));
}

#[test]
fn config_validates_exact_matchers_timeouts_and_unknown_fields() {
    let home = unique_dir("validate-home");
    let workspace = unique_dir("validate-workspace");
    let manager = HookManager::new(&home, &workspace);
    for body in [
        "schema_version = 1\n[[hooks]]\nid = \"bad\"\nevent = \"before_prompt\"\ncommand = [\"true\"]\ntimeout_secs = 0\n",
        "schema_version = 1\n[[hooks]]\nid = \"bad\"\nevent = \"before_prompt\"\ncommand = [\"true\"]\ntool_names = []\n",
        "schema_version = 1\n[[hooks]]\nid = \"bad\"\nevent = \"before_tool\"\ncommand = [\"true\"]\ntool_names = [\"shell_command\", \"shell_command\"]\n",
        "schema_version = 1\n[[hooks]]\nid = \"bad\"\nevent = \"before_prompt\"\ncommand = [\"true\"]\nunknown = true\n",
        "schema_version = 1\n[[hooks]]\nid = \"bad\"\nevent = \"after_turn\"\ncommand = [\"true\"]\ntool_names = [\"shell_command\"]\n",
    ] {
        write_config(&manager.user_config_path(), body);
        assert!(manager.settings().is_err(), "config should fail: {body}");
    }
}

#[test]
fn project_fingerprint_tracks_definition_order_but_not_script_contents() {
    let home = unique_dir("fingerprint-home");
    let workspace = unique_dir("fingerprint-workspace");
    let manager = HookManager::new(&home, &workspace);
    let script = workspace.join("hook.sh");
    fs::write(&script, "one").expect("script");
    let first = format!(
        "schema_version = 1\n[[hooks]]\nid = \"a\"\nevent = \"before_prompt\"\ncommand = [{:?}]\n[[hooks]]\nid = \"b\"\nevent = \"pre_compact\"\ncommand = [\"true\"]\n",
        script.to_string_lossy()
    );
    write_config(&manager.project_config_path(), &first);
    let first_fingerprint = manager
        .settings()
        .expect("first")
        .project_fingerprint
        .expect("fingerprint");
    fs::write(&script, "two").expect("change script");
    assert_eq!(
        manager
            .settings()
            .expect("after script change")
            .project_fingerprint
            .as_deref(),
        Some(first_fingerprint.as_str())
    );
    let reordered = first
        .replace("id = \"a\"", "id = \"temporary\"")
        .replace("id = \"b\"", "id = \"a\"")
        .replace("id = \"temporary\"", "id = \"b\"");
    write_config(&manager.project_config_path(), &reordered);
    assert_ne!(
        manager
            .settings()
            .expect("reordered")
            .project_fingerprint
            .as_deref(),
        Some(first_fingerprint.as_str())
    );
}

#[test]
fn trust_and_revoke_are_scoped_to_the_workspace_and_exact_fingerprint() {
    let home = unique_dir("trust-home");
    let workspace = unique_dir("trust-workspace");
    let manager = HookManager::new(&home, &workspace);
    write_config(
        &manager.project_config_path(),
        "schema_version = 1\n[[hooks]]\nid = \"project\"\nevent = \"before_prompt\"\ncommand = [\"true\"]\n",
    );
    assert!(!manager.settings().expect("untrusted").project_trusted);
    assert!(manager.trust_project().expect("trust").project_trusted);
    let other_host = HookManager::new(unique_dir("other-host-home"), &workspace);
    assert!(
        !other_host
            .settings()
            .expect("other host settings")
            .project_trusted,
        "trust records must remain isolated to the execution host"
    );
    write_config(
        &manager.project_config_path(),
        "schema_version = 1\n[[hooks]]\nid = \"project\"\nevent = \"before_prompt\"\ncommand = [\"false\"]\n",
    );
    assert!(!manager.settings().expect("changed").project_trusted);
    manager.trust_project().expect("retrust");
    assert!(!manager.revoke_project().expect("revoke").project_trusted);
}

#[tokio::test]
async fn untrusted_project_hooks_never_start_and_trusted_hooks_do() {
    let home = unique_dir("untrusted-home");
    let workspace = unique_dir("untrusted-workspace");
    let manager = HookManager::new(&home, &workspace);
    let marker = workspace.join("project-hook-ran");
    let script = workspace.join("project-hook.sh");
    fs::write(
            &script,
            format!(
                "printf ran > '{}'\nprintf '%s' '{{\"decision\":\"continue\",\"additional_context\":[]}}'\n",
                marker.display()
            ),
        )
        .expect("write script");
    write_config(
        &manager.project_config_path(),
        &format!(
            "schema_version = 1\n[[hooks]]\nid = \"project\"\nevent = \"before_prompt\"\ncommand = [\"/bin/sh\", {:?}]\n",
            script.to_string_lossy()
        ),
    );

    let snapshot = manager.load_snapshot().expect("untrusted snapshot");
    let run = snapshot
        .registry()
        .runtime()
        .run_before_prompt(BeforePromptInput {
            context: context(&workspace),
            prompt: "hello".to_string(),
        })
        .await;
    assert!(run.events.is_empty());
    assert!(!marker.exists());

    manager.trust_project().expect("trust project");
    let snapshot = manager.load_snapshot().expect("trusted snapshot");
    let run = snapshot
        .registry()
        .runtime()
        .run_before_prompt(BeforePromptInput {
            context: context(&workspace),
            prompt: "hello".to_string(),
        })
        .await;
    assert_eq!(run.events.len(), 2);
    assert_eq!(fs::read_to_string(marker).expect("marker"), "ran");
}

#[tokio::test]
async fn command_hook_failure_modes_default_open_and_allow_closed_override() {
    let home = unique_dir("failure-mode-home");
    let workspace = unique_dir("failure-mode-workspace");
    let manager = HookManager::new(&home, &workspace);
    write_config(
        &manager.user_config_path(),
        "schema_version = 1\n[[hooks]]\nid = \"open\"\nevent = \"before_prompt\"\ncommand = [\"/bin/sh\", \"-c\", \"printf invalid\"]\n[[hooks]]\nid = \"closed\"\nevent = \"before_prompt\"\ncommand = [\"/bin/sh\", \"-c\", \"printf invalid\"]\nfailure_mode = \"closed\"\n",
    );

    let run = manager
        .load_snapshot()
        .expect("snapshot")
        .registry()
        .runtime()
        .run_before_prompt(BeforePromptInput {
            context: context(&workspace),
            prompt: "hello".to_string(),
        })
        .await;

    assert!(run.denied());
    assert!(matches!(
        &run.events[1],
        agent_protocol::AgentEvent::MiddlewareFinished(invocation)
            if invocation.outcome == agent_protocol::MiddlewareOutcome::FailedOpen
    ));
    assert!(matches!(
        &run.events[3],
        agent_protocol::AgentEvent::MiddlewareFinished(invocation)
            if invocation.outcome == agent_protocol::MiddlewareOutcome::FailedClosed
    ));
}

#[tokio::test]
async fn loaded_snapshot_is_stable_when_configuration_changes() {
    let home = unique_dir("snapshot-home");
    let workspace = unique_dir("snapshot-workspace");
    let manager = HookManager::new(&home, &workspace);
    let marker = workspace.join("snapshot-marker");
    let old_script = workspace.join("old-hook.sh");
    let new_script = workspace.join("new-hook.sh");
    fs::write(
            &old_script,
            format!(
                "printf old >> '{}'\nprintf '%s' '{{\"decision\":\"continue\",\"additional_context\":[{{\"content\":\"old context\"}}]}}'\n",
                marker.display()
            ),
        )
        .expect("old script");
    fs::write(
            &new_script,
            format!(
                "printf new >> '{}'\nprintf '%s' '{{\"decision\":\"continue\",\"additional_context\":[\"new context\"]}}'\n",
                marker.display()
            ),
        )
        .expect("new script");
    let config = |script: &Path| {
        format!(
            "schema_version = 1\n[[hooks]]\nid = \"snapshot\"\nevent = \"before_prompt\"\ncommand = [\"/bin/sh\", {:?}]\n",
            script.to_string_lossy()
        )
    };
    write_config(&manager.user_config_path(), &config(&old_script));
    let old_snapshot = manager.load_snapshot().expect("old snapshot");
    write_config(&manager.user_config_path(), &config(&new_script));

    let old_run = old_snapshot
        .registry()
        .runtime()
        .run_before_prompt(BeforePromptInput {
            context: context(&workspace),
            prompt: "hello".to_string(),
        })
        .await;
    assert_eq!(old_run.context[0].content, "old context");
    assert_eq!(fs::read_to_string(&marker).expect("old marker"), "old");

    let new_run = manager
        .load_snapshot()
        .expect("new snapshot")
        .registry()
        .runtime()
        .run_before_prompt(BeforePromptInput {
            context: context(&workspace),
            prompt: "hello".to_string(),
        })
        .await;
    assert_eq!(new_run.context[0].content, "new context");
    assert_eq!(fs::read_to_string(marker).expect("new marker"), "oldnew");
}

#[tokio::test]
async fn command_receives_json_cwd_and_inherited_environment() {
    let workspace = unique_dir("command");
    let output_path = workspace.join("hook-input.json");
    let hook = CommandHook {
        definition: HookDefinition {
            id: "command".to_string(),
            event: HookEvent::BeforePrompt,
            command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!(
                    "test -n \"$PATH\" && test \"$PWD\" = {:?} && tee {:?} >/dev/null && printf '%s' '{{\"decision\":\"continue\",\"reason\":null,\"additional_context\":[\"policy\"]}}'",
                    workspace.to_string_lossy(),
                    output_path.to_string_lossy(),
                ),
            ],
            timeout_secs: 10,
            failure_mode: HookFailureMode::Open,
            tool_names: None,
            agent_scopes: None,
        },
        source: MiddlewareSource::UserCommand,
        context_budget: Arc::new(AtomicUsize::new(0)),
    };
    let result = hook
        .invoke(context(&workspace), json!({ "prompt": "hello" }))
        .await
        .expect("invoke");
    assert_eq!(result.additional_context[0].content, "policy");
    let input: Value =
        serde_json::from_slice(&fs::read(output_path).expect("input")).expect("input JSON");
    assert_eq!(input["schema_version"], 1);
    assert_eq!(input["invocation_id"], "invocation-1");
    assert_eq!(input["event"], "before_prompt");
    assert_eq!(input["payload"]["prompt"], "hello");
}

#[tokio::test]
async fn command_rejects_invalid_json_decisions_and_excess_context() {
    let workspace = unique_dir("invalid-output");
    let invalid_json = command_hook(HookEvent::BeforePrompt, "printf nope");
    let error = invalid_json
        .invoke(context(&workspace), json!({ "prompt": "hello" }))
        .await
        .expect_err("invalid JSON");
    assert!(error.to_string().contains("valid JSON"));

    let invalid_decision = command_hook(
        HookEvent::BeforePrompt,
        "printf '%s' '{\"decision\":\"approve\",\"additional_context\":[]}'",
    );
    let error = invalid_decision
        .invoke(context(&workspace), json!({ "prompt": "hello" }))
        .await
        .expect_err("approve is not valid before prompt");
    assert!(error.to_string().contains("invalid for before_prompt"));

    let budget = command_hook(HookEvent::BeforePrompt, "true");
    let error = budget
        .reserve_context(&[ContextBlock::new(
            "x".repeat(MAX_OPERATION_CONTEXT_BYTES + 1),
        )])
        .expect_err("context over limit");
    assert!(error.to_string().contains("context exceeds"));
}

#[tokio::test]
async fn command_failures_cover_exit_timeout_and_output_limits() {
    let workspace = unique_dir("command-failures");
    let context = context(&workspace);

    let nonzero = run_hook_command(
        &[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo detail >&2; exit 7".to_string(),
        ],
        &workspace,
        10,
        &context,
        b"{}".to_vec(),
    )
    .await
    .expect_err("nonzero exit");
    assert!(nonzero.to_string().contains("detail"));

    let started = Instant::now();
    let timeout = run_hook_command(
        &[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 30".to_string(),
        ],
        &workspace,
        1,
        &context,
        b"{}".to_vec(),
    )
    .await
    .expect_err("timeout");
    assert!(timeout.to_string().contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(3));

    let stdout = run_hook_command(
        &[
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("head -c {} /dev/zero", MAX_HOOK_STDOUT_BYTES + 1),
        ],
        &workspace,
        10,
        &context,
        b"{}".to_vec(),
    )
    .await
    .expect_err("stdout limit");
    assert!(stdout.to_string().contains("stdout exceeds"));

    let stderr = run_hook_command(
        &[
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("head -c {} /dev/zero >&2", MAX_HOOK_STDERR_BYTES + 1),
        ],
        &workspace,
        10,
        &context,
        b"{}".to_vec(),
    )
    .await
    .expect_err("stderr limit");
    assert!(stderr.to_string().contains("stderr exceeds"));
}

#[test]
fn matcher_uses_exact_tool_names_and_agent_scopes() {
    let workspace = unique_dir("matcher");
    let mut hook = command_hook(HookEvent::BeforeTool, "true");
    hook.definition.tool_names = Some(vec!["shell_command".to_string()]);
    hook.definition.agent_scopes = Some(vec![MiddlewareAgentScope::Main]);
    let main = context(&workspace);
    assert!(hook.matches(&main, Some("shell_command")));
    assert!(!hook.matches(&main, Some("shell")));
    let mut delegated = main;
    delegated.agent_scope = MiddlewareAgentScope::DelegatedSubagent;
    assert!(!hook.matches(&delegated, Some("shell_command")));
}

#[tokio::test]
async fn after_turn_hook_runs_on_agent_chain_and_receives_turn_summary() {
    let home = unique_dir("after-turn-home");
    let workspace = unique_dir("after-turn-workspace");
    let manager = HookManager::new(&home, &workspace);
    let input_path = workspace.join("after-turn-input.json");
    let shell = format!(
        "tee {:?} >/dev/null; printf '%s' '{{\"decision\":\"continue\",\"additional_context\":[\"cargo test failed\"]}}'",
        input_path.to_string_lossy(),
    );
    write_config(
        &manager.user_config_path(),
        &format!(
            "schema_version = 1\n[[hooks]]\nid = \"verify\"\nevent = \"after_turn\"\ncommand = [\"/bin/sh\", \"-c\", {:?}]\n",
            shell
        ),
    );

    let snapshot = manager.load_snapshot().expect("snapshot");
    let run = snapshot
        .registry()
        .agent()
        .run_after_turn(AfterTurnInput {
            context: context(&workspace),
            final_text: "done".to_string(),
            tool_call_count: 2,
            turn_message_count: 5,
            tool_names: vec!["shell_command".to_string()],
        })
        .await;

    assert!(run.continue_requested);
    assert!(run.fail_reasons.is_empty());
    assert_eq!(run.context.len(), 1);
    assert_eq!(run.context[0].content, "cargo test failed");
    assert_eq!(
        run.context[0].stage,
        agent_protocol::MiddlewareStage::AfterTurn
    );
    assert_eq!(run.events.len(), 2);

    let input: Value =
        serde_json::from_slice(&fs::read(input_path).expect("input")).expect("input JSON");
    assert_eq!(input["event"], "after_turn");
    assert_eq!(input["payload"]["final_text"], "done");
    assert_eq!(input["payload"]["tool_call_count"], 2);
    assert_eq!(input["payload"]["turn_message_count"], 5);
    assert_eq!(input["payload"]["tool_names"], json!(["shell_command"]));

    // after_turn 挂 agent 链而不是 runtime 链：before_prompt 不应触发任何 hook。
    let runtime_run = snapshot
        .registry()
        .runtime()
        .run_before_prompt(BeforePromptInput {
            context: context(&workspace),
            prompt: "hello".to_string(),
        })
        .await;
    assert!(runtime_run.events.is_empty());
}

#[tokio::test]
async fn after_turn_decisions_are_mapped_and_validated() {
    let workspace = unique_dir("after-turn-decisions");
    let payload = || {
        json!({
            "final_text": "done",
            "tool_call_count": 0,
            "turn_message_count": 1,
            "tool_names": [],
        })
    };

    let fail = command_hook(
        HookEvent::AfterTurn,
        "printf '%s' '{\"decision\":\"fail\",\"reason\":\"tests red\"}'",
    );
    let result = fail
        .invoke(context(&workspace), payload())
        .await
        .expect("fail decision");
    assert!(matches!(
        crate::protocol::after_turn_output(result),
        AfterTurnOutput::Fail { reason } if reason == "tests red"
    ));

    let complete = command_hook(
        HookEvent::AfterTurn,
        "printf '%s' '{\"decision\":\"complete\"}'",
    );
    let result = complete
        .invoke(context(&workspace), payload())
        .await
        .expect("complete decision");
    assert!(matches!(
        crate::protocol::after_turn_output(result),
        AfterTurnOutput::Complete
    ));

    let deny = command_hook(
        HookEvent::AfterTurn,
        "printf '%s' '{\"decision\":\"deny\"}'",
    );
    let error = deny
        .invoke(context(&workspace), payload())
        .await
        .expect_err("deny is not valid after turn");
    assert!(error.to_string().contains("invalid for after_turn"));
}

#[tokio::test]
async fn hook_that_never_reads_stdin_still_returns_its_output() {
    let workspace = unique_dir("epipe");
    let hook = command_hook(
        HookEvent::AfterTurn,
        "printf '%s' '{\"decision\":\"complete\"}'",
    );
    // printf 不读 stdin 并立即退出，重复触发以覆盖 stdin 写入与进程退出的竞争。
    for _ in 0..20 {
        let result = hook
            .invoke(
                context(&workspace),
                json!({
                    "final_text": "done",
                    "tool_call_count": 0,
                    "turn_message_count": 1,
                    "tool_names": [],
                }),
            )
            .await
            .expect("hook output must survive the stdin EPIPE race");
        assert!(matches!(
            crate::protocol::after_turn_output(result),
            AfterTurnOutput::Complete
        ));
    }
}

#[tokio::test]
async fn cancellation_terminates_a_running_command() {
    let workspace = unique_dir("cancel");
    let child_marker = workspace.join("child-survived");
    let context = context(&workspace);
    let cancellation = context.cancellation.clone();
    let started = Instant::now();
    let argv = [
        "/bin/sh".to_string(),
        "-c".to_string(),
        format!("(sleep 1; touch '{}') & sleep 30", child_marker.display()),
    ];
    let command = run_hook_command(&argv, &workspace, 30, &context, b"{}".to_vec());
    tokio::pin!(command);
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(50)) => cancellation.cancel(),
        _ = &mut command => panic!("command completed before cancellation"),
    }
    let error = command.await.expect_err("cancelled");
    assert!(error.to_string().contains("cancelled"));
    assert!(started.elapsed() < Duration::from_secs(2));
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert!(
        !child_marker.exists(),
        "cancellation must terminate the Unix process group"
    );
    context.cancellation.cancel();
}
