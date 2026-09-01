pub use agent_core::ModelEvent;
use agent_core::{Model, ModelFailure, ModelFuture, ModelRequest, ModelStream};
use agent_protocol::{
    Conversation, Message, ReasoningLevel, ReasoningProfile, ToolCall, ToolDefinition,
};
use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::{FutureExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use thiserror::Error;

mod client;
pub use client::*;
mod sse;
pub use sse::*;
mod types;
pub(crate) use types::*;
#[cfg(test)]
mod tests;
