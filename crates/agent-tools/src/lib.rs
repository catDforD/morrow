pub mod mcp;
mod web_fetch;

use agent_config::{McpServerConfig, ToolsConfig};
pub use agent_core::{
    CancellationToken, ToolApproval, ToolExecution, ToolExecutionContext, ToolExecutionKind,
    ToolExecutionMode, ToolFuture, ToolResult, ToolRuntime,
};
use agent_protocol::{
    ApprovalDecision, ApprovalRequest, FileChangeOperation, FileChangeSummary, PermissionMode,
    PermissionProfile, ShellCommandSummary, ShellPolicy, SubagentExecutionSummary,
    SubagentIdentity, SubagentInstanceSnapshot, SubagentRole, ToolCall, ToolDefinition,
    ToolExecutionSummary, default_subagent_identities,
};
use agent_sandbox::{PermissionDecision, PermissionEvaluator, PermissionEvaluatorError};
use async_trait::async_trait;
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

pub use mcp::McpToolCache;
pub use web_fetch::WEB_FETCH_TOOL_NAME;
use web_fetch::WebFetchTool;

mod file_tools;
pub use file_tools::*;
mod patch;
pub use patch::*;
mod registry;
pub use registry::*;
mod search;
pub use search::*;
mod shell;
pub use shell::*;
mod subagent;
pub use subagent::*;
#[cfg(test)]
mod tests;
