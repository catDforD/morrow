pub mod middleware;
pub mod session_handle;
pub mod session_projection;
pub mod session_store;
pub mod subagent_store;
pub mod subagent_supervisor;

use agent_config::{ContextConfig, McpServerConfig, ModelContextLimits, ToolsConfig};
use agent_core::tokens::{
    apply_request_padding, estimate_message_tokens, estimate_role_text_tokens,
    estimate_tool_definitions_tokens,
};
use agent_core::{
    Agent, AgentError, AgentRunContext, ModelEvent, ModelFailure, ModelRequest,
    ToolExecutionContext,
};
use agent_protocol::{
    AgentEvent, AgentEventOrigin, ApprovalDecision, ApprovalRequest, Conversation, Message,
    MiddlewareAgentScope, ModelInvocation, PermissionMode, PermissionProfile, Session, SessionFact,
    SessionTurnStatus, ShellPolicy, SubagentExecutionSummary, SubagentIdentity, Thread, ToolCall,
    ToolDefinition, TurnRecord, TurnStatus, TurnStepKind,
};
use agent_tools::{BuiltInToolAllowlist, SubagentExecutor, ToolRegistry, ToolRegistryError};
use futures_util::StreamExt;
use futures_util::future::{BoxFuture, FutureExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub use agent_core::{
    CancellationToken, MiddlewareContextBlock, MiddlewareExecutionContext, Model,
};
pub use agent_tools::{McpToolCache, SubagentController};
pub use middleware::{
    BeforePromptInput, CompactionCause, MiddlewareRegistry, PostCompactInput, PreCompactInput,
    RuntimeMiddleware, RuntimeMiddlewareChain,
};
pub use session_handle::{SessionHandle, SessionSubscription};
pub use session_projection::{
    SessionProjectionError, project_session, projection_to_legacy_session,
};
pub use session_store::{
    SessionDirectoryListing, SessionEntry, SessionListingDiagnostic, SessionListingEntry,
    SessionStore, SessionStoreError, SessionWriterLease,
};
pub use subagent_store::{SubagentInstanceDocument, SubagentSessionStore, SubagentStoreError};
pub use subagent_supervisor::{
    DenySubagentObserver, MAX_CONCURRENT_SUBAGENT_RUNS, MAX_PERSISTENT_SUBAGENTS_PER_SESSION,
    SubagentObserver, SubagentRoleRuntime, SubagentSupervisor, build_role_runtimes,
    build_role_runtimes_with_middleware, subagent_store_for_session,
};

mod compaction;
pub use compaction::*;
mod instructions;
pub use instructions::*;
mod middleware_glue;
pub use middleware_glue::*;
mod subagent_support;
pub(crate) use subagent_support::*;
mod system_prompt;
pub use system_prompt::*;
mod turn;
pub use turn::*;
#[cfg(test)]
mod tests;
