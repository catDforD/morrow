mod adapter;
mod config;
mod manager;
mod process;
mod protocol;
mod trust;
mod types;

pub use manager::HookManager;
pub use types::{
    DEFAULT_HOOK_TIMEOUT_SECS, HOOK_CONFIG_SCHEMA_VERSION, HOOK_TRUST_SCHEMA_VERSION,
    HookConfigSource, HookDefinition, HookDefinitionStatus, HookError, HookEvent, HookFailureMode,
    HookSettings, HookSnapshot, MAX_HOOK_STDERR_BYTES, MAX_HOOK_STDOUT_BYTES,
    MAX_HOOK_TIMEOUT_SECS, MAX_OPERATION_CONTEXT_BYTES, MIN_HOOK_TIMEOUT_SECS,
};

#[cfg(test)]
use adapter::CommandHook;
#[cfg(test)]
use agent_core::{AfterTurnInput, AfterTurnOutput, ContextBlock, MiddlewareExecutionContext};
#[cfg(test)]
use agent_protocol::{MiddlewareAgentScope, MiddlewareSource};
#[cfg(test)]
use agent_runtime::BeforePromptInput;
#[cfg(test)]
use process::run_hook_command;
#[cfg(test)]
use serde_json::{Value, json};
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
#[cfg(test)]
use std::time::Duration;

#[cfg(test)]
mod tests;
