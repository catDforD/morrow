use super::*;

pub const MAX_SUBAGENT_PROMPT_SUFFIX_CHARS: usize = 4_000;
pub const MIN_SUBAGENT_TIMEOUT_SECS: u64 = 30;
pub const MAX_SUBAGENT_TIMEOUT_SECS: u64 = 1_800;
pub const MIN_SUBAGENT_TOOL_ROUNDS: usize = 1;
pub const MAX_SUBAGENT_TOOL_ROUNDS: usize = 99;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct SubagentIdentity {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentRole {
    Explore,
    Plan,
    Worker,
    Reviewer,
}

impl SubagentRole {
    pub const ALL: [Self; 4] = [Self::Explore, Self::Plan, Self::Worker, Self::Reviewer];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::Plan => "plan",
            Self::Worker => "worker",
            Self::Reviewer => "reviewer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentRoleOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_selection: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt_suffix: String,
    pub timeout_secs: u64,
    pub max_tool_rounds: usize,
}

impl Default for SubagentRoleOverride {
    fn default() -> Self {
        Self {
            model_selection: None,
            prompt_suffix: String::new(),
            timeout_secs: 300,
            max_tool_rounds: 99,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentInstanceStatus {
    Idle,
    Queued,
    Running,
    WaitingApproval,
    Interrupted,
    Failed,
    Cancelled,
}

impl SubagentInstanceStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::WaitingApproval)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentRunStatus {
    Queued,
    Running,
    WaitingApproval,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl SubagentRunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentRunSummary {
    pub instance_id: String,
    pub run_id: String,
    pub role: SubagentRole,
    pub status: SubagentRunStatus,
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub model_calls: usize,
    pub tool_calls: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_changes: Vec<FileChangeSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shell_commands: Vec<ShellCommandSummary>,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentRunRecord {
    pub id: String,
    pub task: String,
    pub status: SubagentRunStatus,
    pub turn_index: usize,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<SubagentRunSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentInstanceSnapshot {
    pub id: String,
    pub role: SubagentRole,
    pub identity: SubagentIdentity,
    pub status: SubagentInstanceStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_summary: Option<SubagentRunSummary>,
    #[serde(default)]
    pub event_log_truncated: bool,
}

pub fn default_subagent_identities() -> Vec<SubagentIdentity> {
    const NAMES: &[&str] = &[
        "后藤一里",
        "山田凉",
        "喜多郁代",
        "伊地知虹夏",
        "中野梓",
        "平泽唯",
        "琴吹䌷",
        "秋山澪",
        "田井中律",
        "井芹仁菜",
        "河原木桃香",
        "安和昴",
        "海老冢智",
        "露帕",
        "高松灯",
        "千早爱音",
        "要乐奈",
        "长崎爽世",
        "椎名立希",
        "丰川祥子",
        "若叶睦",
        "三角初华",
    ];
    NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| SubagentIdentity {
            id: format!("builtin-{:02}", index + 1),
            name: (*name).to_string(),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentExecutionSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    pub task: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub model_calls: usize,
    pub tool_calls: usize,
    pub truncated: bool,
}

impl SubagentExecutionSummary {
    pub fn success(
        task: impl Into<String>,
        result: impl Into<String>,
        model_calls: usize,
        tool_calls: usize,
        truncated: bool,
    ) -> Self {
        Self {
            agent_id: None,
            agent_name: None,
            task: task.into(),
            result: Some(result.into()),
            error: None,
            model_calls,
            tool_calls,
            truncated,
        }
    }

    pub fn failure(
        task: impl Into<String>,
        error: impl Into<String>,
        model_calls: usize,
        tool_calls: usize,
    ) -> Self {
        Self {
            agent_id: None,
            agent_name: None,
            task: task.into(),
            result: None,
            error: Some(error.into()),
            model_calls,
            tool_calls,
            truncated: false,
        }
    }

    pub fn with_agent_name(mut self, agent_name: impl Into<String>) -> Self {
        self.agent_name = Some(agent_name.into());
        self
    }

    pub fn with_agent_identity(mut self, identity: &SubagentIdentity) -> Self {
        self.agent_id = Some(identity.id.clone());
        self.agent_name = Some(identity.name.clone());
        self
    }
}
