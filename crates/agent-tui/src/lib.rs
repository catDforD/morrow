//! Morrow's terminal user interface.
//!
//! This crate deliberately depends on a small, strongly typed [`WorkspaceBackend`]
//! boundary instead of the HTTP protocol.  `agent-app` can therefore be used directly
//! by the local CLI while tests and future remote adapters can provide their own backend.

mod backend;
mod completion;
mod input;
mod persistence;
mod run;
mod state;
mod terminal;
mod ui;

pub use backend::*;
pub use completion::{PathCompletion, complete_workspace_paths};
pub use persistence::{TUI_STATE_SCHEMA_VERSION, TuiStateFile, WorkspaceTuiState};
pub use run::{InitialSession, LaunchOptions, RunOutcome, run};
pub use state::{
    AppState, CompletionKind, Effect, InspectorTab, LayoutMode, MainPage, Message, Overlay,
    SettingsSection,
};
pub use terminal::TerminalGuard;
pub use ui::render;

use std::io;

/// Errors returned by the interactive frontend. Backend failures are shown in the UI
/// whenever possible; an error escapes only when startup or the event channel fails.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("无法初始化全屏终端: {0}")]
    Terminal(#[source] io::Error),
    #[error("全屏终端运行失败: {0}")]
    RuntimeTerminal(#[source] io::Error),
    #[error("TUI 后端错误: {0}")]
    Backend(#[from] BackendError),
    #[error("TUI 状态错误: {0}")]
    State(#[source] io::Error),
    #[error("TUI 事件流已关闭")]
    EventStreamClosed,
}
