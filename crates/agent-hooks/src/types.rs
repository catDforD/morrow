use agent_core::FailureMode;
use agent_protocol::MiddlewareAgentScope;
use agent_runtime::MiddlewareRegistry;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

pub const HOOK_CONFIG_SCHEMA_VERSION: u32 = 1;
pub const HOOK_TRUST_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 10;
pub const MIN_HOOK_TIMEOUT_SECS: u64 = 1;
pub const MAX_HOOK_TIMEOUT_SECS: u64 = 300;
pub const MAX_HOOK_STDOUT_BYTES: usize = 256 * 1024;
pub const MAX_HOOK_STDERR_BYTES: usize = 64 * 1024;
pub const MAX_OPERATION_CONTEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    BeforePrompt,
    BeforeTool,
    PermissionRequest,
    AfterTool,
    PreCompact,
    PostCompact,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BeforePrompt => "before_prompt",
            Self::BeforeTool => "before_tool",
            Self::PermissionRequest => "permission_request",
            Self::AfterTool => "after_tool",
            Self::PreCompact => "pre_compact",
            Self::PostCompact => "post_compact",
        }
    }

    pub(crate) fn is_tool_event(self) -> bool {
        matches!(
            self,
            Self::BeforeTool | Self::PermissionRequest | Self::AfterTool
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookFailureMode {
    #[default]
    Open,
    Closed,
}

impl From<HookFailureMode> for FailureMode {
    fn from(value: HookFailureMode) -> Self {
        match value {
            HookFailureMode::Open => Self::Open,
            HookFailureMode::Closed => Self::Closed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookDefinition {
    pub id: String,
    pub event: HookEvent,
    pub command: Vec<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub failure_mode: HookFailureMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_names: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_scopes: Option<Vec<MiddlewareAgentScope>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookConfigSource {
    User,
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookDefinitionStatus {
    pub id: String,
    pub event: HookEvent,
    pub command: Vec<String>,
    pub timeout_secs: u64,
    pub failure_mode: HookFailureMode,
    pub tool_names: Option<Vec<String>>,
    pub agent_scopes: Option<Vec<MiddlewareAgentScope>>,
    pub source: HookConfigSource,
    pub trusted: bool,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookSettings {
    pub schema_version: u32,
    pub user_config_path: String,
    pub project_config_path: String,
    pub trust_store_path: String,
    pub project_fingerprint: Option<String>,
    pub project_trusted: bool,
    pub hooks: Vec<HookDefinitionStatus>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone)]
pub struct HookSnapshot {
    pub(crate) registry: Arc<MiddlewareRegistry>,
    pub(crate) settings: HookSettings,
}

impl std::fmt::Debug for HookSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HookSnapshot")
            .field("registry", &self.registry)
            .field("settings", &self.settings)
            .finish()
    }
}

impl HookSnapshot {
    pub fn registry(&self) -> Arc<MiddlewareRegistry> {
        self.registry.clone()
    }

    pub fn settings(&self) -> &HookSettings {
        &self.settings
    }
}

#[derive(Debug, Error)]
pub enum HookError {
    #[error("home directory was not found")]
    HomeDirNotFound,
    #[error("failed to access hook path {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse hook config {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("hook config {path} uses unsupported schema v{version}")]
    UnsupportedConfigSchema { path: PathBuf, version: u32 },
    #[error("invalid hook config {path}: {message}")]
    InvalidConfig { path: PathBuf, message: String },
    #[error("failed to parse hook trust store {path}: {source}")]
    TrustParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("hook trust store {path} uses unsupported schema v{version}")]
    UnsupportedTrustSchema { path: PathBuf, version: u32 },
    #[error("failed to serialize hook trust store: {0}")]
    TrustSerialize(#[source] serde_json::Error),
    #[error("project hook config was not found")]
    ProjectConfigNotFound,
}

fn default_timeout_secs() -> u64 {
    DEFAULT_HOOK_TIMEOUT_SECS
}
