mod middleware;
#[doc(hidden)]
pub mod middleware_runner;
pub mod tokens;

pub use middleware::{
    AfterToolInput, AfterTurnInput, AfterTurnOutput, AfterTurnRun, AgentMiddleware,
    AgentMiddlewareChain, BeforeToolInput, ContextBlock, FailureMode, GateDecision, GateOutput,
    GateRun, MiddlewareContextBlock, MiddlewareError, MiddlewareExecutionContext, MiddlewareFuture,
    ObservationOutput, ObservationRun, PermissionDecision, PermissionOutput,
    PermissionRequestInput, PermissionRun,
};

use agent_protocol::{
    AgentEvent, ApprovalDecision, ApprovalRequest, Conversation, Message, ModelInvocation,
    SubagentExecutionSummary, SubagentIdentity, Thread, ToolCall, ToolDefinition,
    ToolExecutionSummary, Turn, TurnRecord, TurnStep,
};
use futures_util::future::{BoxFuture, FutureExt};
use futures_util::stream::{BoxStream, FuturesUnordered, Stream};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::error::Error as StdError;
use std::fmt;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc as Shared, Mutex as StdMutex};
use std::task::{Context, Poll, Waker};
use thiserror::Error;

mod agent;
pub use agent::*;
mod cancellation;
pub use cancellation::*;
mod model;
pub use model::*;
mod tool;
pub use tool::*;
#[cfg(test)]
mod tests;
