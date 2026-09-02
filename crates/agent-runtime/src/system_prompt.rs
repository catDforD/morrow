use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpInspectionTool {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpInspection {
    pub tools: Vec<McpInspectionTool>,
    pub diagnostics: Vec<String>,
}

pub async fn inspect_mcp_servers(
    workspace_root: &Path,
    servers: &[McpServerConfig],
) -> McpInspection {
    let cache = McpToolCache::new();
    let discovery = agent_tools::mcp::discover_tools(workspace_root, servers, &cache).await;
    let mut tools = discovery
        .tools
        .into_iter()
        .flat_map(|provider| provider.definitions())
        .map(|definition| McpInspectionTool {
            name: definition.function.name,
            description: definition.function.description,
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    cache.clear().await;

    McpInspection {
        tools,
        diagnostics: discovery.diagnostics,
    }
}

/// 模型当次实际看到的完整 system prompt：turn base（配置层 base + 每轮重读的
/// AGENTS.md + `<environment>` 块，见 `assembled_turn_system_prompt`）+ subagent guidance。
/// `prepare_session_turn_with_middleware_context` 把它写入 `TurnStarted` fact，
/// `run_agent_turn_inner` 用它发起模型请求，两边共用此函数与同一份 turn base，
/// 保证日志与模型所见一致。
pub(crate) fn effective_turn_system_prompt(
    system_prompt: &str,
    subagent_delegation: bool,
    persistent_controller: bool,
) -> String {
    if !subagent_delegation {
        return system_prompt.to_string();
    }
    let guidance = if persistent_controller {
        format!("{PARENT_SUBAGENT_GUIDANCE}\n\n{PERSISTENT_SUBAGENT_GUIDANCE}")
    } else {
        PARENT_SUBAGENT_GUIDANCE.to_string()
    };
    format!("{system_prompt}\n\n{guidance}")
}

const ENVIRONMENT_GIT_TIMEOUT: Duration = Duration::from_secs(2);

/// 每轮 fresh 的 turn base prompt：`context.system_prompt`（配置层 base）加上
/// 经缓存重读的 AGENTS.md 段落（无缓存时跳过）与 `<environment>` 块。
/// subagent guidance 由 `effective_turn_system_prompt` 在此之后追加，顺序稳定。
pub(crate) async fn assembled_turn_system_prompt(context: RunAgentTurnContext<'_>) -> String {
    let mut prompt = match context.workspace_instructions {
        Some(cache) => cache.apply(context.system_prompt),
        None => context.system_prompt.to_string(),
    };
    append_prompt_block(
        &mut prompt,
        &environment_context_block(context.workspace_root).await,
    );
    prompt
}

fn append_prompt_block(prompt: &mut String, block: &str) {
    if block.is_empty() {
        return;
    }
    let trimmed_len = prompt.trim_end().len();
    prompt.truncate(trimmed_len);
    if !prompt.is_empty() {
        prompt.push_str("\n\n");
    }
    prompt.push_str(block);
}

pub(crate) async fn environment_context_block(workspace_root: &Path) -> String {
    let mut lines = vec![
        format!("workspace_root: {}", workspace_root.display()),
        format!("os: {}", std::env::consts::OS),
        format!("arch: {}", std::env::consts::ARCH),
        format!("date: {}", utc_today()),
    ];
    if let Some(branch) = current_git_branch(workspace_root).await {
        lines.push(format!("git_branch: {branch}"));
    }
    format!("<environment>\n{}\n</environment>", lines.join("\n"))
}

/// 当前 git 分支；非 git repo、git 缺失、命令失败或超时时静默返回 None。
pub(crate) async fn current_git_branch(workspace_root: &Path) -> Option<String> {
    let output = tokio::time::timeout(
        ENVIRONMENT_GIT_TIMEOUT,
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(workspace_root)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!branch.is_empty()).then_some(branch)
}

fn utc_today() -> String {
    utc_date_string(SystemTime::now())
}

/// SystemTime → UTC 日历日期（YYYY-MM-DD），civil-from-days 算法。
pub(crate) fn utc_date_string(time: SystemTime) -> String {
    let days = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() / 86_400)
        .unwrap_or(0) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u32;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}
