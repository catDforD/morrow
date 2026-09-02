use super::*;

pub const SESSION_DOCUMENT_SCHEMA_VERSION: u32 = 7;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub summarized_turns: usize,
}

impl SessionContext {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Session {
    pub active_thread: Thread,
    pub turns: Vec<TurnRecord>,
    pub context: SessionContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionApplyError;

impl std::fmt::Display for SessionApplyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("cannot apply a running turn to a session")
    }
}

impl std::error::Error for SessionApplyError {}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_thread(active_thread: Thread) -> Self {
        Self {
            active_thread,
            turns: Vec::new(),
            context: SessionContext::new(),
        }
    }

    /// 一次性记录 turn；只有成功完成的消息才进入下一轮模型上下文。
    pub fn apply_turn(&mut self, record: TurnRecord) {
        self.try_apply_turn(record)
            .expect("only terminal turn records may be applied");
    }

    pub fn try_apply_turn(&mut self, record: TurnRecord) -> Result<(), SessionApplyError> {
        if record.turn.status == TurnStatus::Running {
            return Err(SessionApplyError);
        }
        if record.turn.status == TurnStatus::Completed {
            self.active_thread
                .messages
                .extend(record.messages.iter().cloned());
        }
        self.turns.push(record);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionDocument {
    pub schema_version: u32,
    pub session: Session,
}

impl SessionDocument {
    pub fn new(session: Session) -> Self {
        Self {
            schema_version: SESSION_DOCUMENT_SCHEMA_VERSION,
            session,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    #[default]
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl PermissionMode {
    /// Severity rank: read_only < workspace_write < danger_full_access.
    pub fn severity(self) -> u8 {
        match self {
            Self::ReadOnly => 0,
            Self::WorkspaceWrite => 1,
            Self::DangerFullAccess => 2,
        }
    }

    /// The more restrictive of `self` and `ceiling`.
    pub fn clamp(self, ceiling: Self) -> Self {
        if self.severity() <= ceiling.severity() {
            self
        } else {
            ceiling
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningLevel {
    #[default]
    Off,
    High,
    Max,
}

impl ReasoningLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningProfile {
    #[default]
    None,
    Deepseek,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ModelSelection {
    pub provider_id: String,
    pub model_id: String,
    pub reasoning: ReasoningLevel,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ModelInvocation {
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub model_name: String,
    pub reasoning: ReasoningLevel,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
            Self::DangerFullAccess => "danger_full_access",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellPolicy {
    Deny,
    Prompt,
    Allow,
}

impl ShellPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Prompt => "prompt",
            Self::Allow => "allow",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct PermissionProfile {
    pub mode: PermissionMode,
    pub shell: ShellPolicy,
}

impl PermissionProfile {
    pub fn for_mode(mode: PermissionMode) -> Self {
        Self {
            mode,
            shell: match mode {
                PermissionMode::ReadOnly | PermissionMode::WorkspaceWrite => ShellPolicy::Prompt,
                PermissionMode::DangerFullAccess => ShellPolicy::Allow,
            },
        }
    }
}

impl Default for PermissionProfile {
    fn default() -> Self {
        Self::for_mode(PermissionMode::ReadOnly)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionProjectionDocument {
    pub schema_version: u32,
    pub session: SessionProjection,
}

impl SessionProjectionDocument {
    pub fn new(session: SessionProjection) -> Self {
        Self {
            schema_version: SESSION_DOCUMENT_SCHEMA_VERSION,
            session,
        }
    }
}
